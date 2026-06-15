//! Session storage - JSON file persistence with in-process and cross-process
//! locking.
//!
//! `Storage` serialises read-modify-write cycles via two layers:
//!
//! 1. **In-process per-profile mutex** (one `Arc<Mutex<()>>` per profile name,
//!    registered process-wide). Performance + observability layer, not a
//!    correctness primitive on the supported platforms (Linux, macOS): a
//!    userspace mutex is roughly an order of magnitude cheaper than the
//!    flock syscall on the uncontended path, and same-thread re-entry
//!    deadlocks here immediately rather than via a 50ms polling loop on
//!    the flock. Removing this layer would still produce correct on-disk
//!    state because `fs2::FileExt` maps to `flock(2)`, whose locks are
//!    scoped to the open file description (OFD) on **both** Linux and
//!    macOS/BSD. (A common misconception is that macOS `flock` is
//!    process-scoped; it is not. Apple's flock(2) man page and
//!    `xnu/bsd/kern/kern_descrip.c::sys_flock` key the lock on
//!    `fp->fp_glob`, the open file description, identical in effect to
//!    Linux's documented OFD scoping.) Every `Storage::update` opens its
//!    own fd via `OpenOptions::open`, so two `Storage` handles in the
//!    same process get distinct OFDs and `flock` between them conflicts
//!    just as it does between processes. If AoE is ever ported to a
//!    platform whose underlying lock primitive is process-scoped (e.g.
//!    POSIX `fcntl(F_SETLK)` advisory locks, or certain Windows backends
//!    that key on the `HANDLE` rather than the open file description),
//!    this mutex becomes load-bearing and must not be removed without
//!    re-establishing intra-process exclusion.
//! 2. **Cross-process advisory `flock(2)`** on a sidecar lock file
//!    (`<profile_dir>/.storage.lock` for sessions+groups,
//!    `<app_dir>/.workspace-ordering.lock` for ordering). Sole guarantor
//!    of write serialisation; `atomic_write` separately guarantees that
//!    lock-free readers observe a consistent JSON document. Every mutator
//!    holds the flock from before `load` until after `atomic_write`.
//!    Polled `fs2::FileExt::try_lock_exclusive` with a 50ms backoff so
//!    that a wait longer than 1s fires a single `tracing::warn`; the
//!    kernel releases the lock on process exit, including SIGKILL, so a
//!    crashed peer cannot wedge other aoe processes. Mirrors the pattern
//!    already used by `recovery.rs` and `logging.rs`.
//!
//! All mutation goes through `update` (load -> mutate -> save under both
//! locks). `save_workspace_ordering` is private and only consumed by
//! `update_workspace_ordering` internally; the per-profile `save` /
//! `save_groups` helpers were removed entirely. This keeps it structurally
//! impossible to bypass the locks.
//!
//! Lock-ordering rule across the process: server callers MUST drop
//! `AppState.instances` (tokio RwLock) before acquiring `Storage`'s
//! per-profile mutex via `tokio::task::spawn_blocking(... storage.update)`.
//! The flock can park on a wedged peer for arbitrary time; holding the
//! tokio RwLock across the wait would block every other reader/writer of
//! `AppState.instances` and park the worker thread. The cross-process
//! `flock` is acquired AFTER the in-process mutex and released BEFORE it
//! (RAII drop order). The closure passed to `update` is
//! `FnOnce(...) -> Result<R>` and cannot await, so `std::sync::Mutex` is
//! safe across the body even on the tokio runtime.
//!
//! Closures must remain CPU/memory only (no network, no user input, no tmux
//! work). A closure that hangs holds both layers indefinitely and blocks
//! every peer process. The same hung-hook caveat documented in
//! `recovery.rs` applies here.
//!
//! `update_workspace_ordering` and `Storage::update` must NOT be called from
//! inside each other's closures. They use distinct lock files but acquiring
//! both in different orders across processes would deadlock cross-process.
//! Today no caller does this; this comment is the invariant.

use anyhow::{anyhow, Result};
use fs2::FileExt;
use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::file_watch::FileWatchService;

use super::{get_app_dir, get_profile_dir, Group, Instance};

/// Sidecar lock file name for per-profile storage. Lives next to
/// `sessions.json` and `groups.json` and covers both: every code path that
/// mutates them does so as a pair under the same in-process mutex, so a
/// single sidecar is sufficient and avoids any sub-file lock-ordering rule.
const STORAGE_LOCK_FILENAME: &str = ".storage.lock";

/// Sidecar lock file name for the global workspace-ordering file. Lives in
/// `<app_dir>` next to `workspace-ordering.json`.
const WORKSPACE_LOCK_FILENAME: &str = ".workspace-ordering.lock";

/// Emit a tracing warn if the cross-process `flock` is held by a peer for
/// longer than this. Surfaces a wedged peer in `aoe logs` instead of a
/// silent stall. The acquire itself blocks indefinitely; the warning is
/// observability only, not a timeout.
const FLOCK_WAIT_WARN_AFTER: Duration = Duration::from_secs(1);

/// Write `content` to `path` atomically (temp file + fsync + rename + dir fsync).
/// Existing perms are preserved; on a fresh file the result is tempfile's 0o600 default.
pub(crate) fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    let dir = path.parent().ok_or_else(|| {
        anyhow!(
            "atomic_write needs a path with a parent: {}",
            path.display()
        )
    })?;
    let existing_perms = fs::metadata(path).ok().map(|m| m.permissions());
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(content)?;
    tmp.as_file().sync_data()?;
    let file = tmp.persist(path)?;
    if let Some(perms) = existing_perms {
        file.set_permissions(perms)?;
    }
    // Best-effort dir fsync so the rename itself survives power loss.
    if let Ok(dir_file) = fs::File::open(dir) {
        let _ = dir_file.sync_all();
    }
    Ok(())
}

/// Process-wide registry of per-profile save mutexes. Every `Storage::new` for
/// a given profile name resolves to the same `Arc<Mutex<()>>`, so independent
/// `Storage` handles in different parts of the process serialise correctly.
fn save_lock_for(profile: &str) -> Arc<Mutex<()>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();
    let registry = REGISTRY.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = registry
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard
        .entry(profile.to_string())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// Dedicated lock for the global `workspace-ordering.json` file. Separate from
/// the per-profile registry because the file lives at the app-data root and is
/// shared across profiles.
fn workspace_ordering_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// RAII guard for a held cross-process `flock`. Drops via `fs2::FileExt::unlock`,
/// which is also performed by the kernel when the file descriptor is closed,
/// so a panic during the critical section still releases the lock.
struct StorageFlock {
    file: fs::File,
}

impl Drop for StorageFlock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

/// Acquire the cross-process advisory `flock` on `<dir>/<name>` by polling
/// `try_lock_exclusive` every 50ms until it is granted. Open semantics
/// mirror `recovery::try_acquire_recovery_lock` (read+write, create, no
/// truncate) and `logging.rs`'s rotation lock.
///
/// Polling instead of `lock_exclusive` is deliberate: `fs2` exposes no hook
/// to instrument a blocking acquire, and we need a single `tracing::warn`
/// after `FLOCK_WAIT_WARN_AFTER` so a wedged peer is observable in
/// `aoe logs`. The 50ms cadence is below human perception and far above any
/// realistic mutator's hold time.
///
/// On Unix the lock file is chmodded to `0o600` so it never widens beyond
/// the rest of `<app_dir>` regardless of the caller's umask. The kernel
/// releases the lock on process exit (including SIGKILL), so a crashed peer
/// cannot wedge us forever.
fn acquire_storage_flock(dir: &Path, name: &str) -> Result<StorageFlock> {
    fs::create_dir_all(dir)?;
    let path = dir.join(name);
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)?
    };
    #[cfg(not(unix))]
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;

    if let Err(e) = file.try_lock_exclusive() {
        if e.kind() != std::io::ErrorKind::WouldBlock {
            return Err(e.into());
        }
        let started = Instant::now();
        let mut warned = false;
        loop {
            match file.try_lock_exclusive() {
                Ok(()) => {
                    let waited = started.elapsed();
                    if waited >= FLOCK_WAIT_WARN_AFTER {
                        if warned {
                            tracing::info!(
                                target: "session.store",
                                ?waited,
                                path = %path.display(),
                                "storage flock acquired after wait"
                            );
                        } else {
                            tracing::warn!(
                                target: "session.store",
                                ?waited,
                                path = %path.display(),
                                "storage flock contended for >1s; another aoe process held it"
                            );
                        }
                    }
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if !warned && started.elapsed() >= FLOCK_WAIT_WARN_AFTER {
                        tracing::warn!(
                            target: "session.store",
                            path = %path.display(),
                            "storage flock contended for >1s; another aoe process is mid-write"
                        );
                        warned = true;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(StorageFlock { file })
}

pub struct Storage {
    profile: String,
    sessions_path: PathBuf,
    save_lock: Arc<Mutex<()>>,
    /// Used to surface in-process writes immediately to subscribers via the
    /// kernel-event-equivalent dispatcher path; see
    /// `FileWatchService::notify_local_change`. Cheap to clone (`Arc`).
    file_watch: Arc<FileWatchService>,
}

// Cross-device-syncable sidebar ordering. Workspaces are a client
// construct (a group of sessions keyed on `repoPath::branch` or
// `repoPath::__session__::session_id`), so the server treats the entries
// here as opaque strings. The list is a partial order: workspace ids not
// in the list fall back to the default newest-first ordering. Persisted
// globally (not per-profile) because the sidebar shows sessions across
// all profiles and a per-profile file would fragment the user's layout.
// See #1169.
#[derive(serde::Deserialize, serde::Serialize, Default)]
pub struct WorkspaceOrdering {
    pub order: Vec<String>,
}

impl Storage {
    pub fn new(profile: &str, file_watch: Arc<FileWatchService>) -> Result<Self> {
        let profile_name = if profile.is_empty() {
            super::config::resolve_default_profile()
        } else {
            profile.to_string()
        };

        let profile_dir = get_profile_dir(&profile_name)?;
        let sessions_path = profile_dir.join("sessions.json");
        let save_lock = save_lock_for(&profile_name);

        Ok(Self {
            profile: profile_name,
            sessions_path,
            save_lock,
            file_watch,
        })
    }

    /// Construct a `Storage` wired to a noop `FileWatchService`.
    ///
    /// Short-lived CLI subprocesses and integration-test writers pair with
    /// this constructor: they never drive the watcher loop, so the noop
    /// path keeps callers free of `FileWatchService::noop()` literals at
    /// every site. Production writers that need live in-process
    /// propagation must construct via `Storage::new` with the daemon's
    /// `Arc<FileWatchService>` instead.
    pub fn new_unwatched(profile: &str) -> Result<Self> {
        Self::new(profile, FileWatchService::noop())
    }

    pub fn profile(&self) -> &str {
        &self.profile
    }

    pub fn load(&self) -> Result<Vec<Instance>> {
        if !self.sessions_path.exists() {
            return Ok(Vec::new());
        }

        let content = fs::read_to_string(&self.sessions_path)?;
        if content.trim().is_empty() {
            return Ok(Vec::new());
        }

        // Two-phase parse: deserialise the outer array as opaque values
        // first, then attempt `Instance` per row. A single unparseable row
        // (forward-incompatible field, partial write, manual edit) degrades
        // to "that one session is missing" instead of locking the user out
        // of every session. Top-level corruption (not a valid JSON array)
        // still propagates as `Err` so it is never silently masked.
        let rows: Vec<serde_json::Value> = serde_json::from_str(&content)?;
        let mut instances = Vec::with_capacity(rows.len());
        let mut corrupt: Vec<serde_json::Value> = Vec::new();
        for (idx, row) in rows.into_iter().enumerate() {
            match <Instance as serde::Deserialize>::deserialize(&row) {
                Ok(mut inst) => {
                    inst.set_file_watch(self.file_watch.clone());
                    instances.push(inst);
                }
                Err(e) => {
                    tracing::warn!(
                        profile = %self.profile,
                        row = idx,
                        error = %e,
                        path = %self.sessions_path.display(),
                        "skipping corrupt session row"
                    );
                    corrupt.push(row);
                }
            }
        }

        if !corrupt.is_empty() {
            self.quarantine_corrupt_rows(&corrupt);
        }

        Ok(instances)
    }

    /// Write corrupt session rows to a sibling `sessions.corrupt.jsonl`
    /// quarantine file (one JSON object per line) for later inspection and
    /// manual recovery. Best-effort: a failure to write the sidecar is
    /// logged but never fails the load, since the whole point is to keep the
    /// surviving sessions reachable.
    ///
    /// Truncates rather than appends: `load()` runs on read-only refresh
    /// paths (TUI reconcile, web list, CLI) that never rewrite
    /// `sessions.json`, so a persistently corrupt row would otherwise be
    /// re-appended on every load and grow the sidecar without bound. Each
    /// load sees the full current corrupt set, so an overwrite is a
    /// complete, deduplicated snapshot.
    fn quarantine_corrupt_rows(&self, rows: &[serde_json::Value]) {
        let path = self.sessions_path.with_file_name("sessions.corrupt.jsonl");

        let mut buf = String::new();
        for row in rows {
            match serde_json::to_string(row) {
                Ok(line) => {
                    buf.push_str(&line);
                    buf.push('\n');
                }
                Err(e) => tracing::warn!(
                    error = %e,
                    "failed to serialise corrupt session row for quarantine"
                ),
            }
        }
        if buf.is_empty() {
            return;
        }

        // `atomic_write` (not `fs::write`) so the sidecar matches the
        // durability and privacy guarantees of `sessions.json`: a crash
        // mid-write cannot tear the only surviving copy of the lost row,
        // the file lands at 0o600 (it can echo tokens from
        // `Instance.command`) instead of umask-default 0o644, and the
        // unlocked, concurrently-reachable `load()` callers collapse to a
        // benign last-writer-wins instead of interleaving bytes.
        if let Err(e) = atomic_write(&path, buf.as_bytes()) {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                "failed to write session quarantine file"
            );
        }
    }

    pub fn load_with_groups(&self) -> Result<(Vec<Instance>, Vec<Group>)> {
        let instances = self.load()?;

        let groups_path = self.sessions_path.with_file_name("groups.json");
        let groups = if groups_path.exists() {
            let content = fs::read_to_string(&groups_path)?;
            if content.trim().is_empty() {
                Vec::new()
            } else {
                serde_json::from_str(&content)?
            }
        } else {
            Vec::new()
        };

        Ok((instances, groups))
    }

    /// Locked load -> mutate -> save. The closure receives mutable references
    /// to the current persisted state of `sessions.json` and `groups.json`.
    /// On `Ok` from the closure, both files are serialised before any disk
    /// write, so a serialisation failure on either side leaves both files
    /// untouched. Likewise, an `Err` from the closure leaves both files
    /// untouched. `groups.json` is only rewritten when the closure actually
    /// changed the groups vec (most callers only touch instances).
    ///
    /// `groups.json` is written first, `sessions.json` second. Per-file
    /// notify semantics: each `notify_local_change` call is gated by the
    /// preceding `atomic_write?`, so a notify on a path is surfaced only
    /// when that path's write returned `Ok`. A disk-level failure on the
    /// second `atomic_write` (after the first succeeded) can leave a torn
    /// pair: the new groups are persisted with the prior instances, the
    /// groups notify already fired, and `update()` returns `Err` without
    /// emitting a sessions notify. The torn-pair window is bounded by two
    /// `rename(2)` syscalls on sibling files and is tolerated by the
    /// loader (`GroupTree` accepts orphan group rows).
    ///
    /// This is the only public mutator entry point; all writes funnel
    /// through here so both lock layers are always taken.
    pub fn update<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut Vec<Instance>, &mut Vec<Group>) -> Result<R>,
    {
        let _mu = self
            .save_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let profile_dir = self.sessions_path.parent().ok_or_else(|| {
            anyhow!(
                "sessions_path missing parent: {}",
                self.sessions_path.display()
            )
        })?;
        let _flock = acquire_storage_flock(profile_dir, STORAGE_LOCK_FILENAME)?;
        let (mut instances, mut groups) = self.load_with_groups()?;
        let groups_before = groups.clone();
        let result = f(&mut instances, &mut groups)?;

        // Pre-serialise both buffers so a serde failure on either side
        // aborts before any file is touched.
        let instances_buf = serde_json::to_vec_pretty(&instances)?;
        let groups_changed = groups != groups_before;
        let groups_buf = if groups_changed {
            Some(serde_json::to_vec_pretty(&groups)?)
        } else {
            None
        };

        // groups first, sessions last: a torn pair leaves orphan groups
        // (loader-tolerant) rather than instances pointing at a missing
        // group_path.
        if let Some(buf) = groups_buf {
            let groups_path = self.sessions_path.with_file_name("groups.json");
            atomic_write(&groups_path, &buf)?;
            // Surface the rename to in-process subscribers immediately;
            // the kernel echo arrives ~ms later for the same rename and
            // collapses into the same per-key debounce slot. Runs strictly
            // AFTER atomic_write returns so any subscriber waking on the
            // notify is guaranteed to read the post-rename file.
            self.file_watch.notify_local_change(&groups_path);
        }
        atomic_write(&self.sessions_path, &instances_buf)?;
        self.file_watch.notify_local_change(&self.sessions_path);
        Ok(result)
    }
}

// Workspace ordering is stored at the app-data root, not per-profile:
// `list_sessions` returns sessions across all profiles, so the sidebar
// is a single global view and a per-profile file would only fragment
// the user's chosen layout. Workspace ids derive from `repoPath::branch`
// (or `repoPath::__session__::session_id`) and are profile-independent.
fn workspace_ordering_path() -> Result<PathBuf> {
    Ok(get_app_dir()?.join("workspace-ordering.json"))
}

pub fn load_workspace_ordering() -> Result<WorkspaceOrdering> {
    let path = workspace_ordering_path()?;
    if !path.exists() {
        return Ok(WorkspaceOrdering::default());
    }
    let content = fs::read_to_string(&path)?;
    if content.trim().is_empty() {
        return Ok(WorkspaceOrdering::default());
    }
    Ok(serde_json::from_str(&content)?)
}

/// Locked load -> mutate -> save for the global workspace ordering file.
/// On `Ok` from the closure, the file is rewritten atomically under the
/// dedicated workspace-ordering lock. On `Err`, the file is not touched.
pub fn update_workspace_ordering<F, R>(f: F) -> Result<R>
where
    F: FnOnce(&mut WorkspaceOrdering) -> Result<R>,
{
    let _mu = workspace_ordering_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let app_dir = get_app_dir()?;
    let _flock = acquire_storage_flock(&app_dir, WORKSPACE_LOCK_FILENAME)?;
    let mut ordering = load_workspace_ordering()?;
    let result = f(&mut ordering)?;
    save_workspace_ordering(&ordering)?;
    Ok(result)
}

fn save_workspace_ordering(ordering: &WorkspaceOrdering) -> Result<()> {
    let path = workspace_ordering_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(ordering)?;
    atomic_write(&path, content.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_watch::{FileMatcher, FileWatchService, WatchSpec};
    use crate::session::GroupTree;
    use serial_test::serial;
    use tempfile::tempdir;

    fn setup_test_home(temp: &std::path::Path) {
        std::env::set_var("HOME", temp);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp.join(".config"));
    }

    #[test]
    #[serial]
    fn test_storage_roundtrip() -> Result<()> {
        let temp = tempdir()?;
        setup_test_home(temp.path());

        let storage = Storage::new_unwatched("test-profile")?;

        let instances = vec![
            Instance::new("test1", "/tmp/test1"),
            Instance::new("test2", "/tmp/test2"),
        ];

        storage.update(|i, g| {
            *i = instances.to_vec();
            *g = GroupTree::new_with_groups(&instances, &[]).get_all_groups();
            Ok(())
        })?;
        let loaded = storage.load()?;

        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].title, "test1");
        assert_eq!(loaded[1].title, "test2");

        Ok(())
    }

    #[test]
    #[serial]
    fn test_load_skips_corrupt_row_and_quarantines() -> Result<()> {
        let temp = tempdir()?;
        setup_test_home(temp.path());
        let storage = Storage::new_unwatched("test-profile")?;

        // [ valid, malformed, valid ]: the malformed row is an object that
        // is missing `Instance`'s required `id`/`project_path` fields.
        let valid = [
            Instance::new("alpha", "/tmp/alpha"),
            Instance::new("beta", "/tmp/beta"),
        ];
        let mut rows: Vec<serde_json::Value> = valid
            .iter()
            .map(|i| serde_json::to_value(i).unwrap())
            .collect();
        rows.insert(1, serde_json::json!({ "title": "corrupt-no-id" }));

        fs::create_dir_all(storage.sessions_path.parent().unwrap())?;
        fs::write(&storage.sessions_path, serde_json::to_vec_pretty(&rows)?)?;

        let loaded = storage.load()?;
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].title, "alpha");
        assert_eq!(loaded[1].title, "beta");

        let quarantine = storage
            .sessions_path
            .with_file_name("sessions.corrupt.jsonl");
        assert!(quarantine.exists(), "quarantine sidecar should be created");
        let q = fs::read_to_string(&quarantine)?;
        assert_eq!(q.lines().count(), 1, "exactly one row quarantined");
        assert!(q.contains("corrupt-no-id"), "malformed row is preserved");

        // The sidecar can echo tokens carried in `Instance.command`, so it
        // must be written 0o600 like `sessions.json`, not umask-default 0o644.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&quarantine)?.permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "quarantine sidecar must be owner-only");
        }

        // A second read-only load must not duplicate the row: load() runs on
        // refresh paths that never rewrite sessions.json, so the sidecar is
        // overwritten with the current corrupt set rather than appended to.
        assert_eq!(storage.load()?.len(), 2);
        let q = fs::read_to_string(&quarantine)?;
        assert_eq!(q.lines().count(), 1, "repeated load must not duplicate");

        Ok(())
    }

    #[test]
    #[serial]
    fn test_load_top_level_corruption_still_errors() -> Result<()> {
        let temp = tempdir()?;
        setup_test_home(temp.path());
        let storage = Storage::new_unwatched("test-profile")?;

        fs::create_dir_all(storage.sessions_path.parent().unwrap())?;
        let quarantine = storage
            .sessions_path
            .with_file_name("sessions.corrupt.jsonl");

        // Both forms of top-level corruption must surface as Err and never be
        // masked by the per-row fallthrough: valid JSON of the wrong shape (an
        // object, not an array) and syntactically invalid JSON (a torn write).
        for bad in [&b"{}"[..], &b"{ this is not valid json ]"[..]] {
            fs::write(&storage.sessions_path, bad)?;
            assert!(
                storage.load().is_err(),
                "top-level corruption should still surface as Err"
            );
            assert!(
                !quarantine.exists(),
                "no quarantine file for top-level corruption"
            );
        }

        Ok(())
    }

    #[test]
    #[serial]
    fn test_storage_new_with_empty_profile_bootstraps() -> Result<()> {
        // On a fresh install with no profiles, an empty profile argument
        // resolves through `resolve_default_profile`, which bootstraps the
        // first profile. The name is "main", never the magic "default".
        let temp = tempdir()?;
        setup_test_home(temp.path());

        let storage = Storage::new_unwatched("")?;
        assert_eq!(storage.profile(), "main");
        Ok(())
    }

    #[test]
    #[serial]
    fn test_storage_new_with_empty_profile_uses_existing() -> Result<()> {
        // When profiles already exist, an empty profile argument resolves to
        // the first one (sorted), not a hard-coded name.
        let temp = tempdir()?;
        setup_test_home(temp.path());

        get_profile_dir("work")?;
        get_profile_dir("personal")?;

        let storage = Storage::new_unwatched("")?;
        assert_eq!(storage.profile(), "personal");
        Ok(())
    }

    #[test]
    #[serial]
    fn test_storage_new_with_empty_profile_honors_config() -> Result<()> {
        // An explicitly configured default_profile wins over the first-found
        // directory.
        let temp = tempdir()?;
        setup_test_home(temp.path());

        get_profile_dir("work")?;
        get_profile_dir("personal")?;
        let config = super::super::config::Config {
            default_profile: "work".to_string(),
            ..Default::default()
        };
        super::super::config::save_config(&config)?;

        let storage = Storage::new_unwatched("")?;
        assert_eq!(storage.profile(), "work");
        Ok(())
    }

    #[test]
    #[serial]
    fn test_storage_new_with_custom_profile() -> Result<()> {
        let temp = tempdir()?;
        setup_test_home(temp.path());

        let storage = Storage::new_unwatched("custom-profile")?;
        assert_eq!(storage.profile(), "custom-profile");
        Ok(())
    }

    #[test]
    #[serial]
    fn test_storage_load_nonexistent_file() -> Result<()> {
        let temp = tempdir()?;
        setup_test_home(temp.path());

        let storage = Storage::new_unwatched("test-empty")?;
        let loaded = storage.load()?;

        assert!(loaded.is_empty());
        Ok(())
    }

    #[test]
    #[serial]
    fn test_storage_load_empty_file() -> Result<()> {
        let temp = tempdir()?;
        setup_test_home(temp.path());

        let storage = Storage::new_unwatched("test-empty-file")?;

        // Create empty file
        fs::create_dir_all(storage.sessions_path.parent().unwrap())?;
        fs::write(&storage.sessions_path, "")?;

        let loaded = storage.load()?;
        assert!(loaded.is_empty());
        Ok(())
    }

    #[test]
    #[serial]
    fn test_storage_load_whitespace_only_file() -> Result<()> {
        let temp = tempdir()?;
        setup_test_home(temp.path());

        let storage = Storage::new_unwatched("test-whitespace")?;

        fs::create_dir_all(storage.sessions_path.parent().unwrap())?;
        fs::write(&storage.sessions_path, "   \n  \t  ")?;

        let loaded = storage.load()?;
        assert!(loaded.is_empty());
        Ok(())
    }

    #[test]
    #[serial]
    fn test_storage_save_leaves_no_temp_files() -> Result<()> {
        let temp = tempdir()?;
        setup_test_home(temp.path());

        let storage = Storage::new_unwatched("test-no-debris")?;

        for i in 0..5 {
            let instances = vec![Instance::new(&format!("iter{i}"), "/tmp/test")];
            storage.update(|i, g| {
                *i = instances.to_vec();
                *g = GroupTree::new_with_groups(&instances, &[]).get_all_groups();
                Ok(())
            })?;
        }

        let dir = storage.sessions_path.parent().unwrap();
        let entries: Vec<_> = fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();

        for entry in &entries {
            assert!(
                !entry.contains(".tmp"),
                "atomic_write must not leak temp files; found {}",
                entry
            );
        }
        assert!(entries.contains(&"sessions.json".to_string()));
        Ok(())
    }

    #[test]
    #[serial]
    fn test_storage_save_empty_array() -> Result<()> {
        let temp = tempdir()?;
        setup_test_home(temp.path());

        let storage = Storage::new_unwatched("test-empty-save")?;
        {
            let xs: Vec<Instance> = vec![];
            storage.update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })?
        };

        let content = fs::read_to_string(&storage.sessions_path)?;
        assert_eq!(content.trim(), "[]");
        Ok(())
    }

    #[test]
    #[serial]
    fn test_storage_load_with_groups_no_groups_file() -> Result<()> {
        let temp = tempdir()?;
        setup_test_home(temp.path());

        let storage = Storage::new_unwatched("test-no-groups")?;

        let instances = vec![Instance::new("test", "/tmp/test")];
        storage.update(|i, g| {
            *i = instances.to_vec();
            *g = GroupTree::new_with_groups(&instances, &[]).get_all_groups();
            Ok(())
        })?;

        let (loaded_instances, loaded_groups) = storage.load_with_groups()?;
        assert_eq!(loaded_instances.len(), 1);
        assert!(loaded_groups.is_empty());
        Ok(())
    }

    #[test]
    #[serial]
    fn test_storage_save_and_load_with_groups() -> Result<()> {
        let temp = tempdir()?;
        setup_test_home(temp.path());

        let storage = Storage::new_unwatched("test-with-groups")?;

        let mut instances = vec![Instance::new("test", "/tmp/test")];
        instances[0].group_path = "work/projects".to_string();

        let groups = vec![Group::new("projects", "work/projects")];
        let group_tree = GroupTree::new_with_groups(&instances, &groups);

        storage.update(|i, g| {
            *i = instances.to_vec();
            *g = group_tree.get_all_groups();
            Ok(())
        })?;

        let (loaded_instances, loaded_groups) = storage.load_with_groups()?;
        assert_eq!(loaded_instances.len(), 1);
        assert_eq!(loaded_instances[0].group_path, "work/projects");
        assert!(!loaded_groups.is_empty());
        Ok(())
    }

    #[test]
    #[serial]
    fn test_storage_load_invalid_json() -> Result<()> {
        let temp = tempdir()?;
        setup_test_home(temp.path());

        let storage = Storage::new_unwatched("test-invalid")?;

        fs::create_dir_all(storage.sessions_path.parent().unwrap())?;
        fs::write(&storage.sessions_path, "{ invalid json }")?;

        let result = storage.load();
        assert!(result.is_err());
        Ok(())
    }

    #[test]
    #[serial]
    fn test_storage_preserves_instance_fields() -> Result<()> {
        let temp = tempdir()?;
        setup_test_home(temp.path());

        let storage = Storage::new_unwatched("test-fields")?;

        let mut instance = Instance::new("Test Project", "/home/user/project");
        instance.tool = "opencode".to_string();
        instance.command = "opencode --config test".to_string();
        instance.group_path = "work/clients".to_string();

        {
            let xs: Vec<Instance> = vec![instance.clone()];
            storage.update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })?
        };
        let loaded = storage.load()?;

        assert_eq!(loaded.len(), 1);
        let loaded_instance = &loaded[0];
        assert_eq!(loaded_instance.title, "Test Project");
        assert_eq!(loaded_instance.project_path, "/home/user/project");
        assert_eq!(loaded_instance.tool, "opencode");
        assert_eq!(loaded_instance.command, "opencode --config test");
        assert_eq!(loaded_instance.group_path, "work/clients");
        Ok(())
    }

    #[test]
    #[serial]
    fn test_storage_profile_accessor() -> Result<()> {
        let temp = tempdir()?;
        setup_test_home(temp.path());

        // Verify profiles are correctly named
        let storage1 = Storage::new_unwatched("profile-alpha")?;
        let storage2 = Storage::new_unwatched("profile-beta")?;

        assert_eq!(storage1.profile(), "profile-alpha");
        assert_eq!(storage2.profile(), "profile-beta");

        // Verify they use different paths (implying isolation)
        assert_ne!(storage1.sessions_path, storage2.sessions_path);
        Ok(())
    }

    #[test]
    #[serial]
    fn test_storage_groups_file_empty() -> Result<()> {
        let temp = tempdir()?;
        setup_test_home(temp.path());

        let storage = Storage::new_unwatched("test-empty-groups")?;

        // Save sessions
        {
            let xs: Vec<Instance> = vec![Instance::new("test", "/tmp/test")];
            storage.update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })?
        };

        // Create empty groups file
        let groups_path = storage.sessions_path.with_file_name("groups.json");
        fs::write(&groups_path, "   ")?;

        let (instances, groups) = storage.load_with_groups()?;
        assert_eq!(instances.len(), 1);
        assert!(groups.is_empty());
        Ok(())
    }

    #[test]
    #[serial]
    fn test_workspace_ordering_roundtrip() -> Result<()> {
        let temp = tempdir()?;
        setup_test_home(temp.path());

        // Empty by default.
        let empty = load_workspace_ordering()?;
        assert!(empty.order.is_empty());

        let saved = WorkspaceOrdering {
            order: vec![
                "/repo/a::main".to_string(),
                "/repo/b::feature/x".to_string(),
                "/repo/c::__session__::abc123".to_string(),
            ],
        };
        save_workspace_ordering(&saved)?;

        let loaded = load_workspace_ordering()?;
        assert_eq!(loaded.order, saved.order);
        Ok(())
    }

    #[test]
    #[serial]
    fn test_workspace_ordering_overwrites_on_save() -> Result<()> {
        let temp = tempdir()?;
        setup_test_home(temp.path());

        save_workspace_ordering(&WorkspaceOrdering {
            order: vec!["a".to_string(), "b".to_string()],
        })?;
        save_workspace_ordering(&WorkspaceOrdering {
            order: vec!["b".to_string()],
        })?;

        let loaded = load_workspace_ordering()?;
        assert_eq!(loaded.order, vec!["b".to_string()]);
        Ok(())
    }

    #[test]
    #[serial]
    fn test_workspace_ordering_handles_empty_file() -> Result<()> {
        let temp = tempdir()?;
        setup_test_home(temp.path());

        let path = workspace_ordering_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, "   ")?;

        let loaded = load_workspace_ordering()?;
        assert!(loaded.order.is_empty());
        Ok(())
    }

    #[test]
    #[serial]
    fn test_update_atomic_load_modify_save() -> Result<()> {
        let temp = tempdir()?;
        setup_test_home(temp.path());

        let storage = Storage::new_unwatched("test-update-roundtrip")?;
        storage.update(|i, g| {
            *i = [Instance::new("seed", "/tmp/seed")].to_vec();
            *g = GroupTree::new_with_groups(&[], &[]).get_all_groups();
            Ok(())
        })?;

        storage.update(|instances, _groups| {
            instances.push(Instance::new("added", "/tmp/added"));
            Ok(())
        })?;

        let loaded = storage.load()?;
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].title, "seed");
        assert_eq!(loaded[1].title, "added");
        Ok(())
    }

    #[test]
    #[serial]
    fn test_update_propagates_closure_error() -> Result<()> {
        let temp = tempdir()?;
        setup_test_home(temp.path());

        let storage = Storage::new_unwatched("test-update-err")?;
        let initial = vec![Instance::new("keep", "/tmp/keep")];
        storage.update(|i, g| {
            *i = initial.to_vec();
            *g = GroupTree::new_with_groups(&initial, &[]).get_all_groups();
            Ok(())
        })?;

        let result: Result<()> = storage.update(|instances, _| {
            instances.push(Instance::new("doomed", "/tmp/doomed"));
            Err(anyhow!("forced abort"))
        });
        assert!(result.is_err());

        let loaded = storage.load()?;
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].title, "keep");
        Ok(())
    }

    #[test]
    #[serial]
    fn test_update_serializes_concurrent_writers_same_profile() -> Result<()> {
        let temp = tempdir()?;
        setup_test_home(temp.path());

        let storage = Storage::new_unwatched("test-update-concurrent")?;
        storage.update(|i, g| {
            *i = [].to_vec();
            *g = GroupTree::new_with_groups(&[], &[]).get_all_groups();
            Ok(())
        })?;

        let n_threads = 32usize;
        std::thread::scope(|scope| {
            for tid in 0..n_threads {
                scope.spawn(move || {
                    let storage = Storage::new_unwatched("test-update-concurrent").unwrap();
                    storage
                        .update(|instances, _| {
                            instances.push(Instance::new(
                                &format!("inst-{tid}"),
                                &format!("/tmp/inst-{tid}"),
                            ));
                            Ok(())
                        })
                        .unwrap();
                });
            }
        });

        let loaded = storage.load()?;
        assert_eq!(
            loaded.len(),
            n_threads,
            "lost updates: expected {n_threads}, got {}",
            loaded.len()
        );
        let mut titles: Vec<_> = loaded.iter().map(|i| i.title.clone()).collect();
        titles.sort();
        for tid in 0..n_threads {
            assert!(
                titles.contains(&format!("inst-{tid}")),
                "missing inst-{tid}"
            );
        }
        Ok(())
    }

    #[test]
    #[serial]
    fn test_update_does_not_serialize_across_profiles() -> Result<()> {
        let temp = tempdir()?;
        setup_test_home(temp.path());

        let storage_a = Storage::new_unwatched("test-update-profile-a")?;
        let storage_b = Storage::new_unwatched("test-update-profile-b")?;

        std::thread::scope(|scope| {
            scope.spawn(|| {
                storage_a
                    .update(|instances, _| {
                        instances.push(Instance::new("a1", "/tmp/a1"));
                        Ok(())
                    })
                    .unwrap();
            });
            scope.spawn(|| {
                storage_b
                    .update(|instances, _| {
                        instances.push(Instance::new("b1", "/tmp/b1"));
                        Ok(())
                    })
                    .unwrap();
            });
        });

        assert_eq!(storage_a.load()?.len(), 1);
        assert_eq!(storage_b.load()?.len(), 1);
        Ok(())
    }

    #[test]
    #[serial]
    fn test_update_takes_same_lock_across_threads() -> Result<()> {
        use std::sync::Barrier;
        use std::time::{Duration, Instant};

        let temp = tempdir()?;
        setup_test_home(temp.path());

        let storage = Storage::new_unwatched("test-commit-lock")?;
        storage.update(|i, g| {
            *i = [].to_vec();
            *g = GroupTree::new_with_groups(&[], &[]).get_all_groups();
            Ok(())
        })?;

        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let entered_clone = Arc::clone(&entered);
        let release_clone = Arc::clone(&release);

        let updater = std::thread::spawn(move || {
            let storage = Storage::new_unwatched("test-commit-lock").unwrap();
            storage
                .update(|instances, _| {
                    instances.push(Instance::new("from-update", "/tmp/u"));
                    entered_clone.wait();
                    release_clone.wait();
                    Ok(())
                })
                .unwrap();
        });

        entered.wait();
        let start = Instant::now();
        let committer = std::thread::spawn(|| {
            let storage = Storage::new_unwatched("test-commit-lock").unwrap();
            storage
                .update(|i, g| {
                    *i = [Instance::new("from-commit", "/tmp/c")].to_vec();
                    *g = GroupTree::new_with_groups(&[], &[]).get_all_groups();
                    Ok(())
                })
                .unwrap();
        });

        std::thread::sleep(Duration::from_millis(80));
        assert!(
            !committer.is_finished(),
            "commit should be blocked by update's lock"
        );
        release.wait();
        updater.join().unwrap();
        committer.join().unwrap();

        assert!(
            start.elapsed() >= Duration::from_millis(50),
            "commit returned suspiciously fast"
        );

        let loaded = storage.load()?;
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].title, "from-commit");
        Ok(())
    }

    #[test]
    #[serial]
    fn test_workspace_ordering_update_serializes() -> Result<()> {
        let temp = tempdir()?;
        setup_test_home(temp.path());

        update_workspace_ordering(|ord| {
            ord.order.clear();
            Ok(())
        })?;

        let n_threads = 16usize;
        std::thread::scope(|scope| {
            for tid in 0..n_threads {
                scope.spawn(move || {
                    update_workspace_ordering(|ord| {
                        ord.order.push(format!("ws-{tid}"));
                        Ok(())
                    })
                    .unwrap();
                });
            }
        });

        let loaded = load_workspace_ordering()?;
        assert_eq!(loaded.order.len(), n_threads);
        for tid in 0..n_threads {
            assert!(
                loaded.order.contains(&format!("ws-{tid}")),
                "missing ws-{tid}"
            );
        }
        Ok(())
    }

    #[test]
    #[serial]
    fn test_profile_lock_registry_returns_same_arc_for_same_profile() -> Result<()> {
        let temp = tempdir()?;
        setup_test_home(temp.path());

        let s1 = Storage::new_unwatched("test-registry-shared")?;
        let s2 = Storage::new_unwatched("test-registry-shared")?;
        assert!(Arc::ptr_eq(&s1.save_lock, &s2.save_lock));

        let s3 = Storage::new_unwatched("test-registry-distinct")?;
        assert!(!Arc::ptr_eq(&s1.save_lock, &s3.save_lock));
        Ok(())
    }

    #[test]
    #[serial]
    fn test_update_writes_both_sessions_and_groups_files() -> Result<()> {
        let temp = tempdir()?;
        setup_test_home(temp.path());

        let storage = Storage::new_unwatched("test-update-both-files")?;
        storage.update(|i, g| {
            *i = [].to_vec();
            *g = GroupTree::new_with_groups(&[], &[]).get_all_groups();
            Ok(())
        })?;

        storage.update(|instances, groups| {
            instances.push(Instance::new("inst", "/tmp/inst"));
            groups.push(Group::new("projects", "work/projects"));
            Ok(())
        })?;

        let groups_path = storage.sessions_path.with_file_name("groups.json");
        assert!(groups_path.exists(), "groups.json should exist");

        let (loaded_instances, loaded_groups) = storage.load_with_groups()?;
        assert_eq!(loaded_instances.len(), 1);
        assert_eq!(loaded_groups.len(), 1);
        assert_eq!(loaded_groups[0].name, "projects");
        Ok(())
    }

    #[test]
    #[serial]
    fn test_update_closure_err_leaves_both_files_untouched() -> Result<()> {
        let temp = tempdir()?;
        setup_test_home(temp.path());

        let storage = Storage::new_unwatched("test-update-err-untouched")?;
        let seed = vec![Instance::new("seed", "/tmp/seed")];
        let seed_groups = vec![Group::new("seed-group", "work/seed")];
        let mut tree = GroupTree::new_with_groups(&seed, &seed_groups);
        tree.create_group("work/seed");
        storage.update(|i, g| {
            *i = seed.to_vec();
            *g = tree.get_all_groups();
            Ok(())
        })?;

        let groups_path = storage.sessions_path.with_file_name("groups.json");
        let sessions_before = fs::read(&storage.sessions_path)?;
        let groups_before = fs::read(&groups_path)?;

        let outcome: Result<()> = storage.update(|instances, groups| {
            instances.push(Instance::new("doomed-inst", "/tmp/doomed"));
            groups.push(Group::new("doomed-group", "doomed/path"));
            Err(anyhow!("forced abort"))
        });
        assert!(outcome.is_err());

        assert_eq!(fs::read(&storage.sessions_path)?, sessions_before);
        assert_eq!(fs::read(&groups_path)?, groups_before);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn test_update_write_failure_emits_no_notify() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempdir()?;
        setup_test_home(temp.path());

        let svc = FileWatchService::new().expect("live svc");
        let storage = Storage::new("test-update-no-notify", svc.clone())?;
        storage.update(|instances, _groups| {
            *instances = vec![Instance::new("seed", "/tmp/seed")];
            Ok(())
        })?;

        let profile_dir = get_profile_dir("test-update-no-notify")?;
        let sessions_path = profile_dir.join("sessions.json");
        let groups_path = profile_dir.join("groups.json");
        let (mut sessions_rx, _sessions_h) = svc
            .subscribe_channel(
                WatchSpec {
                    dir: profile_dir.clone(),
                    matcher: FileMatcher::Exact(sessions_path),
                    debounce: None,
                },
                4,
            )
            .expect("subscribe sessions");
        let (mut groups_rx, _groups_h) = svc
            .subscribe_channel(
                WatchSpec {
                    dir: profile_dir.clone(),
                    matcher: FileMatcher::Exact(groups_path),
                    debounce: None,
                },
                4,
            )
            .expect("subscribe groups");

        while tokio::time::timeout(std::time::Duration::from_millis(400), sessions_rx.recv())
            .await
            .is_ok()
        {}
        while tokio::time::timeout(std::time::Duration::from_millis(50), groups_rx.recv())
            .await
            .is_ok()
        {}

        let original_mode = fs::metadata(&profile_dir)?.permissions().mode();
        let mut readonly = fs::metadata(&profile_dir)?.permissions();
        readonly.set_mode(0o500);
        fs::set_permissions(&profile_dir, readonly)?;

        let update_res = storage.update(|instances, groups| {
            instances.push(Instance::new("late", "/tmp/late"));
            groups.push(Group::new("late-group", "/tmp/late-group"));
            Ok(())
        });

        let mut restore = fs::metadata(&profile_dir)?.permissions();
        restore.set_mode(original_mode);
        fs::set_permissions(&profile_dir, restore)?;

        assert!(update_res.is_err(), "write failure must surface as Err");

        let sessions_recv =
            tokio::time::timeout(std::time::Duration::from_millis(150), sessions_rx.recv()).await;
        assert!(
            sessions_recv.is_err() || matches!(sessions_recv, Ok(None)),
            "failed update must not emit a sessions notify_local_change delivery"
        );
        let groups_recv =
            tokio::time::timeout(std::time::Duration::from_millis(150), groups_rx.recv()).await;
        assert!(
            groups_recv.is_err() || matches!(groups_recv, Ok(None)),
            "failed update must not emit a groups notify_local_change delivery either; per-file gating means a write that never returned Ok must not fire its notify"
        );
        Ok(())
    }

    #[test]
    #[serial]
    fn test_update_skips_groups_write_when_groups_unchanged() -> Result<()> {
        let temp = tempdir()?;
        setup_test_home(temp.path());

        let storage = Storage::new_unwatched("test-skip-groups-write")?;
        let seed_instances = [Instance::new("seed", "/tmp/seed")];
        storage.update(|i, g| {
            *i = seed_instances.to_vec();
            g.push(Group::new("seed-group", "seed-group"));
            Ok(())
        })?;

        let groups_path = storage.sessions_path.with_file_name("groups.json");
        let groups_mtime_before = fs::metadata(&groups_path)?.modified()?;

        std::thread::sleep(std::time::Duration::from_millis(10));

        storage.update(|instances, _groups| {
            instances.push(Instance::new("added", "/tmp/added"));
            Ok(())
        })?;

        let groups_mtime_after = fs::metadata(&groups_path)?.modified()?;
        assert_eq!(
            groups_mtime_before, groups_mtime_after,
            "groups.json should not be rewritten when closure does not mutate groups"
        );
        Ok(())
    }

    #[test]
    #[serial]
    fn test_update_rewrites_groups_when_changed() -> Result<()> {
        let temp = tempdir()?;
        setup_test_home(temp.path());

        let storage = Storage::new_unwatched("test-rewrite-groups")?;
        let seed_instances = [Instance::new("seed", "/tmp/seed")];
        storage.update(|i, g| {
            *i = seed_instances.to_vec();
            g.push(Group::new("seed-group", "seed-group"));
            Ok(())
        })?;

        let groups_path = storage.sessions_path.with_file_name("groups.json");
        let groups_mtime_before = fs::metadata(&groups_path)?.modified()?;

        std::thread::sleep(std::time::Duration::from_millis(10));

        storage.update(|_instances, groups| {
            groups.push(Group::new("new-group", "work/new-group"));
            Ok(())
        })?;

        let groups_mtime_after = fs::metadata(&groups_path)?.modified()?;
        assert_ne!(
            groups_mtime_before, groups_mtime_after,
            "groups.json should be rewritten when closure mutates groups"
        );
        Ok(())
    }

    #[test]
    #[serial]
    fn test_save_lock_registry_recovers_from_poison() -> Result<()> {
        let temp = tempdir()?;
        setup_test_home(temp.path());

        let storage_outer = Storage::new_unwatched("test-poison-recovery")?;
        let _ = std::thread::spawn(move || {
            let _ = storage_outer.update(|_instances, _groups| -> Result<()> {
                panic!("forced poison");
            });
        })
        .join();

        let storage_after = Storage::new_unwatched("test-poison-recovery")?;
        storage_after.update(|instances, _groups| {
            instances.push(Instance::new("after-poison", "/tmp/after"));
            Ok(())
        })?;

        let loaded = storage_after.load()?;
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].title, "after-poison");
        Ok(())
    }
}
