//! Process-wide file change observer over one `notify` watcher, plus an in-process fast path.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use thiserror::Error;
use tokio::sync::mpsc;

/// Identity of a watched path. Birth time distinguishes a same-path recreate whose inode
/// number was recycled; without btime the comparison degrades to `(dev, ino)`.
#[cfg(unix)]
pub type WatchIdentity = (u64, u64, Option<std::time::SystemTime>);
#[cfg(not(unix))]
pub type WatchIdentity = ();

pub fn capture_watch_identity(path: &Path) -> std::io::Result<WatchIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = std::fs::metadata(path)?;
        Ok((m.dev(), m.ino(), m.created().ok()))
    }
    #[cfg(not(unix))]
    {
        std::fs::metadata(path)?;
        Ok(())
    }
}

const NOOP_SENTINEL: SubscriptionId = SubscriptionId(0);

#[derive(Debug, Clone)]
pub struct FileEvent {
    pub path: PathBuf,
    pub kind: FileEventKind,
    pub source: EventSource,
}

/// Within a debounce window `Upserted` wins over `Removed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileEventKind {
    Upserted,
    Removed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventSource {
    Kernel,
    Local,
}

#[derive(Debug, Clone)]
pub enum FileMatcher {
    Exact(PathBuf),
    AnyOf(Vec<PathBuf>),
}

#[derive(Debug, Clone)]
pub struct WatchSpec {
    pub dir: PathBuf,
    pub matcher: FileMatcher,
    pub debounce: Option<Duration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchErrorKind {
    Backend,
    ResourceExhausted,
    NotFound,
    Permission,
    DispatcherDead,
    Other,
}

#[derive(Debug, Error)]
pub enum WatchError {
    #[error("file watcher init failed: {message}")]
    Init {
        kind: WatchErrorKind,
        message: String,
        #[source]
        source: Option<notify::Error>,
    },
    #[error("could not watch {dir}: {message}")]
    Watch {
        dir: PathBuf,
        kind: WatchErrorKind,
        message: String,
        #[source]
        source: Option<notify::Error>,
    },
    #[error("file watcher dispatcher has terminated")]
    DispatcherDead,
}

impl WatchError {
    pub fn kind(&self) -> WatchErrorKind {
        match self {
            WatchError::Init { kind, .. } | WatchError::Watch { kind, .. } => *kind,
            WatchError::DispatcherDead => WatchErrorKind::DispatcherDead,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct SubscriptionId(u64);

#[derive(Debug)]
struct DirState {
    refcount: usize,
    /// Re-stat on every subscribe: a mismatch forces a rewatch, since NonRecursive watches do
    /// not follow a same-path recreate. Cleared on rewatch failure.
    installed_identity: Option<WatchIdentity>,
}

struct Subscription {
    spec: WatchSpec,
    sink: mpsc::Sender<FileEvent>,
}

struct DebounceEntry {
    pending: FileEvent,
    fire_at: Instant,
}

struct Inner {
    watcher: Option<RecommendedWatcher>,
    subscriptions: HashMap<SubscriptionId, Subscription>,
    dirs: HashMap<PathBuf, DirState>,
    next_id: u64,
    pending: HashMap<(SubscriptionId, PathBuf), DebounceEntry>,
    slots: BTreeMap<Instant, Vec<(SubscriptionId, PathBuf)>>,
}

enum DispatchMsg {
    Kernel(notify::Result<notify::Event>),
    Local(PathBuf),
    #[cfg(any(test, debug_assertions))]
    Barrier(tokio::sync::oneshot::Sender<()>),
}

pub struct FileWatchService {
    inner: Mutex<Inner>,
    #[cfg(any(test, debug_assertions))]
    kernel_observers: Mutex<HashMap<PathBuf, Vec<tokio::sync::oneshot::Sender<()>>>>,
    dispatcher_dead: AtomicBool,
    tokio_tx: mpsc::UnboundedSender<DispatchMsg>,
    last_kernel_warn_unix_ms: AtomicI64,
    dropped_kernel_err_count: AtomicU64,
}

pub struct SubscriptionHandle {
    id: SubscriptionId,
    service: Weak<FileWatchService>,
}

impl std::fmt::Debug for FileWatchService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileWatchService")
            .field(
                "dispatcher_dead",
                &self.dispatcher_dead.load(Ordering::Acquire),
            )
            .finish_non_exhaustive()
    }
}

impl FileWatchService {
    fn build(
        watcher: Option<RecommendedWatcher>,
        tokio_tx: mpsc::UnboundedSender<DispatchMsg>,
    ) -> Arc<Self> {
        Arc::new(FileWatchService {
            dispatcher_dead: AtomicBool::new(watcher.is_none()),
            inner: Mutex::new(Inner {
                watcher,
                subscriptions: HashMap::new(),
                dirs: HashMap::new(),
                next_id: 1,
                pending: HashMap::new(),
                slots: BTreeMap::new(),
            }),
            #[cfg(any(test, debug_assertions))]
            kernel_observers: Mutex::new(HashMap::new()),
            tokio_tx,
            last_kernel_warn_unix_ms: AtomicI64::new(0),
            dropped_kernel_err_count: AtomicU64::new(0),
        })
    }

    /// Falls back to `noop()` when the backend fails or `AOE_FILE_WATCH=off`.
    pub fn new() -> Result<Arc<Self>, WatchError> {
        if std::env::var("AOE_FILE_WATCH").as_deref() == Ok("off") {
            tracing::info!(
                target: "file_watch.service",
                noop = true,
                reason = "AOE_FILE_WATCH=off",
                "file watch service running in noop mode"
            );
            return Ok(Self::noop());
        }
        let (notify_tx, notify_rx) = std::sync::mpsc::channel();
        let watcher = match notify::recommended_watcher(move |res| {
            let _ = notify_tx.send(res);
        }) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(
                    target: "file_watch.service",
                    error = %e,
                    "notify init failed; degrading to noop"
                );
                return Ok(Self::noop());
            }
        };

        let (tokio_tx, tokio_rx) = mpsc::unbounded_channel::<DispatchMsg>();
        let svc = Self::build(Some(watcher), tokio_tx.clone());

        // The drain thread holds the only kernel sender and a `Weak` service, so dropping the
        // last `Arc` shuts it down.
        let svc_weak_for_drain = Arc::downgrade(&svc);
        std::thread::Builder::new()
            .name("file_watch_drain".into())
            .spawn(move || loop {
                let (reason, err) = match notify_rx.recv() {
                    Ok(res) => {
                        if tokio_tx.send(DispatchMsg::Kernel(res)).is_ok() {
                            continue;
                        }
                        ("dispatcher_channel_closed", None)
                    }
                    Err(e) => ("notify_channel_closed", Some(e)),
                };
                if let Some(svc) = svc_weak_for_drain.upgrade() {
                    log_dispatcher_dead_once(
                        &svc,
                        reason,
                        err.as_ref().map(|e| e as &dyn std::fmt::Display),
                    );
                }
                return;
            })
            .map_err(|e| WatchError::Init {
                kind: WatchErrorKind::Other,
                message: format!("could not spawn file_watch_drain thread: {e}"),
                source: None,
            })?;

        crate::task_util::spawn_supervised(
            "file_watch.dispatcher",
            crate::task_util::PanicPolicy::Log,
            dispatcher_loop(Arc::downgrade(&svc), tokio_rx),
        );

        Ok(svc)
    }

    pub fn noop() -> Arc<Self> {
        let (tokio_tx, _) = mpsc::unbounded_channel::<DispatchMsg>();
        Self::build(None, tokio_tx)
    }

    pub(crate) fn notify_local_change(&self, path: &Path) {
        if self.dispatcher_dead.load(Ordering::Acquire) {
            return;
        }
        // Canonicalize so the debounce key matches the kernel echo (`/private/var/...` on macOS).
        let final_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
        if self.tokio_tx.send(DispatchMsg::Local(final_path)).is_err() {
            log_dispatcher_dead_once(self, "local_send_failed", Some(&"channel closed"));
        }
    }

    /// Dropping the receiver does not unsubscribe; drop the handle.
    pub fn subscribe_channel(
        self: &Arc<Self>,
        mut spec: WatchSpec,
        capacity: usize,
    ) -> Result<(mpsc::Receiver<FileEvent>, SubscriptionHandle), WatchError> {
        let mut inner = self.inner.lock().expect("file_watch inner mutex poisoned");
        let cap = capacity.max(1);
        let handle = |id| SubscriptionHandle {
            id,
            service: Arc::downgrade(self),
        };
        if inner.watcher.is_none() {
            let (_tx, rx) = mpsc::channel::<FileEvent>(cap);
            return Ok((rx, handle(NOOP_SENTINEL)));
        }
        // A dead dispatcher would accept subscriptions that never receive events.
        if self.dispatcher_dead.load(Ordering::Acquire) {
            return Err(WatchError::DispatcherDead);
        }
        let dir = std::fs::canonicalize(&spec.dir).map_err(|e| WatchError::Watch {
            dir: spec.dir.clone(),
            kind: classify_io_err_kind(e.kind()),
            message: format!("canonicalize failed: {e}"),
            source: None,
        })?;
        spec.dir = dir.clone();
        let current_identity = capture_watch_identity(&dir).ok();
        let drift_against_existing = inner.dirs.get(&dir).is_some_and(|state| {
            match (current_identity, state.installed_identity) {
                (Some(curr), Some(stored)) => stored != curr,
                (None, stored) => stored.is_some(),
                (Some(_), None) => false,
            }
        });
        let state = inner.dirs.entry(dir.clone()).or_insert(DirState {
            refcount: 0,
            installed_identity: None,
        });
        let pre_bump = state.refcount;
        state.refcount += 1;
        let needs_install = pre_bump == 0;
        if needs_install || drift_against_existing {
            // Hold the lock across the watch call so no subscriber observes a mid-rollback refcount.
            let watcher = inner.watcher.as_mut().expect("live service has a watcher");
            if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
                if needs_install {
                    inner.dirs.remove(&dir);
                } else {
                    let state = inner.dirs.get_mut(&dir).expect("entry exists");
                    state.refcount = pre_bump;
                    state.installed_identity = None;
                }
                return Err(WatchError::Watch {
                    dir,
                    kind: classify_notify_err(&e),
                    message: format!("notify watch failed: {e}"),
                    source: Some(e),
                });
            }
            if let Some(curr) = current_identity {
                inner
                    .dirs
                    .get_mut(&dir)
                    .expect("entry exists")
                    .installed_identity = Some(curr);
            }
        }

        let (tx, rx) = mpsc::channel::<FileEvent>(cap);
        let id = SubscriptionId(inner.next_id);
        inner.next_id = inner.next_id.checked_add(1).unwrap_or(1);
        inner
            .subscriptions
            .insert(id, Subscription { spec, sink: tx });
        Ok((rx, handle(id)))
    }

    pub fn subscriber_count(&self) -> usize {
        self.inner
            .lock()
            .map(|inner| inner.subscriptions.len())
            .unwrap_or(0)
    }
}

#[cfg(any(test, debug_assertions))]
#[doc(hidden)]
pub mod test_support {
    use super::{Arc, DispatchMsg, FileWatchService, Path, WatchError};

    pub async fn dispatch_barrier(svc: &FileWatchService) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        assert!(svc.tokio_tx.send(DispatchMsg::Barrier(tx)).is_ok());
        rx.await.expect("live dispatcher acknowledges barrier");
    }

    pub fn observe_kernel_path(
        svc: &FileWatchService,
        path: &Path,
    ) -> tokio::sync::oneshot::Receiver<()> {
        assert!(!svc.dispatcher_dead.load(super::Ordering::Acquire));
        let path = std::fs::canonicalize(path.parent().expect("parent"))
            .expect("existing parent")
            .join(path.file_name().expect("filename"));
        let (tx, rx) = tokio::sync::oneshot::channel();
        svc.kernel_observers
            .lock()
            .unwrap()
            .entry(path)
            .or_default()
            .push(tx);
        rx
    }

    pub fn new_filewatch() -> Result<Arc<FileWatchService>, WatchError> {
        FileWatchService::new()
    }

    pub fn noop_filewatch() -> Arc<FileWatchService> {
        FileWatchService::noop()
    }
}

fn classify_notify_err(e: &notify::Error) -> WatchErrorKind {
    use notify::ErrorKind;
    match e.kind {
        ErrorKind::Io(ref io) => classify_io_err_kind(io.kind()),
        ErrorKind::PathNotFound => WatchErrorKind::NotFound,
        ErrorKind::MaxFilesWatch => WatchErrorKind::ResourceExhausted,
        _ => WatchErrorKind::Other,
    }
}

fn classify_io_err_kind(k: std::io::ErrorKind) -> WatchErrorKind {
    match k {
        std::io::ErrorKind::NotFound => WatchErrorKind::NotFound,
        std::io::ErrorKind::PermissionDenied => WatchErrorKind::Permission,
        _ => WatchErrorKind::Backend,
    }
}

fn log_dispatcher_dead_once(
    svc: &FileWatchService,
    reason: &'static str,
    err: Option<&dyn std::fmt::Display>,
) {
    if !svc.dispatcher_dead.swap(true, Ordering::AcqRel) {
        tracing::error!(
            target: "file_watch.service",
            reason,
            error = err.map(tracing::field::display),
            subscribers_affected = svc.subscriber_count(),
            "file watch dispatcher exiting; live propagation disabled, polling fallback canonical"
        );
    }
}

/// Covers `.tmp*` tempfiles, `*.tmp` rename-based writes, and editor temp files.
fn is_tempfile(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| {
            name.starts_with(".tmp")
                || name.ends_with(".tmp")
                || name.starts_with('~')
                || name.starts_with(".#")
        })
}

fn is_lockfile(path: &Path) -> bool {
    path.file_name()
        .and_then(OsStr::to_str)
        .and_then(|name| name.strip_prefix('.')?.strip_suffix(".lock"))
        .is_some_and(|stem| {
            !stem.is_empty()
                && stem
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_' || c == '-')
        })
}

fn matcher_matches(spec: &WatchSpec, path: &Path) -> bool {
    // Compare by file name: kernel paths are canonical on macOS while matcher paths may not be.
    let Some(name) = path.file_name() else {
        return false;
    };
    match &spec.matcher {
        FileMatcher::Exact(p) => p.file_name() == Some(name),
        FileMatcher::AnyOf(ps) => ps.iter().any(|p| p.file_name() == Some(name)),
    }
}

fn classify_event_kind(ev: &notify::Event) -> Option<FileEventKind> {
    use notify::event::{EventKind, ModifyKind};
    // Metadata-only changes are skipped; `Any` stays because PollWatcher and kqueue emit it for writes.
    match ev.kind {
        EventKind::Create(_)
        | EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Name(_) | ModifyKind::Any) => {
            Some(FileEventKind::Upserted)
        }
        EventKind::Remove(_) => Some(FileEventKind::Removed),
        _ => None,
    }
}

async fn dispatcher_loop(
    svc: Weak<FileWatchService>,
    mut rx: mpsc::UnboundedReceiver<DispatchMsg>,
) {
    // Flips `dispatcher_dead` on every exit path of `run_dispatcher`, including panic.
    struct ExitLatch<'a> {
        svc: &'a Weak<FileWatchService>,
    }
    impl Drop for ExitLatch<'_> {
        fn drop(&mut self) {
            if let Some(arc) = self.svc.upgrade() {
                log_dispatcher_dead_once(&arc, "dispatcher_loop_exit", None);
            }
        }
    }
    let _guard = ExitLatch { svc: &svc };
    let exit_reason = run_dispatcher(svc.clone(), &mut rx).await;
    if let Some(arc) = svc.upgrade() {
        log_dispatcher_dead_once(&arc, exit_reason, None);
    }
}

async fn run_dispatcher(
    svc: Weak<FileWatchService>,
    rx: &mut mpsc::UnboundedReceiver<DispatchMsg>,
) -> &'static str {
    loop {
        let Some(arc) = svc.upgrade() else {
            return "service_dropped";
        };
        let next_fire = arc
            .inner
            .lock()
            .expect("file_watch inner mutex poisoned")
            .slots
            .keys()
            .next()
            .copied();
        drop(arc);
        tokio::select! {
            biased;
            msg = rx.recv() => {
                let Some(msg) = msg else { return "channel_closed" };
                let Some(arc) = svc.upgrade() else { return "service_dropped" };
                match msg {
                    DispatchMsg::Kernel(res) => handle_kernel(&arc, res),
                    DispatchMsg::Local(path) => {
                        dispatch_path(&arc, &path, FileEventKind::Upserted, EventSource::Local);
                    }
                    #[cfg(any(test, debug_assertions))]
                    DispatchMsg::Barrier(tx) => {
                        let _ = tx.send(());
                    }
                }
            }
            _ = sleep_until_optional(next_fire) => {
                let Some(arc) = svc.upgrade() else { return "service_dropped" };
                fire_due(&arc);
            }
        }
    }
}

async fn sleep_until_optional(deadline: Option<Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(tokio::time::Instant::from_std(d)).await,
        None => std::future::pending::<()>().await,
    }
}

fn handle_kernel(svc: &Arc<FileWatchService>, res: notify::Result<notify::Event>) {
    let ev = match res {
        Ok(ev) => ev,
        Err(e) => {
            let now_ms = crate::util::now_ms() as i64;
            let last = svc.last_kernel_warn_unix_ms.load(Ordering::Acquire);
            if now_ms.saturating_sub(last) >= 1_000 {
                svc.last_kernel_warn_unix_ms
                    .store(now_ms, Ordering::Release);
                let dropped = svc.dropped_kernel_err_count.swap(0, Ordering::AcqRel);
                tracing::warn!(
                    target: "file_watch.service",
                    error = %e,
                    dropped_since_last = dropped,
                    "kernel watcher emitted error; live propagation may degrade until next valid event"
                );
            } else {
                svc.dropped_kernel_err_count.fetch_add(1, Ordering::AcqRel);
            }
            return;
        }
    };
    let Some(kind) = classify_event_kind(&ev) else {
        return;
    };
    for path in &ev.paths {
        dispatch_path(svc, path, kind, EventSource::Kernel);
        #[cfg(any(test, debug_assertions))]
        if let Some(observers) = svc.kernel_observers.lock().unwrap().remove(path) {
            for observer in observers {
                let _ = observer.send(());
            }
        }
    }
}

fn dispatch_path(
    svc: &Arc<FileWatchService>,
    path: &Path,
    kind: FileEventKind,
    source: EventSource,
) {
    if is_tempfile(path) || is_lockfile(path) {
        return;
    }
    let matched: Vec<(SubscriptionId, Option<Duration>, mpsc::Sender<FileEvent>)> = {
        let inner = svc.inner.lock().expect("file_watch inner mutex poisoned");
        inner
            .subscriptions
            .iter()
            .filter(|(_, sub)| path.starts_with(&sub.spec.dir) && matcher_matches(&sub.spec, path))
            .map(|(id, sub)| (*id, sub.spec.debounce, sub.sink.clone()))
            .collect()
    };
    for (id, debounce, sink) in matched {
        let event = FileEvent {
            path: path.to_path_buf(),
            kind,
            source,
        };
        match debounce {
            None => deliver(&sink, event, id),
            Some(window) => arm_debounce(svc, id, event, window),
        }
    }
}

fn deliver(sink: &mpsc::Sender<FileEvent>, ev: FileEvent, id: SubscriptionId) {
    match sink.try_send(ev) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(dropped)) => tracing::debug!(
            target: "file_watch.subscriber",
            subscriber_id = id.0,
            path = %dropped.path.display(),
            "dropping file event: subscriber channel full"
        ),
        Err(mpsc::error::TrySendError::Closed(_)) => tracing::debug!(
            target: "file_watch.subscriber",
            subscriber_id = id.0,
            "dropping file event: subscriber receiver closed"
        ),
    }
}

/// Older slots become stale and self-evict via the `fire_at` check.
fn arm_debounce(svc: &Arc<FileWatchService>, id: SubscriptionId, ev: FileEvent, window: Duration) {
    let fire_at = Instant::now() + window;
    let key = (id, ev.path.clone());
    let mut inner = svc.inner.lock().expect("file_watch inner mutex poisoned");
    let entry = inner
        .pending
        .entry(key.clone())
        .or_insert_with(|| DebounceEntry {
            pending: ev.clone(),
            fire_at,
        });
    if entry.pending.kind == FileEventKind::Removed && ev.kind == FileEventKind::Upserted {
        entry.pending = ev;
    }
    entry.fire_at = fire_at;
    inner.slots.entry(fire_at).or_default().push(key);
}

fn fire_due(svc: &Arc<FileWatchService>) {
    fire_due_at(svc, Instant::now());
}

fn fire_due_at(svc: &Arc<FileWatchService>, now: Instant) {
    let mut to_deliver = Vec::new();
    {
        let mut inner = svc.inner.lock().expect("file_watch inner mutex poisoned");
        let due_keys: Vec<Instant> = inner.slots.range(..=now).map(|(k, _)| *k).collect();
        for slot_at in due_keys {
            for key in inner.slots.remove(&slot_at).unwrap_or_default() {
                if inner.pending.get(&key).map(|entry| entry.fire_at) != Some(slot_at) {
                    continue;
                }
                let entry = inner.pending.remove(&key).expect("checked above");
                if let Some(sub) = inner.subscriptions.get(&key.0) {
                    to_deliver.push((key.0, entry.pending, sub.sink.clone()));
                }
            }
        }
    }
    for (id, ev, sink) in to_deliver {
        deliver(&sink, ev, id);
    }
}

impl Drop for SubscriptionHandle {
    fn drop(&mut self) {
        if self.id == NOOP_SENTINEL {
            return;
        }
        let Some(svc) = self.service.upgrade() else {
            return;
        };
        // Tolerate poison: a second panic in a destructor would abort.
        let mut inner = svc.inner.lock().unwrap_or_else(|p| p.into_inner());
        let Some(sub) = inner.subscriptions.remove(&self.id) else {
            return;
        };
        let dir = sub.spec.dir;
        if let Some(state) = inner.dirs.get_mut(&dir) {
            state.refcount = state.refcount.saturating_sub(1);
            if state.refcount == 0 {
                inner.dirs.remove(&dir);
                if let Some(w) = inner.watcher.as_mut() {
                    let _ = w.unwatch(&dir);
                }
            }
        }
        inner.pending.retain(|(sid, _), _| *sid != self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;
    use tokio::time::timeout;

    fn write_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, contents).expect("write");
        p
    }

    fn spec(dir: &Path, matcher: FileMatcher) -> WatchSpec {
        WatchSpec {
            dir: dir.to_path_buf(),
            matcher,
            debounce: None,
        }
    }

    fn exact(dir: &Path, name: &str) -> WatchSpec {
        spec(dir, FileMatcher::Exact(dir.join(name)))
    }

    fn assert_empty(rx: &mut mpsc::Receiver<FileEvent>) {
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    /// macOS FSEvents can take ~1.5s to forward small writes.
    const KERNEL_WAIT: Duration = Duration::from_millis(2_500);
    const NEG_WAIT: Duration = Duration::from_millis(300);

    // Environment-disabled construction lives in tests/filewatch_degradation.rs so its
    // process-wide switch cannot disable concurrent live-watch tests.

    #[tokio::test]
    #[serial(file_watch)]
    async fn subscribe_channel_demuxes_and_filters_real_writes() {
        let dir = TempDir::new().unwrap();
        let svc = FileWatchService::new().expect("init");
        let (mut rx_a, _ha) = svc.subscribe_channel(exact(dir.path(), "a"), 8).unwrap();
        let (mut rx_b, _hb) = svc.subscribe_channel(exact(dir.path(), "b"), 8).unwrap();
        write_file(dir.path(), "a", "x");
        let ev = timeout(KERNEL_WAIT, rx_a.recv())
            .await
            .expect("kernel event arrives within budget")
            .expect("channel open");
        assert_eq!(ev.path.file_name(), Some(OsStr::new("a")));
        assert_eq!(ev.source, EventSource::Kernel);
        let tmp_path = dir.path().join("runtime_filter.tmp");
        let matcher = FileMatcher::AnyOf(vec![dir.path().join("runtime_filter"), tmp_path.clone()]);
        let (mut rx_filtered, _hf) = svc.subscribe_channel(spec(dir.path(), matcher), 8).unwrap();
        for name in ["runtime_filter.tmp", "something-else"] {
            let processed = test_support::observe_kernel_path(&svc, &dir.path().join(name));
            write_file(dir.path(), name, "x");
            timeout(KERNEL_WAIT, processed)
                .await
                .expect("native event processed")
                .expect("observer remains live");
        }
        assert!(
            timeout(NEG_WAIT, rx_filtered.recv()).await.is_err(),
            "tempfile and unmatched events must be filtered"
        );
        assert!(
            rx_b.try_recv().is_err(),
            "b subscription must not see a's event"
        );
    }

    #[tokio::test]
    #[serial(file_watch)]
    async fn subscribe_channel_capacity_drops_on_full() {
        let dir = TempDir::new().unwrap();
        let svc = FileWatchService::new().expect("init");
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let paths = ["first", "dropped", "after-drain"].map(|name| canonical.join(name));
        let (mut rx, _h) = svc
            .subscribe_channel(spec(dir.path(), FileMatcher::AnyOf(paths.to_vec())), 1)
            .unwrap();
        svc.notify_local_change(&paths[0]);
        timeout(KERNEL_WAIT, test_support::dispatch_barrier(&svc))
            .await
            .expect("first dispatch fills channel");
        svc.notify_local_change(&paths[1]);
        timeout(KERNEL_WAIT, test_support::dispatch_barrier(&svc))
            .await
            .expect("full channel must not block dispatcher");
        assert_eq!(rx.try_recv().expect("first event retained").path, paths[0]);
        assert_empty(&mut rx);

        svc.notify_local_change(&paths[2]);
        timeout(KERNEL_WAIT, test_support::dispatch_barrier(&svc))
            .await
            .expect("dispatcher remains usable after drop");
        assert_eq!(rx.try_recv().expect("delivery resumes").path, paths[2]);
        assert_empty(&mut rx);
    }

    #[tokio::test]
    #[serial(file_watch)]
    async fn subscription_handle_drop_refcounts_the_dir_watch() {
        let dir = TempDir::new().unwrap();
        let svc = FileWatchService::new().expect("init");
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let refcount = || {
            let inner = svc.inner.lock().unwrap();
            inner.dirs.get(&canonical).map(|d| d.refcount)
        };
        let (mut rx_a, ha) = svc.subscribe_channel(exact(dir.path(), "a"), 4).unwrap();
        let (_rx_b, hb) = svc.subscribe_channel(exact(dir.path(), "b"), 4).unwrap();
        assert_eq!(refcount(), Some(2));
        drop(ha);
        assert!(timeout(KERNEL_WAIT, rx_a.recv())
            .await
            .expect("dropping subscription closes its sender")
            .is_none());
        assert_eq!(refcount(), Some(1));
        drop(hb);
        assert_eq!(refcount(), None);
    }

    #[tokio::test]
    async fn noop_service_is_silent() {
        let svc = FileWatchService::noop();
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let writer_buf = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || TestBufWriter {
                buf: writer_buf.clone(),
            })
            .with_ansi(false)
            .finish();
        let (mut rx, h) = svc
            .subscribe_channel(exact(Path::new("/nowhere"), "x"), 4)
            .expect("noop subscribe");
        tracing::subscriber::with_default(subscriber, || {
            svc.notify_local_change(Path::new("/nowhere/x"));
        });
        assert!(matches!(timeout(NEG_WAIT, rx.recv()).await, Ok(None)));
        drop(h);
        let captured = buf.lock().unwrap();
        assert!(
            captured.is_empty(),
            "noop notify_local_change must emit no log lines, got: {}",
            String::from_utf8_lossy(&captured)
        );
    }

    struct TestBufWriter {
        buf: Arc<Mutex<Vec<u8>>>,
    }

    impl std::io::Write for TestBufWriter {
        fn write(&mut self, src: &[u8]) -> std::io::Result<usize> {
            self.buf.lock().unwrap().extend_from_slice(src);
            Ok(src.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    #[serial(file_watch)]
    async fn debounce_collapses_burst_to_one_event() {
        let dir = TempDir::new().unwrap();
        let svc = FileWatchService::new().expect("init");
        let target = std::fs::canonicalize(dir.path()).unwrap().join("debounced");
        let (mut rx, _h) = svc
            .subscribe_channel(
                WatchSpec {
                    debounce: Some(Duration::from_millis(75)),
                    ..spec(dir.path(), FileMatcher::Exact(target.clone()))
                },
                32,
            )
            .expect("subscribe");
        // No await: this current-thread dispatcher cannot fire between inputs.
        use notify::event::{
            CreateKind, DataChange, EventKind, MetadataKind, ModifyKind, RenameMode,
        };
        for i in 0..10 {
            let kind = match i % 3 {
                0 => EventKind::Create(CreateKind::File),
                1 => EventKind::Modify(ModifyKind::Data(DataChange::Content)),
                _ => EventKind::Modify(ModifyKind::Name(RenameMode::To)),
            };
            handle_kernel(&svc, Ok(notify::Event::new(kind).add_path(target.clone())));
        }
        let deadline = *svc.inner.lock().unwrap().slots.keys().next_back().unwrap();
        fire_due_at(&svc, deadline - Duration::from_nanos(1));
        assert_empty(&mut rx);
        fire_due_at(&svc, deadline);
        let first = rx.try_recv().expect("one event at trailing-edge deadline");
        assert_eq!(first.path, target);
        assert_eq!(first.source, EventSource::Kernel);
        assert_eq!(first.kind, FileEventKind::Upserted);
        fire_due_at(&svc, deadline + Duration::from_secs(1));
        assert_empty(&mut rx);

        handle_kernel(
            &svc,
            Ok(notify::Event::new(EventKind::Modify(ModifyKind::Metadata(
                MetadataKind::Permissions,
            )))
            .add_path(target)),
        );
        fire_due_at(&svc, Instant::now() + Duration::from_millis(75));
        assert_empty(&mut rx);
    }

    #[test]
    fn debounce_per_subscription_per_path_independent() {
        let svc = FileWatchService::noop();
        let dir = PathBuf::from("/watched");
        let paths = [dir.join("a"), dir.join("b")];
        let receivers = [SubscriptionId(1), SubscriptionId(2)].map(|id| {
            let (tx, rx) = mpsc::channel(2);
            let spec = WatchSpec {
                debounce: Some(Duration::ZERO),
                ..spec(&dir, FileMatcher::AnyOf(paths.to_vec()))
            };
            svc.inner
                .lock()
                .unwrap()
                .subscriptions
                .insert(id, Subscription { spec, sink: tx });
            rx
        });

        for path in &paths {
            dispatch_path(&svc, path, FileEventKind::Upserted, EventSource::Local);
        }
        fire_due(&svc);

        for mut rx in receivers {
            let mut received = [
                rx.try_recv().expect("first path delivered").path,
                rx.try_recv().expect("second path delivered").path,
            ];
            received.sort();
            assert_eq!(received, paths);
        }
    }

    #[tokio::test]
    async fn dispatcher_dead_latch_is_one_way() {
        let svc = FileWatchService::new().expect("init");
        let path = TempDir::new().unwrap().path().join("dead-latch");
        log_dispatcher_dead_once(&svc, "first", None);
        assert!(svc.dispatcher_dead.load(Ordering::Acquire));
        log_dispatcher_dead_once(&svc, "second", Some(&"boom"));
        svc.notify_local_change(&path);
        assert!(svc.dispatcher_dead.load(Ordering::Acquire));
    }

    #[tokio::test]
    #[serial(file_watch)]
    async fn subscribe_channel_returns_err_on_watch_failure() {
        let svc = FileWatchService::new().expect("init");
        let bogus = PathBuf::from("/this/path/does/not/exist/file_watch_test");
        match svc.subscribe_channel(exact(&bogus, "x"), 4) {
            Err(WatchError::Watch { dir, .. }) => assert_eq!(dir, bogus),
            Ok(_) => panic!("watching a non-existent path should fail"),
            Err(other) => panic!("expected Watch error, got {other:?}"),
        }
        let dir = TempDir::new().unwrap();
        let _sub = svc
            .subscribe_channel(exact(dir.path(), "y"), 4)
            .expect("sibling subscribe must succeed");
        assert!(!svc.inner.lock().unwrap().dirs.contains_key(&bogus));
    }

    #[tokio::test]
    #[serial(file_watch)]
    async fn notify_local_change_delivers_local_first_under_debounce() {
        let dir = TempDir::new().unwrap();
        let svc = FileWatchService::new().expect("init");
        let target = write_file(dir.path(), "local-coalesce", "seed");
        let (mut rx, _h) = svc
            .subscribe_channel(
                WatchSpec {
                    debounce: Some(Duration::from_millis(75)),
                    ..exact(dir.path(), "local-coalesce")
                },
                8,
            )
            .expect("subscribe");
        svc.notify_local_change(&target);
        std::fs::write(&target, "mutated").expect("mutate");
        let first = timeout(KERNEL_WAIT, rx.recv())
            .await
            .expect("debounced delivery")
            .expect("channel open");
        assert_eq!(first.path.file_name(), target.file_name());
        assert_eq!(first.source, EventSource::Local);
    }

    #[tokio::test]
    #[serial(file_watch)]
    async fn notify_local_change_delivers_only_to_matching_subscribers() {
        let dir = TempDir::new().unwrap();
        let svc = FileWatchService::new().expect("init");
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let watched = canonical.join("watched");
        let (mut rx, _h) = svc
            .subscribe_channel(exact(dir.path(), "watched"), 4)
            .unwrap();
        svc.notify_local_change(&watched);
        timeout(KERNEL_WAIT, test_support::dispatch_barrier(&svc))
            .await
            .expect("matching Local publish processed");
        let delivered = rx.try_recv().expect("matching publish delivered");
        assert_eq!(delivered.path, watched);
        assert_eq!(delivered.source, EventSource::Local);

        svc.notify_local_change(&canonical.join("missed"));
        timeout(KERNEL_WAIT, test_support::dispatch_barrier(&svc))
            .await
            .expect("unmatched Local publish processed");
        assert_empty(&mut rx);
        assert!(!svc.dispatcher_dead.load(Ordering::Acquire));
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial(file_watch)]
    async fn subscribe_rewatches_when_inode_changed_with_refcount_above_zero() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("watched");
        std::fs::create_dir_all(&dir).unwrap();
        let svc = FileWatchService::new().expect("init");
        let canonical = std::fs::canonicalize(&dir).unwrap();
        let installed = |expected_refcount| {
            let inner = svc.inner.lock().unwrap();
            let state = inner.dirs.get(&canonical).expect("entry exists");
            assert_eq!(state.refcount, expected_refcount);
            state.installed_identity.expect("identity recorded")
        };

        let (_rx_keepalive, _h_keepalive) = svc.subscribe_channel(exact(&dir, "file"), 4).unwrap();
        let identity_before = installed(1);

        // Retain the old inode so reuse cannot erase the identity stimulus.
        std::fs::rename(&dir, root.path().join("retired")).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        assert_ne!(identity_before, capture_watch_identity(&dir).unwrap());

        let (mut rx, _h2) = svc
            .subscribe_channel(exact(&dir, "file"), 4)
            .expect("second subscribe re-arms watch on inode drift");
        assert_ne!(identity_before, installed(2));

        let target = write_file(&dir, "file", "payload");
        let evt = timeout(KERNEL_WAIT, rx.recv())
            .await
            .expect("event arrives within budget after watch re-arm")
            .expect("channel open");
        assert_eq!(evt.path, std::fs::canonicalize(&target).unwrap());
    }
}
