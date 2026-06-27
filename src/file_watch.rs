//! Process-wide primitive for observing file content changes via one
//! `notify::RecommendedWatcher`. Constructed in `main()` (or in a per
//! subprocess entry point) and threaded through consumers as `Arc<Self>`.
//!
//! Kernel-driven delivery via `notify` plus an in-process Local fast path
//! (`notify_local_change`, `DispatchMsg::Local`, dispatcher Local arm). The
//! Local path lets a writer in the same process surface its own change
//! immediately; the kernel echo arrives ~ms later for the same atomic
//! rename and collapses into the same per-key debounce slot.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use thiserror::Error;
use tokio::sync::mpsc;

/// Filesystem identity recorded at watch-install time, used to detect
/// peer-driven `rm -rf X && mkdir X` of the same canonical path. The
/// canonical path string survives that race (the new dir resolves to
/// the same string), and on ext4/overlayfs the freed inode number is
/// routinely recycled by the immediate recreate, so `(dev, ino)` alone
/// is insufficient; the identity also carries the birth time
/// (`fs::Metadata::created`, statx btime on Linux), which a recycled
/// inode cannot reproduce. On filesystems without btime the third
/// component is `None` on both sides and the comparison degrades to
/// `(dev, ino)`, leaving the residual hazard only where no-btime and
/// inode recycling coincide. On non-Unix platforms `WatchIdentity` is
/// `()`, so identity comparisons trivially match and the watch-install
/// logic falls back to canonical-path probes alone; Windows still
/// carries the original same-name recreate hazard since
/// `ReadDirectoryChangesW` keys watches by `HANDLE` rather than
/// `(dev, ino)`. Tracking that gap separately.
///
/// Storage convention: the primitive (`DirState`) wraps this in
/// `Option<WatchIdentity>` because the entry is inserted with
/// `refcount=0` before `watcher.watch` runs, so `None` represents the
/// pre-install transient state. Consumers (e.g. `DiskWatchEntry` in
/// `tui/home/mod.rs`) only construct their entries after a successful
/// `subscribe_channel`, so they store a bare `WatchIdentity` and use
/// `unwrap_or_default()` to record `(0, 0, None)` / `()` when the
/// install-time stat fails; that sentinel self-heals on the next
/// rewire because a real `(dev, ino)` will mismatch and force an
/// entry rebuild.
#[cfg(unix)]
pub type WatchIdentity = (u64, u64, Option<std::time::SystemTime>);
/// See [`WatchIdentity`] (Unix variant).
#[cfg(not(unix))]
pub type WatchIdentity = ();

/// Read the filesystem identity of `path` for watch-invalidation
/// comparisons. On Unix returns `(dev, ino, btime)` from
/// `fs::metadata`, with `btime` as `None` where the filesystem does
/// not report a creation time; on other platforms `Ok(())` indicates
/// the path exists (the metadata call is used only to surface a
/// missing path as `Err`; no metadata fields are read). Errors
/// propagate so callers can treat stat-failed as "identity unknown"
/// rather than synthesizing a value.
///
/// # Errors
///
/// Returns `Err` if `fs::metadata(path)` fails (path missing,
/// permission denied, or any other I/O failure). Callers that treat
/// stat-failed as "identity unknown" should call `.ok()` at the use
/// site.
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

/// Sentinel id for handles produced by the noop service. Live services
/// allocate ids starting at 1.
const NOOP_SENTINEL: SubscriptionId = SubscriptionId(0);

/// One file event. Cloned cheaply (path is reference-counted under the hood
/// via `PathBuf`).
#[derive(Debug, Clone)]
pub struct FileEvent {
    /// Absolute path the event refers to.
    pub path: PathBuf,
    /// Coarse classification of the kernel event.
    pub kind: FileEventKind,
    /// Origin tag of the file event.
    pub source: EventSource,
}

/// Coarse event classification. `Upserted` covers create/write/rename-into;
/// `Removed` covers explicit removal. Within a debounce window, `Upserted`
/// wins over `Removed` (consumers tolerate `NotFound` on subsequent reads).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileEventKind {
    /// File was created, written, renamed-into, or otherwise upserted.
    Upserted,
    /// File was removed.
    Removed,
}

/// Origin tag for a delivered event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventSource {
    /// Source is the OS kernel (notify watcher).
    Kernel,
    /// Source is an in-process `notify_local_change` call.
    Local,
}

/// Per-subscription match policy applied to event paths after the uniform
/// filters (tempfile, lockfile suppression). All paths are compared as
/// absolute paths (callers should pass already-absolute paths).
#[derive(Debug, Clone)]
pub enum FileMatcher {
    /// Match a single absolute path exactly.
    Exact(PathBuf),
    /// Match any of a small set of absolute paths exactly.
    AnyOf(Vec<PathBuf>),
}

/// Subscription parameters: directory to watch (NonRecursive) and a
/// per-subscription matcher applied after uniform filters.
#[derive(Debug, Clone)]
pub struct WatchSpec {
    /// Parent directory to register with the kernel watcher. The watcher is
    /// always registered NonRecursive; callers that want subdirectory
    /// coverage subscribe per-directory.
    pub dir: PathBuf,
    /// Match policy for event paths.
    pub matcher: FileMatcher,
    /// Optional trailing-edge debounce window. `None` disables debouncing
    /// (events flow through immediately after uniform/matcher filtering).
    pub debounce: Option<Duration>,
}

/// Coarse classification for a watch failure; useful for callers that want
/// to surface different remediation hints.
#[derive(Debug, Clone, Copy)]
pub enum WatchErrorKind {
    /// The notify backend reported a generic error.
    Backend,
    /// The OS-level watch handle limit was exceeded.
    ResourceExhausted,
    /// A path passed to the backend did not exist.
    NotFound,
    /// Permission denied at the backend.
    Permission,
    /// Anything else.
    Other,
}

/// Errors surfaced by [`FileWatchService`].
///
/// `#[source]` carries `notify::Error`. Bumping the `notify` major
/// requires updating consumers that downcast to inspect
/// `notify::ErrorKind`; in this crate the only reader is
/// `classify_notify_err` (kept inside the same module).
#[derive(Debug, Error)]
pub enum WatchError {
    /// The service failed to initialise (notify backend or worker thread).
    #[error("file watcher init failed: {message}")]
    Init {
        /// Coarse classification.
        kind: WatchErrorKind,
        /// Human-readable detail.
        message: String,
        /// Underlying notify error, if any.
        #[source]
        source: Option<notify::Error>,
    },
    /// Failed to register a directory with the backend on subscribe.
    #[error("could not watch {dir}: {message}")]
    Watch {
        /// Directory that failed to register.
        dir: PathBuf,
        /// Coarse classification.
        kind: WatchErrorKind,
        /// Human-readable detail.
        message: String,
        /// Underlying notify error, if any.
        #[source]
        source: Option<notify::Error>,
    },
    /// The dispatcher task has terminated; subscriptions registered now
    /// would be silently dead-on-arrival (handle returned but no events
    /// would ever flow). Surfaced so callers can fall back to polling
    /// rather than registering an entry that pretends to be live.
    #[error("file watcher dispatcher has terminated")]
    DispatcherDead,
}

/// Internal subscription identifier. `0` is reserved for the noop sentinel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct SubscriptionId(u64);

#[derive(Debug)]
struct DirState {
    refcount: usize,
    /// Filesystem identity captured immediately before each successful
    /// `watcher.watch` call (initial install or drift rewatch) and
    /// stored after the call returns `Ok`. `None` between insertion
    /// and the first install (transient), `Some` once watched.
    /// Re-stat on every `subscribe_channel`; mismatch against this
    /// stored value forces a rewatch even when refcount > 0, since
    /// notify NonRecursive watches do not auto-reattach across the
    /// inode change a same-path recreate produces. A rewatch failure
    /// clears this back to `None` so a permanently failing watch
    /// does not retry on every subsequent subscribe.
    installed_identity: Option<WatchIdentity>,
}

struct DeliverySink(mpsc::Sender<FileEvent>);

impl DeliverySink {
    fn try_send(&self, ev: FileEvent) -> Result<(), mpsc::error::TrySendError<FileEvent>> {
        self.0.try_send(ev)
    }
    fn clone_sink(&self) -> Self {
        Self(self.0.clone())
    }
}

struct Subscription {
    spec: WatchSpec,
    sink: DeliverySink,
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

/// Internal dispatcher message. `Kernel` carries a raw notify result; `Local`
/// carries an in-process upsert path published via `notify_local_change`.
enum DispatchMsg {
    Kernel(notify::Result<notify::Event>),
    Local(PathBuf),
}

/// Process-singleton file-watch primitive. Constructed via [`Self::new`] (or
/// [`Self::noop`]); shared via `Arc<Self>`.
pub struct FileWatchService {
    inner: Mutex<Inner>,
    dispatcher_dead: AtomicBool,
    /// Sender into the dispatcher channel for in-process Local events. The
    /// kernel drain thread holds the SOLE original sender; this clone is
    /// what `notify_local_change` uses. On a noop service it is paired with
    /// a receiver that has been dropped before construction returns, so
    /// `send` resolves to Err and `dispatcher_dead` (pre-set on noop)
    /// suppresses the error log.
    tokio_tx: mpsc::UnboundedSender<DispatchMsg>,
    /// Millis-since-epoch of the most recent kernel-error log emission.
    /// `handle_kernel` skips the `warn!` if a prior log fired within
    /// the last 1s; suppressed errors increment `dropped_kernel_err_count`
    /// and surface in the next emitted log. Lock-free CAS for the
    /// hot-path notify-error case.
    last_kernel_warn_unix_ms: std::sync::atomic::AtomicI64,
    /// Count of kernel errors suppressed by the rate-limit since the
    /// last emitted log. Reset to 0 on every emission.
    dropped_kernel_err_count: std::sync::atomic::AtomicU64,
}

/// RAII guard returned by [`FileWatchService::subscribe_channel`]. Dropping
/// deregisters the subscription from the dispatcher and unwatches the
/// directory if no other subscription needs it. `Send + Sync`.
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
    /// Construct the live service. Spawns the drain thread + tokio dispatcher.
    ///
    /// Graceful degradation: on `notify::recommended_watcher`
    /// Err or when the env var `AOE_FILE_WATCH=off` is set, returns
    /// `Ok(Self::noop())` rather than `Err`. The only error case surfaced by
    /// this constructor today is failure to spawn the drain thread.
    ///
    /// # Errors
    ///
    /// Returns [`WatchError::Init`] if spawning the dedicated drain
    /// `std::thread` fails (e.g., per-process thread limit reached).
    ///
    /// Per-process single-instance rule: each process constructs exactly
    /// one live service at bootstrap and threads `Arc<Self>` through every
    /// consumer rather than building its own. Integration tests outside
    /// the crate go through `test_support::new_filewatch` for clarity.
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
            match notify_tx.send(res) {
                Ok(()) => {}
                Err(_) => {
                    // Drain thread terminated; service teardown in progress.
                    // Logging would spam during shutdown and recovery is
                    // impossible, so silent discard is the correct policy.
                }
            }
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
        let svc = Arc::new(FileWatchService {
            inner: Mutex::new(Inner {
                watcher: Some(watcher),
                subscriptions: HashMap::new(),
                dirs: HashMap::new(),
                next_id: 1,
                pending: HashMap::new(),
                slots: BTreeMap::new(),
            }),
            dispatcher_dead: AtomicBool::new(false),
            tokio_tx: tokio_tx.clone(),
            last_kernel_warn_unix_ms: std::sync::atomic::AtomicI64::new(0),
            dropped_kernel_err_count: std::sync::atomic::AtomicU64::new(0),
        });

        // Drain thread: holds `notify_rx` and the SOLE `tokio_tx`. Service
        // reference is a `Weak` so the loop exits cleanly when the user drops
        // the last `Arc<FileWatchService>` (the watcher inside `Inner` drops,
        // notify shuts down, `notify_rx.recv()` returns Err).
        let svc_weak_for_drain = Arc::downgrade(&svc);
        std::thread::Builder::new()
            .name("file_watch_drain".into())
            .spawn(move || loop {
                match notify_rx.recv() {
                    Ok(res) => {
                        if tokio_tx.send(DispatchMsg::Kernel(res)).is_err() {
                            if let Some(svc) = svc_weak_for_drain.upgrade() {
                                log_dispatcher_dead_once(&svc, "dispatcher_channel_closed");
                            }
                            return;
                        }
                    }
                    Err(e) => {
                        if let Some(svc) = svc_weak_for_drain.upgrade() {
                            log_dispatcher_dead_once_err(&svc, "notify_channel_closed", e);
                        }
                        return;
                    }
                }
            })
            .map_err(|e| WatchError::Init {
                kind: WatchErrorKind::Other,
                message: format!("could not spawn file_watch_drain thread: {e}"),
                source: None,
            })?;

        // Dispatcher task: holds the tokio_rx and a `Weak` so that dropping
        // the service tears the dispatcher down without strong-ref leaks.
        let svc_weak_for_dispatch = Arc::downgrade(&svc);
        crate::task_util::spawn_supervised(
            "file_watch.dispatcher",
            crate::task_util::PanicPolicy::Log,
            dispatcher_loop(svc_weak_for_dispatch, tokio_rx),
        );

        Ok(svc)
    }

    /// Construct a noop service: no kernel watcher, no drain thread, no
    /// dispatcher. [`Self::subscribe_channel`] returns Ok with an
    /// immediately-closed receiver (its paired sender is dropped before
    /// return). [`Self::notify_local_change`] is silently a no-op: the
    /// dispatcher channel's receiver is dropped at construction so any
    /// `send` Errs, and `dispatcher_dead` is pre-set so the error log path
    /// short-circuits. Used by `Storage::new_unwatched` and as the
    /// graceful-degradation fallback when the live constructor fails.
    /// Integration tests outside the crate construct via
    /// `Storage::new_unwatched` or `test_support::noop_filewatch`.
    pub fn noop() -> Arc<Self> {
        let (tokio_tx, tokio_rx) = mpsc::unbounded_channel::<DispatchMsg>();
        // Drop the receiver before returning so any future `tokio_tx.send`
        // resolves to Err without delivering. The pre-set
        // `dispatcher_dead` below makes the resulting
        // `notify_local_change` Err path skip the error log.
        drop(tokio_rx);
        Arc::new(FileWatchService {
            inner: Mutex::new(Inner {
                watcher: None,
                subscriptions: HashMap::new(),
                dirs: HashMap::new(),
                next_id: 1,
                pending: HashMap::new(),
                slots: BTreeMap::new(),
            }),
            dispatcher_dead: AtomicBool::new(true),
            tokio_tx,
            last_kernel_warn_unix_ms: std::sync::atomic::AtomicI64::new(0),
            dropped_kernel_err_count: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Publish an in-process Upserted event for `path`. Used by writers in
    /// the same process (e.g. `Storage::update` after `atomic_write`) to
    /// surface their change immediately. When the kernel echo lands before
    /// the current debounce slot fires, the two deliveries collapse into one;
    /// on slower backends the echo can arrive later and trigger a second,
    /// idempotent delivery. Crate-private; never exposed publicly.
    ///
    /// On a live service this `send` is microseconds and never blocks. On a
    /// noop service the dispatcher channel's receiver was dropped at
    /// construction so the send Errs and the pre-set `dispatcher_dead`
    /// latch suppresses the otherwise-misleading dispatcher-dead log.
    pub(crate) fn notify_local_change(&self, path: &Path) {
        // Sticky latch fast-path: noop services pre-set this true at
        // construction; live services flip it on dispatcher exit. Saves
        // a syscall (canonicalize) and a doomed channel send per call,
        // hot when the daemon process is shutting down or a CLI
        // subprocess is using `Storage::new_unwatched`.
        if self.dispatcher_dead.load(Ordering::Acquire) {
            return;
        }
        // Canonicalise so the debounce key (SubscriptionId, path) matches
        // the kernel echo's canonical form (e.g. `/private/var/...` on
        // macOS). Without this they hash to different debounce slots and
        // fire as two deliveries instead of collapsing. Fallback to the
        // raw path if canonicalize fails (e.g. an Upserted notification
        // for a path the kernel can no longer stat); the dispatcher's
        // `path.starts_with(&sub.spec.dir)` scope check still uses the
        // canonicalized `spec.dir`, so a non-canonical Local path will
        // miss matching subscriptions and silently drop, which is the
        // same outcome as the original kernel event would produce on the
        // (rare) post-rename-then-unlink race.
        let final_path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
        if self.tokio_tx.send(DispatchMsg::Local(final_path)).is_err() {
            log_dispatcher_dead_once_err(self, "local_send_failed", "channel closed");
        }
    }

    /// Subscribe and receive events on a bounded mpsc channel.
    ///
    /// On a noop service, returns Ok with an immediately-closed receiver
    /// (its paired sender is dropped before return); `recv().await` resolves
    /// to `None`. Dropping the receiver does NOT unsubscribe; drop the
    /// returned [`SubscriptionHandle`] for that.
    ///
    /// `capacity` caps the per-subscriber channel; on backpressure the
    /// dispatcher uses `try_send` and drops with a rate-limited `debug!`.
    /// `capacity == 0` is normalised to 1 (tokio mpsc rejects zero).
    ///
    /// # Errors
    ///
    /// Returns [`WatchError::Watch`] if registering the spec's directory
    /// with the kernel backend fails (refcount is rolled back atomically;
    /// the partial-state invariant is maintained).
    ///
    /// Returns [`WatchError::DispatcherDead`] when the live service's
    /// dispatcher has terminated since construction. Noop services hit
    /// the silent-receiver short-circuit instead and never see this.
    pub fn subscribe_channel(
        self: &Arc<Self>,
        spec: WatchSpec,
        capacity: usize,
    ) -> Result<(mpsc::Receiver<FileEvent>, SubscriptionHandle), WatchError> {
        let mut inner = self.inner.lock().expect("file_watch inner mutex poisoned");
        let cap = capacity.max(1);
        // Noop short-circuit before touching subscriptions / dirs.
        if inner.watcher.is_none() {
            // Dropping `_tx` before return makes `rx.recv().await` resolve
            // to `None` immediately.
            let (_tx, rx) = mpsc::channel::<FileEvent>(cap);
            return Ok((
                rx,
                SubscriptionHandle {
                    id: NOOP_SENTINEL,
                    service: Arc::downgrade(self),
                },
            ));
        }
        // A live service whose dispatcher has died (panic or channel
        // close) must surface the failure: the subscriptions map would
        // otherwise accept an entry that silently never receives events.
        if self.dispatcher_dead.load(Ordering::Acquire) {
            return Err(WatchError::DispatcherDead);
        }
        // Canonicalise the directory so kernel-emitted paths (which arrive
        // already canonical, e.g., `/private/var/...` on macOS) match the
        // dir-scoping check below. This also surfaces non-existent dirs as
        // `WatchError::Watch` before we touch the dirs map.
        let canonical_dir = match std::fs::canonicalize(&spec.dir) {
            Ok(p) => p,
            Err(e) => {
                return Err(WatchError::Watch {
                    dir: spec.dir.clone(),
                    kind: classify_io_err_kind(e.kind()),
                    message: format!("canonicalize failed: {e}"),
                    source: None,
                });
            }
        };
        let mut spec = spec;
        spec.dir = canonical_dir.clone();
        let dir = canonical_dir;
        let current_identity = capture_watch_identity(&dir).ok();
        let drift_against_existing = match (current_identity, inner.dirs.get(&dir)) {
            (Some(curr), Some(state)) => match state.installed_identity {
                Some(stored) => stored != curr,
                None => false,
            },
            (None, Some(state)) => state.installed_identity.is_some(),
            _ => false,
        };
        let pre_bump = inner
            .dirs
            .entry(dir.clone())
            .or_insert(DirState {
                refcount: 0,
                installed_identity: None,
            })
            .refcount;
        // Release the immutable borrow before re-borrowing mutably below.
        inner.dirs.get_mut(&dir).expect("just inserted").refcount = pre_bump + 1;
        let needs_install = pre_bump == 0;
        if needs_install || drift_against_existing {
            // Lock STAYS HELD across the watch call to preserve the
            // invariant: another subscriber must not observe
            // refcount==1 mid-rollback. Re-arming on inode drift
            // calls `Watcher::watch` a second time on the same path;
            // notify-rs is expected to install a fresh kernel
            // descriptor against the new inode (inotify allocates a
            // new wd, FSEvents starts a new stream, kqueue reopens
            // the fd).
            let watcher = inner.watcher.as_mut().expect(
                "watcher initialized in FileWatchService::new; noop path short-circuits above",
            );
            if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
                if needs_install {
                    inner.dirs.remove(&dir);
                } else {
                    let state = inner
                        .dirs
                        .get_mut(&dir)
                        .expect("entry exists during drift rewatch");
                    state.refcount = pre_bump;
                    state.installed_identity = None;
                }
                let kind = classify_notify_err(&e);
                return Err(WatchError::Watch {
                    dir: dir.clone(),
                    kind,
                    message: format!("notify watch failed: {e}"),
                    source: Some(e),
                });
            }
            if let Some(curr) = current_identity {
                inner
                    .dirs
                    .get_mut(&dir)
                    .expect("entry exists post-watch")
                    .installed_identity = Some(curr);
            }
        }

        let (tx, rx) = mpsc::channel::<FileEvent>(cap);
        let id = SubscriptionId(inner.next_id);
        inner.next_id = inner.next_id.checked_add(1).unwrap_or(1);
        inner.subscriptions.insert(
            id,
            Subscription {
                spec,
                sink: DeliverySink(tx),
            },
        );
        Ok((
            rx,
            SubscriptionHandle {
                id,
                service: Arc::downgrade(self),
            },
        ))
    }

    #[cfg(not(any(test, feature = "test-support")))]
    fn subscriber_count(&self) -> usize {
        self.inner
            .lock()
            .map(|inner| inner.subscriptions.len())
            .unwrap_or(0)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn subscriber_count(&self) -> usize {
        self.inner
            .lock()
            .map(|inner| inner.subscriptions.len())
            .unwrap_or(0)
    }
}

#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod test_support {
    use super::{Arc, FileWatchService, WatchError};

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
        ErrorKind::Io(ref io) => match io.kind() {
            std::io::ErrorKind::NotFound => WatchErrorKind::NotFound,
            std::io::ErrorKind::PermissionDenied => WatchErrorKind::Permission,
            _ => WatchErrorKind::Backend,
        },
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

fn log_dispatcher_dead_once(svc: &FileWatchService, reason: &'static str) {
    if !svc.dispatcher_dead.swap(true, Ordering::AcqRel) {
        tracing::error!(
            target: "file_watch.service",
            reason,
            subscribers_affected = svc.subscriber_count(),
            "file watch dispatcher exiting; live propagation disabled, polling fallback canonical"
        );
    }
}

fn log_dispatcher_dead_once_err(
    svc: &FileWatchService,
    reason: &'static str,
    err: impl std::fmt::Display,
) {
    if !svc.dispatcher_dead.swap(true, Ordering::AcqRel) {
        tracing::error!(
            target: "file_watch.service",
            reason,
            error = %err,
            subscribers_affected = svc.subscriber_count(),
            "file watch dispatcher exiting; live propagation disabled, polling fallback canonical"
        );
    }
}

/// Uniform tempfile suppression. Covers both
/// `tempfile::NamedTempFile::new_in` (`.tmp*` prefix) and rename-based
/// atomic writes (`runtime_filter.tmp` suffix), plus common editor temp
/// files (`~`-prefixed, emacs `.#name`).
fn is_tempfile(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(OsStr::to_str) else {
        return false;
    };
    if name.starts_with(".tmp") || name.ends_with(".tmp") {
        return true;
    }
    if name.starts_with('~') || name.starts_with(".#") {
        return true;
    }
    false
}

/// Uniform lockfile suppression. Matches `^\.[a-z_-]+\.lock$`.
fn is_lockfile(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(OsStr::to_str) else {
        return false;
    };
    let Some(stripped) = name.strip_prefix('.').and_then(|s| s.strip_suffix(".lock")) else {
        return false;
    };
    !stripped.is_empty()
        && stripped
            .chars()
            .all(|c| c.is_ascii_lowercase() || c == '_' || c == '-')
}

fn matcher_matches(spec: &WatchSpec, path: &Path) -> bool {
    // Compare by `file_name` only: kernel-delivered paths on macOS are
    // canonical (e.g., `/private/var/...`) while caller-supplied matcher
    // paths typically use the non-canonical original. The directory scope
    // is enforced separately via `path.starts_with(&spec.dir)` against the
    // canonicalised `spec.dir` stored on subscribe.
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
    // Modify(Metadata|Other) skipped: chmod/chown/utime do not change the
    // bytes consumers re-read on Upserted; classifying them as Upserted
    // would burn a reload per touch. Modify(Any) preserved because the
    // PollWatcher backend (and FreeBSD kqueue) emit `Any` for content
    // writes; dropping it would silence legitimate updates on those
    // platforms.
    match ev.kind {
        EventKind::Create(_) => Some(FileEventKind::Upserted),
        EventKind::Modify(ModifyKind::Data(_) | ModifyKind::Name(_) | ModifyKind::Any) => {
            Some(FileEventKind::Upserted)
        }
        EventKind::Modify(ModifyKind::Metadata(_) | ModifyKind::Other) => None,
        EventKind::Remove(_) => Some(FileEventKind::Removed),
        _ => None,
    }
}

async fn dispatcher_loop(
    svc: Weak<FileWatchService>,
    mut rx: mpsc::UnboundedReceiver<DispatchMsg>,
) {
    // RAII guard ensuring `dispatcher_dead` is flipped on ANY termination
    // path of `run_dispatcher`, including panic. Without this, a panic
    // inside `run_dispatcher` would unwind past the post-await
    // `log_dispatcher_dead_once` call and leave the latch false until
    // the next channel send fails, producing a window where new
    // subscriptions register against a dead dispatcher and silently
    // never receive events.
    struct ExitLatch<'a> {
        svc: &'a Weak<FileWatchService>,
    }
    impl Drop for ExitLatch<'_> {
        fn drop(&mut self) {
            if let Some(arc) = self.svc.upgrade() {
                log_dispatcher_dead_once(&arc, "dispatcher_loop_exit");
            }
        }
    }
    let _guard = ExitLatch { svc: &svc };
    let exit_reason = run_dispatcher(svc.clone(), &mut rx).await;
    if let Some(arc) = svc.upgrade() {
        log_dispatcher_dead_once(&arc, exit_reason);
    }
}

async fn run_dispatcher(
    svc: Weak<FileWatchService>,
    rx: &mut mpsc::UnboundedReceiver<DispatchMsg>,
) -> &'static str {
    loop {
        let next_fire: Option<Instant> = match svc.upgrade() {
            Some(arc) => {
                let inner = arc.inner.lock().expect("file_watch inner mutex poisoned");
                inner.slots.keys().next().copied()
            }
            None => return "service_dropped",
        };
        tokio::select! {
            biased;
            msg = rx.recv() => {
                match msg {
                    Some(DispatchMsg::Kernel(res)) => {
                        let Some(arc) = svc.upgrade() else { return "service_dropped" };
                        handle_kernel(&arc, res);
                    }
                    Some(DispatchMsg::Local(path)) => {
                        let Some(arc) = svc.upgrade() else { return "service_dropped" };
                        // Local upserts traverse the SAME uniform filter and
                        // per-(SubscriptionId, path) trailing-edge debounce
                        // as kernel events. The local event arrives first;
                        // the kernel echo arrives ~ms later for the same
                        // atomic_write rename and collapses into the same
                        // debounce slot, yielding one delivery per logical
                        // write.
                        dispatch_path(&arc, &path, FileEventKind::Upserted, EventSource::Local);
                    }
                    None => return "channel_closed",
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
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            let last = svc
                .last_kernel_warn_unix_ms
                .load(std::sync::atomic::Ordering::Acquire);
            if now_ms.saturating_sub(last) >= 1_000 {
                svc.last_kernel_warn_unix_ms
                    .store(now_ms, std::sync::atomic::Ordering::Release);
                let dropped = svc
                    .dropped_kernel_err_count
                    .swap(0, std::sync::atomic::Ordering::AcqRel);
                tracing::warn!(
                    target: "file_watch.service",
                    error = %e,
                    dropped_since_last = dropped,
                    "kernel watcher emitted error; live propagation may degrade until next valid event"
                );
            } else {
                svc.dropped_kernel_err_count
                    .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            }
            return;
        }
    };
    let Some(kind) = classify_event_kind(&ev) else {
        return;
    };
    if ev.paths.is_empty() {
        return;
    }
    for path in ev.paths.iter() {
        dispatch_path(svc, path, kind, EventSource::Kernel);
    }
}

/// Run the uniform filter, per-subscription matcher, and per-key debounce
/// for one (path, kind, source) triple. Shared by the Kernel and Local
/// dispatcher arms so an in-process write and the kernel echo for the same
/// atomic rename land in the same debounce slot.
fn dispatch_path(
    svc: &Arc<FileWatchService>,
    path: &Path,
    kind: FileEventKind,
    source: EventSource,
) {
    if is_tempfile(path) || is_lockfile(path) {
        return;
    }
    // Snapshot matching subscriptions under lock; release before any send.
    let matched: Vec<(SubscriptionId, Option<Duration>, DeliverySink)> = {
        let inner = svc.inner.lock().expect("file_watch inner mutex poisoned");
        inner
            .subscriptions
            .iter()
            .filter(|(_, sub)| path.starts_with(&sub.spec.dir) && matcher_matches(&sub.spec, path))
            .map(|(id, sub)| (*id, sub.spec.debounce, sub.sink.clone_sink()))
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

fn deliver(sink: &DeliverySink, ev: FileEvent, id: SubscriptionId) {
    if let Err(e) = sink.try_send(ev) {
        match e {
            mpsc::error::TrySendError::Full(dropped) => {
                tracing::debug!(
                    target: "file_watch.subscriber",
                    subscriber_id = id.0,
                    path = %dropped.path.display(),
                    "dropping file event: subscriber channel full"
                );
            }
            mpsc::error::TrySendError::Closed(_) => {
                tracing::debug!(
                    target: "file_watch.subscriber",
                    subscriber_id = id.0,
                    "dropping file event: subscriber receiver closed"
                );
            }
        }
    }
}

/// Insert or refresh a debounce entry for the given (subscription, path)
/// key. Always extends the deadline to `now + window` and pushes a fresh
/// slot in `slots`; older slots become stale and self-evict via the
/// `fire_at` equality check at fire time.
fn arm_debounce(svc: &Arc<FileWatchService>, id: SubscriptionId, ev: FileEvent, window: Duration) {
    let fire_at = Instant::now() + window;
    let key = (id, ev.path.clone());
    let mut inner = svc.inner.lock().expect("file_watch inner mutex poisoned");
    match inner.pending.get_mut(&key) {
        Some(entry) => {
            // Coalesce: Upserted wins over Removed within a window.
            if matches!(entry.pending.kind, FileEventKind::Removed)
                && matches!(ev.kind, FileEventKind::Upserted)
            {
                entry.pending = ev;
            }
            entry.fire_at = fire_at;
        }
        None => {
            inner.pending.insert(
                key.clone(),
                DebounceEntry {
                    pending: ev,
                    fire_at,
                },
            );
        }
    }
    inner.slots.entry(fire_at).or_default().push(key);
}

/// Pop all due slots and fire them (skipping stale slots whose `fire_at`
/// disagrees with the live entry).
fn fire_due(svc: &Arc<FileWatchService>) {
    let now = Instant::now();
    let mut to_deliver: Vec<(SubscriptionId, FileEvent, DeliverySink)> = Vec::new();
    {
        let mut inner = svc.inner.lock().expect("file_watch inner mutex poisoned");
        let due_keys: Vec<Instant> = inner.slots.range(..=now).map(|(k, _)| *k).collect();
        for slot_at in due_keys {
            let Some(keys) = inner.slots.remove(&slot_at) else {
                continue;
            };
            for key in keys {
                let stale = match inner.pending.get(&key) {
                    Some(entry) => entry.fire_at != slot_at,
                    None => true,
                };
                if stale {
                    continue;
                }
                let entry = match inner.pending.remove(&key) {
                    Some(e) => e,
                    None => continue,
                };
                if let Some(sub) = inner.subscriptions.get(&key.0) {
                    to_deliver.push((key.0, entry.pending, sub.sink.clone_sink()));
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
        // Tolerate a poisoned mutex: if a prior holder panicked, we still
        // want to release our subscription state without a second panic in
        // a destructor (which would `abort()`).
        let mut inner = match svc.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let Some(sub) = inner.subscriptions.remove(&self.id) else {
            return;
        };
        let dir = sub.spec.dir.clone();
        let mut should_unwatch = false;
        if let Some(state) = inner.dirs.get_mut(&dir) {
            state.refcount = state.refcount.saturating_sub(1);
            if state.refcount == 0 {
                should_unwatch = true;
            }
        }
        if should_unwatch {
            inner.dirs.remove(&dir);
            if let Some(w) = inner.watcher.as_mut() {
                let _ = w.unwatch(&dir);
            }
        }
        // Evict any pending debounce entries for this subscription so a
        // late-firing slot can't observe a removed subscription.
        inner.pending.retain(|(sid, _), _| *sid != self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::time::Duration;
    use tempfile::TempDir;
    use tokio::time::timeout;

    fn write_file(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, contents).expect("write");
        p
    }

    /// Wait budget to absorb FSEvents coalescing on macOS. On Linux/inotify
    /// events typically deliver within tens of ms; macOS FSEvents can take
    /// up to ~1.5s to forward small writes, so the ceiling is generous.
    const KERNEL_WAIT: Duration = Duration::from_millis(2_500);
    /// Negative-test budget: long enough to confidently say "no event".
    const NEG_WAIT: Duration = Duration::from_millis(300);

    /// Test 1
    #[tokio::test]
    #[serial(file_watch)]
    async fn service_init_returns_arc_on_success() {
        let svc = FileWatchService::new().expect("init");
        // Live service: watcher present, dispatcher_dead unset.
        let inner = svc.inner.lock().unwrap();
        assert!(inner.watcher.is_some(), "live service must have watcher");
        assert!(!svc.dispatcher_dead.load(Ordering::Acquire));
    }

    /// Test 2
    #[tokio::test]
    #[serial(file_watch)]
    async fn service_init_returns_noop_on_env_off() {
        // SAFETY: `set_var`/`remove_var` are unsafe in 2024 edition for a
        // good reason (data races with other threads reading env). The
        // `#[serial(file_watch)]` annotation prevents in-process races,
        // and no other thread reads `AOE_FILE_WATCH` concurrently here.
        let prev = std::env::var("AOE_FILE_WATCH").ok();
        // SAFETY: see comment above.
        unsafe { std::env::set_var("AOE_FILE_WATCH", "off") };
        let svc = FileWatchService::new().expect("noop init");
        // Restore env before any panic-prone assertion.
        match prev {
            Some(v) => unsafe { std::env::set_var("AOE_FILE_WATCH", v) },
            None => unsafe { std::env::remove_var("AOE_FILE_WATCH") },
        }
        let inner = svc.inner.lock().unwrap();
        assert!(inner.watcher.is_none(), "noop service must lack watcher");
    }

    /// Test 3
    #[tokio::test]
    #[serial(file_watch)]
    async fn subscribe_channel_fires_on_real_write() {
        let dir = TempDir::new().unwrap();
        let svc = FileWatchService::new().expect("init");
        let target = dir.path().join("watched.txt");
        let (mut rx, _h) = svc
            .subscribe_channel(
                WatchSpec {
                    dir: dir.path().to_path_buf(),
                    matcher: FileMatcher::Exact(target.clone()),
                    debounce: None,
                },
                8,
            )
            .expect("subscribe");
        write_file(dir.path(), "watched.txt", "hello");
        let ev = timeout(KERNEL_WAIT, rx.recv())
            .await
            .expect("kernel event arrives within budget")
            .expect("channel open");
        assert_eq!(
            ev.path.file_name(),
            target.file_name(),
            "event must be for the watched file"
        );
        assert_eq!(ev.source, EventSource::Kernel);
    }

    /// Test 4
    #[tokio::test]
    #[serial(file_watch)]
    async fn subscribe_channel_filters_tempfiles() {
        let dir = TempDir::new().unwrap();
        let svc = FileWatchService::new().expect("init");
        // Match anything under the watched dir by using `AnyOf` of the two
        // file names this test could care about; the tempfile filter must
        // suppress `runtime_filter.tmp` despite the matcher allowing it.
        let final_path = dir.path().join("runtime_filter");
        let tmp_path = dir.path().join("runtime_filter.tmp");
        let (mut rx, _h) = svc
            .subscribe_channel(
                WatchSpec {
                    dir: dir.path().to_path_buf(),
                    matcher: FileMatcher::AnyOf(vec![final_path.clone(), tmp_path.clone()]),
                    debounce: None,
                },
                8,
            )
            .expect("subscribe");
        write_file(dir.path(), "runtime_filter.tmp", "x");
        // Negative wait: no event for the tempfile.
        assert!(
            timeout(NEG_WAIT, rx.recv()).await.is_err(),
            "tempfile event must be filtered"
        );
    }

    /// Test 5
    #[tokio::test]
    #[serial(file_watch)]
    async fn subscribe_channel_filters_unmatched_paths() {
        let dir = TempDir::new().unwrap();
        let svc = FileWatchService::new().expect("init");
        let target = dir.path().join("only-this");
        let (mut rx, _h) = svc
            .subscribe_channel(
                WatchSpec {
                    dir: dir.path().to_path_buf(),
                    matcher: FileMatcher::Exact(target),
                    debounce: None,
                },
                8,
            )
            .expect("subscribe");
        write_file(dir.path(), "something-else", "x");
        assert!(
            timeout(NEG_WAIT, rx.recv()).await.is_err(),
            "unmatched path must not deliver"
        );
    }

    /// Test 6
    #[tokio::test]
    #[serial(file_watch)]
    async fn subscribe_channel_capacity_drops_on_full() {
        let dir = TempDir::new().unwrap();
        let svc = FileWatchService::new().expect("init");
        let target = dir.path().join("burst");
        let (mut rx, _h) = svc
            .subscribe_channel(
                WatchSpec {
                    dir: dir.path().to_path_buf(),
                    matcher: FileMatcher::Exact(target.clone()),
                    debounce: None,
                },
                1, // capacity 1 forces drop on the second concurrent event
            )
            .expect("subscribe");
        // Hammer the file: kernel will produce >1 event without the consumer
        // draining. We never drain `rx` here, so once the first event lands,
        // any subsequent try_send hits TrySendError::Full and is dropped.
        for _ in 0..20 {
            write_file(dir.path(), "burst", "x");
        }
        // Receive at least one event; the rest may have been dropped.
        let first = timeout(KERNEL_WAIT, rx.recv())
            .await
            .expect("at least one event")
            .expect("channel open");
        assert_eq!(first.path.file_name(), target.file_name());
    }

    /// Test 7
    #[tokio::test]
    #[serial(file_watch)]
    async fn multiple_subscriptions_demuxed_correctly() {
        let dir = TempDir::new().unwrap();
        let svc = FileWatchService::new().expect("init");
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        let (mut rx_a, _ha) = svc
            .subscribe_channel(
                WatchSpec {
                    dir: dir.path().to_path_buf(),
                    matcher: FileMatcher::Exact(a.clone()),
                    debounce: None,
                },
                8,
            )
            .expect("subscribe a");
        let (mut rx_b, _hb) = svc
            .subscribe_channel(
                WatchSpec {
                    dir: dir.path().to_path_buf(),
                    matcher: FileMatcher::Exact(b.clone()),
                    debounce: None,
                },
                8,
            )
            .expect("subscribe b");
        write_file(dir.path(), "a", "x");
        let ev = timeout(KERNEL_WAIT, rx_a.recv())
            .await
            .expect("a event")
            .expect("open");
        assert_eq!(ev.path.file_name(), a.file_name());
        // No event on b channel.
        assert!(
            timeout(NEG_WAIT, rx_b.recv()).await.is_err(),
            "b subscription must not see a's event"
        );
    }

    /// Test 8
    #[tokio::test]
    #[serial(file_watch)]
    async fn subscription_handle_drop_unsubscribes() {
        let dir = TempDir::new().unwrap();
        let svc = FileWatchService::new().expect("init");
        let target = dir.path().join("watched");
        let (mut rx, h) = svc
            .subscribe_channel(
                WatchSpec {
                    dir: dir.path().to_path_buf(),
                    matcher: FileMatcher::Exact(target),
                    debounce: None,
                },
                8,
            )
            .expect("subscribe");
        // Drop the handle: dispatcher should no longer deliver.
        drop(h);
        write_file(dir.path(), "watched", "x");
        // After handle drop the dispatcher's matching set should not include
        // this subscription. Receiver is still around but the corresponding
        // sender lives inside the (now-removed) Subscription record, so the
        // sender is dropped and `recv()` resolves to `None` quickly.
        let res = timeout(NEG_WAIT, rx.recv()).await;
        match res {
            Ok(None) => {} // sender dropped, channel closed (expected)
            Ok(Some(_)) => panic!("event delivered after handle drop"),
            Err(_) => {} // timed out; also acceptable (no delivery)
        }
    }

    /// Test 9
    #[tokio::test]
    #[serial(file_watch)]
    async fn subscription_handle_drop_unwatches_dir_on_zero_refcount() {
        let dir = TempDir::new().unwrap();
        let svc = FileWatchService::new().expect("init");
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let (rx, h) = svc
            .subscribe_channel(
                WatchSpec {
                    dir: dir.path().to_path_buf(),
                    matcher: FileMatcher::Exact(dir.path().join("x")),
                    debounce: None,
                },
                4,
            )
            .expect("subscribe");
        {
            let inner = svc.inner.lock().unwrap();
            assert_eq!(inner.dirs.get(&canonical).map(|d| d.refcount), Some(1));
        }
        drop(h);
        drop(rx);
        // Drop is synchronous; no await needed.
        let inner = svc.inner.lock().unwrap();
        assert!(
            !inner.dirs.contains_key(&canonical),
            "dir entry must be removed on refcount==0"
        );
    }

    /// Test 10
    #[tokio::test]
    #[serial(file_watch)]
    async fn subscription_handle_drop_keeps_dir_when_refcount_nonzero() {
        let dir = TempDir::new().unwrap();
        let svc = FileWatchService::new().expect("init");
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let (_rx_a, ha) = svc
            .subscribe_channel(
                WatchSpec {
                    dir: dir.path().to_path_buf(),
                    matcher: FileMatcher::Exact(dir.path().join("a")),
                    debounce: None,
                },
                4,
            )
            .expect("subscribe a");
        let (_rx_b, _hb) = svc
            .subscribe_channel(
                WatchSpec {
                    dir: dir.path().to_path_buf(),
                    matcher: FileMatcher::Exact(dir.path().join("b")),
                    debounce: None,
                },
                4,
            )
            .expect("subscribe b");
        {
            let inner = svc.inner.lock().unwrap();
            assert_eq!(inner.dirs.get(&canonical).map(|d| d.refcount), Some(2));
        }
        drop(ha);
        let inner = svc.inner.lock().unwrap();
        assert_eq!(
            inner.dirs.get(&canonical).map(|d| d.refcount),
            Some(1),
            "refcount must decrement, dir entry must remain"
        );
    }

    /// Test 11
    #[tokio::test]
    async fn subscribe_channel_returns_immediately_closed_receiver_on_noop() {
        let svc = FileWatchService::noop();
        let (mut rx, _h) = svc
            .subscribe_channel(
                WatchSpec {
                    dir: PathBuf::from("/nowhere"),
                    matcher: FileMatcher::Exact(PathBuf::from("/nowhere/x")),
                    debounce: None,
                },
                4,
            )
            .expect("noop subscribe");
        // The paired sender was dropped before return; recv resolves to None.
        let res = timeout(NEG_WAIT, rx.recv()).await;
        assert!(matches!(res, Ok(None)));
    }

    /// Test 12
    #[tokio::test]
    async fn noop_handle_drop_does_not_panic() {
        let svc = FileWatchService::noop();
        let (_rx, h) = svc
            .subscribe_channel(
                WatchSpec {
                    dir: PathBuf::from("/nowhere"),
                    matcher: FileMatcher::Exact(PathBuf::from("/nowhere/x")),
                    debounce: None,
                },
                4,
            )
            .expect("noop subscribe");
        drop(h); // Must not panic.
    }

    /// Test 13
    #[tokio::test]
    #[serial(file_watch)]
    async fn debounce_collapses_burst_to_one_event() {
        let dir = TempDir::new().unwrap();
        let svc = FileWatchService::new().expect("init");
        let target = dir.path().join("debounced");
        let (mut rx, _h) = svc
            .subscribe_channel(
                WatchSpec {
                    dir: dir.path().to_path_buf(),
                    matcher: FileMatcher::Exact(target.clone()),
                    debounce: Some(Duration::from_millis(75)),
                },
                32,
            )
            .expect("subscribe");
        // Burst of writes: kernel emits a flurry, debounce should collapse.
        for i in 0..10 {
            write_file(dir.path(), "debounced", &format!("v{i}"));
        }
        // First fire should land within roughly the debounce window after
        // the last kernel event hits the dispatcher; budget generously.
        let first = timeout(KERNEL_WAIT, rx.recv())
            .await
            .expect("debounced event")
            .expect("open");
        assert_eq!(first.path.file_name(), target.file_name());
        // After collapse there should be no immediate follow-up.
        let second = timeout(NEG_WAIT, rx.recv()).await;
        assert!(
            second.is_err() || matches!(second, Ok(None)),
            "burst must collapse to a single delivery"
        );
    }

    /// Test 14
    ///
    /// Verifies that two distinct paths under one subscription each get
    /// their own debounce slot. We separate the writes by more than one
    /// debounce window so the first slot fires before the second write
    /// arrives; this keeps the test deterministic on macOS FSEvents,
    /// which sometimes coalesces back-to-back writes into a single batch.
    #[tokio::test]
    #[serial(file_watch)]
    async fn debounce_per_subscription_per_path_independent() {
        let dir = TempDir::new().unwrap();
        let svc = FileWatchService::new().expect("init");
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        let (mut rx, _h) = svc
            .subscribe_channel(
                WatchSpec {
                    dir: dir.path().to_path_buf(),
                    matcher: FileMatcher::AnyOf(vec![a.clone(), b.clone()]),
                    debounce: Some(Duration::from_millis(75)),
                },
                32,
            )
            .expect("subscribe");
        write_file(dir.path(), "a", "x");
        // Sleep long enough that the debounce slot for "a" has fired AND
        // FSEvents has flushed its coalescing buffer before we write "b".
        tokio::time::sleep(Duration::from_millis(200)).await;
        write_file(dir.path(), "b", "y");
        let mut seen: std::collections::HashSet<std::ffi::OsString> =
            std::collections::HashSet::new();
        let deadline = std::time::Instant::now() + KERNEL_WAIT;
        while seen.len() < 2 {
            let now = std::time::Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline - now;
            match timeout(remaining, rx.recv()).await {
                Ok(Some(ev)) => {
                    if let Some(n) = ev.path.file_name() {
                        seen.insert(n.to_os_string());
                    }
                }
                _ => break,
            }
        }
        assert!(
            seen.contains(a.file_name().unwrap()),
            "expected event for a"
        );
        assert!(
            seen.contains(b.file_name().unwrap()),
            "expected event for b"
        );
    }

    /// Test 15
    #[tokio::test]
    async fn dispatcher_dead_emits_single_error_via_dedup_latch() {
        let svc = FileWatchService::new().expect("init");
        let path = TempDir::new().unwrap().path().join("dead-latch");
        // First flip succeeds (latch was 0).
        log_dispatcher_dead_once(&svc, "first");
        assert!(svc.dispatcher_dead.load(Ordering::Acquire));
        // Subsequent flips are no-ops; the dedup latch ensures we don't
        // emit a second error line.
        log_dispatcher_dead_once(&svc, "second");
        log_dispatcher_dead_once_err(&svc, "third", "boom");
        // Public API calls must never clear the one-way latch again.
        svc.notify_local_change(&path);
        assert!(svc.dispatcher_dead.load(Ordering::Acquire));
    }

    /// Test 16
    #[tokio::test]
    #[serial(file_watch)]
    async fn event_source_kernel_for_external_writes() {
        let dir = TempDir::new().unwrap();
        let svc = FileWatchService::new().expect("init");
        let target = dir.path().join("k");
        let (mut rx, _h) = svc
            .subscribe_channel(
                WatchSpec {
                    dir: dir.path().to_path_buf(),
                    matcher: FileMatcher::Exact(target.clone()),
                    debounce: None,
                },
                8,
            )
            .expect("subscribe");
        write_file(dir.path(), "k", "hello");
        let ev = timeout(KERNEL_WAIT, rx.recv())
            .await
            .expect("event")
            .expect("open");
        assert_eq!(ev.source, EventSource::Kernel);
    }

    /// Test 17: subscribing to a definitely-non-existent path must fail
    /// the underlying `watch()` call and roll back atomically (no `dirs`
    /// entry left behind, sibling subscribes succeed).
    #[tokio::test]
    #[serial(file_watch)]
    async fn subscribe_channel_returns_err_on_watch_failure() {
        let svc = FileWatchService::new().expect("init");
        // A path that almost certainly doesn't exist on any test host.
        let bogus = PathBuf::from("/this/path/does/not/exist/file_watch_test");
        let res = svc.subscribe_channel(
            WatchSpec {
                dir: bogus.clone(),
                matcher: FileMatcher::Exact(bogus.join("x")),
                debounce: None,
            },
            4,
        );
        match res {
            Err(WatchError::Watch { dir, .. }) => assert_eq!(dir, bogus),
            Ok(_) => panic!("watching a non-existent path should fail"),
            Err(other) => panic!("expected Watch error, got {other:?}"),
        }
        // Sibling subscribe in a real tempdir must succeed (no leaked state).
        let dir = TempDir::new().unwrap();
        let _sub = svc
            .subscribe_channel(
                WatchSpec {
                    dir: dir.path().to_path_buf(),
                    matcher: FileMatcher::Exact(dir.path().join("y")),
                    debounce: None,
                },
                4,
            )
            .expect("sibling subscribe must succeed");
        // The bogus dir must not have leaked into `dirs`.
        let inner = svc.inner.lock().unwrap();
        assert!(!inner.dirs.contains_key(&bogus));
    }

    /// Test 18: in-process Local upserts traverse the dispatcher and arrive
    /// before any kernel echo for the same path. On low-latency backends the
    /// later kernel echo can still collapse into the active debounce slot;
    /// on slower backends it may arrive late and produce a second, idempotent
    /// delivery. The invariant we actually depend on is Local-first ordering.
    #[tokio::test]
    #[serial(file_watch)]
    async fn notify_local_change_delivers_local_first_and_tolerates_late_kernel_echo() {
        let dir = TempDir::new().unwrap();
        let svc = FileWatchService::new().expect("init");
        let target = dir.path().join("local-coalesce");
        // Pre-create the file BEFORE subscribing so canonicalize on the
        // notify_local_change call below resolves to the same canonical
        // form the kernel will emit. This event fires before subscribe
        // and is therefore not delivered.
        std::fs::write(&target, "seed").expect("seed");
        let (mut rx, _h) = svc
            .subscribe_channel(
                WatchSpec {
                    dir: dir.path().to_path_buf(),
                    matcher: FileMatcher::Exact(target.clone()),
                    debounce: Some(Duration::from_millis(75)),
                },
                8,
            )
            .expect("subscribe");
        // Local first: synchronous tokio_tx.send beats the kernel pipeline
        // (notify worker -> drain thread -> tokio_tx) regardless of host
        // notify-backend latency.
        svc.notify_local_change(&target);
        // Mutate content: kernel emits an Upserted echo on the same canonical
        // path. If it lands before the current slot fires, debounce coalesces
        // the burst; if it lands later, consumers still see an idempotent
        // second delivery for the same file.
        std::fs::write(&target, "mutated").expect("mutate");
        let first = timeout(KERNEL_WAIT, rx.recv())
            .await
            .expect("debounced delivery")
            .expect("channel open");
        assert_eq!(first.path.file_name(), target.file_name());
        assert_eq!(
            first.source,
            EventSource::Local,
            "Local must always arrive before any later kernel echo"
        );
        let second = timeout(KERNEL_WAIT, rx.recv()).await;
        if let Ok(Some(second)) = second {
            assert_eq!(second.path.file_name(), target.file_name());
            assert_eq!(
                second.source,
                EventSource::Kernel,
                "a late second delivery, when present, must be the kernel echo"
            );
        }
    }

    /// Test 19: a Local publish with no matching subscribers is a silent
    /// no-op and must not trip the dispatcher-dead latch.
    #[tokio::test]
    #[serial(file_watch)]
    async fn notify_local_change_with_no_matching_subscribers_is_silent() {
        let dir = TempDir::new().unwrap();
        let svc = FileWatchService::new().expect("init");
        let watched = dir.path().join("watched");
        let missed = dir.path().join("missed");
        // Seed only the path we publish, so canonicalize in
        // notify_local_change resolves. `watched` is intentionally NOT
        // created: the Exact matcher compares file names, so it needs no
        // file on disk, and not creating it avoids a pre-subscribe write
        // that macOS FSEvents can replay as a late kernel echo.
        std::fs::write(&missed, "seed").expect("seed missed");
        let (mut rx, _h) = svc
            .subscribe_channel(
                WatchSpec {
                    dir: dir.path().to_path_buf(),
                    matcher: FileMatcher::Exact(watched.clone()),
                    debounce: None,
                },
                4,
            )
            .expect("subscribe");
        svc.notify_local_change(&missed);
        // The only Local publish was for `missed`, which must never match
        // the `watched` subscription. Drain the budget and assert no
        // Local-sourced delivery arrives; a stray Kernel echo (FSEvents may
        // replay the pre-subscribe seed) is an acceptable artifact and not
        // what this test guards.
        while let Ok(Some(ev)) = timeout(Duration::from_millis(150), rx.recv()).await {
            assert_ne!(
                ev.source,
                EventSource::Local,
                "unmatched Local publish must not deliver to unrelated subscribers"
            );
        }
        assert!(
            !svc.dispatcher_dead.load(Ordering::Acquire),
            "unmatched Local publish must not trip dispatcher_dead"
        );
    }

    /// Test 20: `notify_local_change` on a noop service is silent: no log
    /// line, no panic, no delivery. The dispatcher_dead latch is pre-set on
    /// noop so the send-Err path skips the error log.
    #[tokio::test]
    #[serial(file_watch)]
    async fn notify_local_change_on_noop_is_silent() {
        let svc = FileWatchService::noop();
        // Capture tracing output: the call must not emit any line.
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let buf_for_writer = buf.clone();
        let make_writer = move || TestBufWriter {
            buf: buf_for_writer.clone(),
        };
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(make_writer)
            .with_ansi(false)
            .finish();
        let (mut rx, _h) = svc
            .subscribe_channel(
                WatchSpec {
                    dir: PathBuf::from("/nowhere"),
                    matcher: FileMatcher::Exact(PathBuf::from("/nowhere/x")),
                    debounce: None,
                },
                4,
            )
            .expect("noop subscribe");
        tracing::subscriber::with_default(subscriber, || {
            svc.notify_local_change(&PathBuf::from("/nowhere/x"));
        });
        // No delivery within budget.
        let res = timeout(Duration::from_millis(100), rx.recv()).await;
        assert!(
            matches!(res, Ok(None)) || res.is_err(),
            "noop notify must not deliver"
        );
        let captured = buf.lock().unwrap();
        assert!(
            captured.is_empty(),
            "noop notify_local_change must emit no log lines, got: {}",
            String::from_utf8_lossy(&captured)
        );
    }

    /// `MakeWriter` impl that writes into a shared `Vec<u8>` so the test can
    /// inspect captured tracing output. Simple enough to inline alongside
    /// the noop-silence test rather than pull a heavier helper.
    struct TestBufWriter {
        buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
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

    /// Locks the primitive's self-heal contract: a peer-driven
    /// `rm -rf X && mkdir X` of the same canonical path leaves the
    /// kernel watch in IN_IGNORED limbo, so a subsequent
    /// `subscribe_channel` on the same path (refcount > 0) MUST
    /// detect the inode drift and re-arm the watch.
    #[cfg(unix)]
    #[tokio::test]
    #[serial(file_watch)]
    async fn subscribe_rewatches_when_inode_changed_with_refcount_above_zero() {
        let root = TempDir::new().unwrap();
        let dir = root.path().join("watched");
        std::fs::create_dir_all(&dir).unwrap();
        let svc = FileWatchService::new().expect("init");
        let target = dir.join("file");

        let (_rx_keepalive, _h_keepalive) = svc
            .subscribe_channel(
                WatchSpec {
                    dir: dir.clone(),
                    matcher: FileMatcher::Exact(target.clone()),
                    debounce: None,
                },
                4,
            )
            .expect("first subscribe installs watch");

        let canonical = std::fs::canonicalize(&dir).unwrap();
        let identity_before = {
            let inner = svc.inner.lock().unwrap();
            let state = inner.dirs.get(&canonical).expect("entry exists");
            assert_eq!(state.refcount, 1);
            state
                .installed_identity
                .expect("identity recorded on install")
        };

        // ext4/overlayfs recycle the freed inode number for an immediate
        // same-path recreate, and inode timestamps come from the kernel's
        // coarse clock (jiffy resolution, up to 10ms at HZ=100), so a
        // recreate landing in the same tick as the original create would
        // tie on (dev, ino, btime) and hide the drift from the identity
        // check. Real recreates are seconds away from the original
        // install; the sleep models that gap without flaking on fast
        // filesystems.
        std::thread::sleep(Duration::from_millis(50));
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&dir).unwrap();

        let (mut rx, _h2) = svc
            .subscribe_channel(
                WatchSpec {
                    dir: dir.clone(),
                    matcher: FileMatcher::Exact(target.clone()),
                    debounce: None,
                },
                4,
            )
            .expect("second subscribe re-arms watch on inode drift");

        let identity_after = {
            let inner = svc.inner.lock().unwrap();
            let state = inner.dirs.get(&canonical).expect("entry exists");
            assert_eq!(state.refcount, 2, "second subscribe bumps refcount");
            state
                .installed_identity
                .expect("identity refreshed after rewatch")
        };
        assert_ne!(
            identity_before, identity_after,
            "remove + recreate of the same path must yield a distinct identity \
             (btime breaks the tie when the filesystem recycles the inode number)"
        );

        write_file(&dir, "file", "payload");
        let evt = timeout(KERNEL_WAIT, rx.recv())
            .await
            .expect("event arrives within budget after watch re-arm")
            .expect("channel open");
        let expected = std::fs::canonicalize(&target).expect("target exists post-write");
        assert_eq!(evt.path, expected);
    }
}
