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

use anyhow::{anyhow, Context, Result};
use fs2::FileExt;
use std::collections::HashMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::file_watch::FileWatchService;

use super::{
    get_app_dir, get_profile_dir, get_profile_dir_path, resolve_existing_profile, Group, Instance,
};

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

/// Resolve `path` through a symlink chain to the underlying target file. Used
/// for user-facing config files where users symlink to a dotfiles repo:
/// `rename(2)` would otherwise replace the symlink instead of updating the
/// target, silently desyncing the dotfile tree.
///
/// Returns `path` unchanged when it is not a symlink. When the chain ends in
/// a missing target (fresh install or dangling link), returns that target
/// path so the caller materialises a regular file there. Caps recursion at
/// 32 hops, well below typical kernel MAXSYMLINKS limits (40 on Linux, 32 on
/// Darwin); deeper chains are almost certainly loops.
pub(crate) fn resolve_symlink_chain(path: &Path) -> Result<PathBuf> {
    let mut current = path.to_path_buf();
    let mut hops: usize = 0;
    loop {
        match fs::symlink_metadata(&current) {
            Ok(metadata) if !metadata.file_type().is_symlink() => return Ok(current),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(current),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Failed to inspect {}", current.display()));
            }
            Ok(_) => {}
        }

        if hops >= 32 {
            return Err(anyhow!("Symlink chain too deep: {}", path.display()));
        }

        let target = fs::read_link(&current)
            .with_context(|| format!("Failed to read symlink {}", current.display()))?;
        current = if target.is_absolute() {
            target
        } else {
            current
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(target)
        };
        hops += 1;
    }
}

/// Like [`atomic_write`], but resolves any symlink at `path` first and writes
/// through to the underlying file. Use for user-facing agent config files
/// where users may symlink the path into a dotfiles repo (chezmoi, stow,
/// dotbot). Plain [`atomic_write`] would replace the symlink with a regular
/// file via `rename(2)`, silently desyncing the dotfile tree.
pub(crate) fn atomic_write_following_symlinks(path: &Path, content: &[u8]) -> Result<()> {
    atomic_write(&resolve_symlink_chain(path)?, content)
}

/// Serialized read-modify-write of a small standalone data file.
///
/// Acquires an exclusive cross-process `flock` on a sidecar
/// (`<dir>/.<file>.lock`), then, under the lock: reads `path` (missing or
/// blank content parses to `T::default()`), runs `mutate`, and persists the
/// result via [`atomic_write`]. The sidecar is deliberate: `atomic_write`
/// replaces the data file by `rename(2)`, which would leave a lock taken on
/// the data file itself attached to the orphaned inode, letting the next
/// writer lock the new inode concurrently.
///
/// The file lands owner-only (0o600) on Unix: a fresh file gets tempfile's
/// 0o600 default via `atomic_write`, and pre-existing files are re-tightened
/// because some callers store secrets (e.g. `mcp_state.json`).
///
/// When `mutate` returns `Err`, the file is left untouched and the error
/// comes back in the inner `Result`; a mutation may modify `T` before
/// noticing it must fail, and persisting that half-applied state would
/// destroy data the caller never meant to touch. The outer `Result` carries
/// lock, parse, and write failures.
///
/// Symlinks at `path` are resolved up front and everything (sidecar lock,
/// read, write) operates on the target: users symlink these files into
/// dotfile repos, and a rename over the symlink would replace it with a
/// regular file; locking the target also keeps two processes that reach the
/// same file through different symlink paths mutually exclusive.
pub(crate) fn locked_update<T, R, E>(
    path: &Path,
    parse: impl FnOnce(&str) -> Result<T>,
    serialize: impl FnOnce(&T) -> Result<String>,
    mutate: impl FnOnce(&mut T) -> std::result::Result<R, E>,
) -> Result<std::result::Result<R, E>>
where
    T: Default,
{
    let path = &resolve_symlink_chain(path)?;
    let dir = path.parent().ok_or_else(|| {
        anyhow!(
            "locked_update needs a path with a parent: {}",
            path.display()
        )
    })?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("locked_update needs a file path: {}", path.display()))?;
    let lock_name = format!(".{}.lock", file_name.to_string_lossy());
    let _flock = acquire_storage_flock(dir, &lock_name)?;

    let mut value = match fs::read_to_string(path) {
        Ok(content) if content.trim().is_empty() => T::default(),
        Ok(content) => parse(&content)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => T::default(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };

    let result = mutate(&mut value);
    if result.is_ok() {
        atomic_write(path, serialize(&value)?.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        }
    }
    Ok(result)
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
pub(crate) struct StorageFlock {
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
pub(crate) fn acquire_storage_flock(dir: &Path, name: &str) -> Result<StorageFlock> {
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

    #[cfg(test)]
    pub(crate) fn new_for_test_path(profile: &str, sessions_path: PathBuf) -> Self {
        Self {
            profile: profile.to_string(),
            sessions_path,
            save_lock: save_lock_for(profile),
            file_watch: FileWatchService::noop(),
        }
    }

    /// Construct a `Storage` for an existing profile, never creating it.
    ///
    /// Use this instead of [`Storage::new`] anywhere the caller is
    /// referencing a profile rather than birthing one (every CLI read/write
    /// path except the one that creates a brand-new session): resolving an
    /// unknown `-p <name>` through `new`'s `get_profile_dir` silently
    /// materializes an empty `profiles/<name>/` directory as a side effect
    /// of the read.
    pub fn open(profile: &str, file_watch: Arc<FileWatchService>) -> Result<Self> {
        let profile_name = resolve_existing_profile(profile)?;
        let profile_dir = get_profile_dir_path(&profile_name)?;
        let sessions_path = profile_dir.join("sessions.json");
        let save_lock = save_lock_for(&profile_name);

        Ok(Self {
            profile: profile_name,
            sessions_path,
            save_lock,
            file_watch,
        })
    }

    /// [`Storage::open`] wired to a noop `FileWatchService`. See
    /// [`Storage::new_unwatched`] for why CLI subprocesses want the noop
    /// watcher.
    pub fn open_unwatched(profile: &str) -> Result<Self> {
        Self::open(profile, FileWatchService::noop())
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

    fn quarantine_corrupt_rows(&self, rows: &[serde_json::Value]) {
        let path = self.sessions_path.with_file_name("sessions.corrupt.jsonl");
        Self::write_corrupt_rows_quarantine(&path, rows, "session");
    }

    fn quarantine_corrupt_group_rows(&self, rows: &[serde_json::Value]) {
        let path = self.sessions_path.with_file_name("groups.corrupt.jsonl");
        Self::write_corrupt_rows_quarantine(&path, rows, "group");
    }

    /// Write corrupt rows to a sibling quarantine sidecar for later inspection
    /// and manual recovery. Each line preserves one original JSON value; rows
    /// are not limited to objects because a malformed element can be any JSON
    /// value. Best-effort: a failure to write the sidecar is logged but never
    /// fails the load, since the whole point is to keep surviving sessions and
    /// groups reachable.
    ///
    /// Truncates rather than appends: load paths can run on read-only refresh
    /// flows (TUI reconcile, web list, CLI) that never rewrite the source JSON,
    /// so a persistently corrupt row would otherwise be re-appended on every
    /// load and grow the sidecar without bound. Each load sees the full current
    /// corrupt set, so an overwrite is a complete, deduplicated snapshot.
    fn write_corrupt_rows_quarantine(path: &Path, rows: &[serde_json::Value], row_kind: &str) {
        let mut buf = String::new();
        for row in rows {
            match serde_json::to_string(row) {
                Ok(line) => {
                    buf.push_str(&line);
                    buf.push('\n');
                }
                Err(e) => tracing::warn!(
                    error = %e,
                    row_kind = %row_kind,
                    "failed to serialise corrupt row for quarantine"
                ),
            }
        }
        if buf.is_empty() {
            return;
        }

        // `atomic_write` (not `fs::write`) so the sidecar matches the
        // durability and privacy guarantees of the source JSON file: a crash
        // mid-write cannot tear the only surviving copy of the lost row, fresh
        // sidecars land at 0o600 while existing permissions are preserved, and
        // concurrently-reachable read callers collapse to a benign
        // last-writer-wins instead of interleaving bytes.
        if let Err(e) = atomic_write(path, buf.as_bytes()) {
            tracing::warn!(
                error = %e,
                path = %path.display(),
                row_kind = %row_kind,
                "failed to write quarantine file"
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
                let rows: Vec<serde_json::Value> = serde_json::from_str(&content)?;
                let mut groups = Vec::with_capacity(rows.len());
                let mut corrupt: Vec<serde_json::Value> = Vec::new();
                for (idx, row) in rows.into_iter().enumerate() {
                    match <Group as serde::Deserialize>::deserialize(&row) {
                        Ok(group) => groups.push(group),
                        Err(e) => {
                            tracing::warn!(
                                profile = %self.profile,
                                row = idx,
                                error = %e,
                                path = %groups_path.display(),
                                "skipping corrupt group row"
                            );
                            corrupt.push(row);
                        }
                    }
                }

                if !corrupt.is_empty() {
                    self.quarantine_corrupt_group_rows(&corrupt);
                }

                groups
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

// Recent projects is a global most-recently-used store, written when a
// session is deleted so the project it lived in survives in the new-session
// wizard's Recent tab after its last session is gone (#2141). Live projects
// still come from the session list directly; this file is only the tombstone
// + recency for projects that no longer have any session. Stored at the
// app-data root for the same cross-profile reason as workspace ordering.
const RECENT_PROJECTS_LOCK_FILENAME: &str = ".recent-projects.lock";
const RECENT_PROJECTS_CAP: usize = 20;

fn recent_projects_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn recent_projects_path() -> Result<PathBuf> {
    Ok(get_app_dir()?.join("recent-projects.json"))
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug, PartialEq)]
pub struct RecentProjectEntry {
    pub path: String,
    pub display_name: String,
    pub tool: String,
    /// RFC 3339, always UTC, so lexical order equals chronological order.
    pub last_used_at: String,
}

#[derive(serde::Deserialize, serde::Serialize, Default)]
struct RecentProjects {
    projects: Vec<RecentProjectEntry>,
}

/// Build a recent-project entry from a session being deleted, or `None` for
/// sessions that must never appear in the wizard Recent list: scratch
/// sessions (transient dirs) and multi-repo workspaces (they collapse to a
/// single path and re-selecting one would silently drop the other repos).
/// Mirrors the web client filter in `ProjectStep.tsx::collectRecentProjects`.
/// The path is the worktree's main repo when present, else the project path,
/// with any trailing slash trimmed so it keys identically to the client.
pub fn recent_project_entry_for(inst: &Instance) -> Option<RecentProjectEntry> {
    if inst.scratch || inst.workspace_info.is_some() {
        return None;
    }
    let raw = inst
        .worktree_info
        .as_ref()
        .map(|w| w.main_repo_path.as_str())
        .unwrap_or(inst.project_path.as_str());
    let trimmed = raw.trim_end_matches(['/', '\\']);
    let path = if trimmed.is_empty() { "/" } else { trimmed };
    // `file_name` resolves the basename with the host platform's separator
    // rules, so a Windows path like `C:\repo\proj` yields `proj` rather than
    // the whole string. Falls back to the path itself for roots (`/`, `C:\`).
    let display_name = std::path::Path::new(path)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(path)
        .to_string();
    let last_used_at = inst
        .last_accessed_at
        .unwrap_or(inst.created_at)
        .to_rfc3339();
    Some(RecentProjectEntry {
        path: path.to_string(),
        display_name,
        tool: inst.tool.clone(),
        last_used_at,
    })
}

/// Upsert a recently used project, keyed by normalized path (newest
/// `last_used_at` wins), capped to the most recent `RECENT_PROJECTS_CAP`.
/// Best-effort from the caller's view: delete flows log and ignore errors.
pub fn record_recent_project(entry: RecentProjectEntry) -> Result<()> {
    let _mu = recent_projects_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let app_dir = get_app_dir()?;
    let _flock = acquire_storage_flock(&app_dir, RECENT_PROJECTS_LOCK_FILENAME)?;
    let mut store = load_recent_projects_inner()?;
    store.projects.retain(|p| p.path != entry.path);
    store.projects.push(entry);
    store
        .projects
        .sort_by(|a, b| b.last_used_at.cmp(&a.last_used_at));
    store.projects.truncate(RECENT_PROJECTS_CAP);
    save_recent_projects(&store)?;
    Ok(())
}

/// Persisted recent projects, newest first. Lock-free read; `atomic_write`
/// guarantees a consistent document. Callers still filter dead directories.
pub fn load_recent_projects() -> Result<Vec<RecentProjectEntry>> {
    Ok(load_recent_projects_inner()?.projects)
}

fn load_recent_projects_inner() -> Result<RecentProjects> {
    let path = recent_projects_path()?;
    if !path.exists() {
        return Ok(RecentProjects::default());
    }
    let content = fs::read_to_string(&path)?;
    if content.trim().is_empty() {
        return Ok(RecentProjects::default());
    }
    Ok(serde_json::from_str(&content)?)
}

fn save_recent_projects(store: &RecentProjects) -> Result<()> {
    let path = recent_projects_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(store)?;
    atomic_write(&path, content.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_watch::{FileMatcher, FileWatchService, WatchSpec};
    use crate::session::test_support::{isolate_app_dir_at, AppDirGuard};
    use crate::session::GroupTree;
    use serial_test::serial;
    use tempfile::tempdir;

    fn setup_test_home(temp: &std::path::Path) -> AppDirGuard {
        isolate_app_dir_at(temp)
    }

    fn parse_u64(s: &str) -> Result<u64> {
        Ok(s.trim().parse::<u64>()?)
    }

    fn serialize_u64(v: &u64) -> Result<String> {
        Ok(v.to_string())
    }

    #[test]
    fn locked_update_missing_file_starts_from_default() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("counter.txt");
        let seen = locked_update(&path, parse_u64, serialize_u64, |v| {
            let seen = *v;
            *v += 1;
            Ok::<_, anyhow::Error>(seen)
        })
        .unwrap()
        .unwrap();
        assert_eq!(seen, 0, "missing file must parse as T::default()");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "1");
    }

    #[test]
    fn locked_update_round_trips_existing_content() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("counter.txt");
        std::fs::write(&path, "41").unwrap();
        locked_update(&path, parse_u64, serialize_u64, |v| {
            *v += 1;
            Ok::<_, anyhow::Error>(())
        })
        .unwrap()
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "42");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "data file must land owner-only");
        }
    }

    #[test]
    fn locked_update_concurrent_writers_lose_no_updates() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("counter.txt");
        const THREADS: usize = 4;
        const INCREMENTS: usize = 25;

        let mut handles = Vec::new();
        for _ in 0..THREADS {
            let path = path.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..INCREMENTS {
                    locked_update(&path, parse_u64, serialize_u64, |v| {
                        *v += 1;
                        Ok::<_, anyhow::Error>(())
                    })
                    .unwrap()
                    .unwrap();
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            (THREADS * INCREMENTS).to_string(),
            "every increment must land; the sidecar flock serializes read-modify-write"
        );
    }

    #[cfg(unix)]
    #[test]
    fn locked_update_preserves_symlinks() {
        let tmp = tempdir().unwrap();
        let target = tmp.path().join("real-counter.txt");
        std::fs::write(&target, "41").unwrap();
        let link = tmp.path().join("counter.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        locked_update(&link, parse_u64, serialize_u64, |v| {
            *v += 1;
            Ok::<_, anyhow::Error>(())
        })
        .unwrap()
        .unwrap();

        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the symlink must survive; a rename over it would desync dotfile setups"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "42");
    }

    #[test]
    fn locked_update_failed_mutation_leaves_file_untouched() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("counter.txt");
        std::fs::write(&path, "41").unwrap();

        let inner = locked_update(&path, parse_u64, serialize_u64, |v| {
            *v += 1;
            Err::<(), _>(anyhow!("validation failed after mutating"))
        })
        .unwrap();

        assert!(inner.is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "41",
            "a failed mutation must not persist half-applied state"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_symlink_chain_returns_path_when_missing() {
        let tmp = tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist.json");
        assert_eq!(resolve_symlink_chain(&missing).unwrap(), missing);
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_symlink_chain_returns_path_when_regular_file() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("regular.json");
        std::fs::write(&path, b"x").unwrap();
        assert_eq!(resolve_symlink_chain(&path).unwrap(), path);
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_symlink_chain_follows_multi_hop_chain() {
        use std::os::unix::fs::symlink;
        let tmp = tempdir().unwrap();
        let target = tmp.path().join("target.json");
        std::fs::write(&target, b"x").unwrap();
        let mid = tmp.path().join("mid.json");
        symlink(&target, &mid).unwrap();
        let top = tmp.path().join("top.json");
        symlink("mid.json", &top).unwrap();
        assert_eq!(resolve_symlink_chain(&top).unwrap(), target);
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_symlink_chain_detects_loop() {
        use std::os::unix::fs::symlink;
        let tmp = tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        symlink(&b, &a).unwrap();
        symlink(&a, &b).unwrap();
        let err = resolve_symlink_chain(&a).unwrap_err().to_string();
        assert!(err.contains("too deep"), "got: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_symlink_chain_dangling_returns_target_path() {
        use std::os::unix::fs::symlink;
        let tmp = tempdir().unwrap();
        let missing = tmp.path().join("missing.json");
        let link = tmp.path().join("link.json");
        symlink(&missing, &link).unwrap();
        assert_eq!(resolve_symlink_chain(&link).unwrap(), missing);
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_symlink_chain_resolves_at_max_depth() {
        use std::os::unix::fs::symlink;
        let tmp = tempdir().unwrap();
        let target = tmp.path().join("target.json");
        std::fs::write(&target, b"x").unwrap();
        let mut prev = target.clone();
        for i in (0..32).rev() {
            let link = tmp.path().join(format!("link_{}.json", i));
            symlink(&prev, &link).unwrap();
            prev = link;
        }
        assert_eq!(resolve_symlink_chain(&prev).unwrap(), target);
    }

    #[cfg(unix)]
    #[test]
    fn test_resolve_symlink_chain_rejects_over_max_depth() {
        use std::os::unix::fs::symlink;
        let tmp = tempdir().unwrap();
        let target = tmp.path().join("target.json");
        std::fs::write(&target, b"x").unwrap();
        let mut prev = target.clone();
        for i in (0..33).rev() {
            let link = tmp.path().join(format!("link_{}.json", i));
            symlink(&prev, &link).unwrap();
            prev = link;
        }
        let err = resolve_symlink_chain(&prev).unwrap_err().to_string();
        assert!(err.contains("too deep"), "got: {err}");
    }

    #[test]
    #[serial]
    fn test_storage_roundtrip() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());

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
    fn test_open_unwatched_errors_on_unknown_profile_without_creating_dir() {
        let temp = tempdir().unwrap();
        let guard = setup_test_home(temp.path());
        let profile_dir = guard.path().join("profiles").join("ghost");
        assert!(!profile_dir.exists());

        let result = Storage::open_unwatched("ghost");
        let err = match result {
            Ok(_) => panic!("unknown profile must error"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("does not exist"), "got: {err}");
        assert!(
            !profile_dir.exists(),
            "open_unwatched must not create profiles/<name>/ as a side effect",
        );
    }

    #[test]
    #[serial]
    fn test_open_unwatched_succeeds_for_created_profile() {
        let temp = tempdir().unwrap();
        let _guard = setup_test_home(temp.path());
        crate::session::create_profile("known").unwrap();

        let storage = Storage::open_unwatched("known").expect("known profile must open");
        assert_eq!(storage.profile(), "known");
    }

    #[test]
    #[serial]
    fn test_load_skips_corrupt_row_and_quarantines() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());
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
        let _guard = setup_test_home(temp.path());
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
        let _guard = setup_test_home(temp.path());

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
        let _guard = setup_test_home(temp.path());

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
        let _guard = setup_test_home(temp.path());

        get_profile_dir("work")?;
        get_profile_dir("personal")?;
        super::super::config::update_config(|config| {
            config.default_profile = "work".to_string();
        })?;

        let storage = Storage::new_unwatched("")?;
        assert_eq!(storage.profile(), "work");
        Ok(())
    }

    #[test]
    #[serial]
    fn test_storage_new_with_custom_profile() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());

        let storage = Storage::new_unwatched("custom-profile")?;
        assert_eq!(storage.profile(), "custom-profile");
        Ok(())
    }

    #[test]
    #[serial]
    fn test_storage_load_nonexistent_file() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());

        let storage = Storage::new_unwatched("test-empty")?;
        let loaded = storage.load()?;

        assert!(loaded.is_empty());
        Ok(())
    }

    #[test]
    #[serial]
    fn test_storage_load_empty_file() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());

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
        let _guard = setup_test_home(temp.path());

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
        let _guard = setup_test_home(temp.path());

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
        let _guard = setup_test_home(temp.path());

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
        let _guard = setup_test_home(temp.path());

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
        let _guard = setup_test_home(temp.path());

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
    fn test_load_with_groups_skips_corrupt_row_and_quarantines() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());

        let storage = Storage::new_unwatched("test-groups-corrupt-row")?;
        fs::create_dir_all(storage.sessions_path.parent().unwrap())?;
        let expected_instances = [Instance::new("session", "/tmp/session")];
        fs::write(
            &storage.sessions_path,
            serde_json::to_vec_pretty(&expected_instances)?,
        )?;

        let groups_path = storage.sessions_path.with_file_name("groups.json");
        let valid = [
            Group::new("alpha", "work/alpha"),
            Group::new("beta", "work/beta"),
        ];
        let mut rows: Vec<serde_json::Value> = valid
            .iter()
            .map(|group| serde_json::to_value(group).unwrap())
            .collect();
        rows.insert(1, serde_json::json!({ "name": "corrupt-no-path" }));
        fs::write(&groups_path, serde_json::to_vec_pretty(&rows)?)?;

        let (instances, groups) = storage.load_with_groups()?;
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].title, "session");
        assert_eq!(instances[0].project_path, "/tmp/session");
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].name, "alpha");
        assert_eq!(groups[0].path, "work/alpha");
        assert_eq!(groups[1].name, "beta");
        assert_eq!(groups[1].path, "work/beta");

        let quarantine = storage.sessions_path.with_file_name("groups.corrupt.jsonl");
        assert!(quarantine.exists(), "quarantine sidecar should be created");
        let q = fs::read_to_string(&quarantine)?;
        assert_eq!(q.lines().count(), 1, "exactly one row quarantined");
        assert!(q.contains("corrupt-no-path"), "malformed row is preserved");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&quarantine)?.permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "quarantine sidecar must be owner-only");
        }

        Ok(())
    }

    #[test]
    #[serial]
    fn test_load_with_groups_repeated_read_overwrites_quarantine() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());

        let storage = Storage::new_unwatched("test-groups-corrupt-row-repeat")?;
        fs::create_dir_all(storage.sessions_path.parent().unwrap())?;
        fs::write(&storage.sessions_path, "[]")?;

        let groups_path = storage.sessions_path.with_file_name("groups.json");
        let rows = serde_json::json!([
            Group::new("alpha", "work/alpha"),
            { "name": "corrupt-no-path" },
            Group::new("beta", "work/beta")
        ]);
        fs::write(&groups_path, serde_json::to_vec_pretty(&rows)?)?;

        assert_eq!(storage.load_with_groups()?.1.len(), 2);
        let quarantine = storage.sessions_path.with_file_name("groups.corrupt.jsonl");
        let first = fs::read_to_string(&quarantine)?;

        assert_eq!(storage.load_with_groups()?.1.len(), 2);
        let second = fs::read_to_string(&quarantine)?;
        assert_eq!(second, first);
        assert_eq!(
            second.lines().count(),
            1,
            "repeated load must not duplicate"
        );

        Ok(())
    }

    #[test]
    #[serial]
    fn test_load_with_groups_top_level_corruption_still_errors() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());

        let storage = Storage::new_unwatched("test-groups-top-level-corrupt")?;
        fs::create_dir_all(storage.sessions_path.parent().unwrap())?;
        fs::write(&storage.sessions_path, "[]")?;

        let groups_path = storage.sessions_path.with_file_name("groups.json");
        let quarantine = storage.sessions_path.with_file_name("groups.corrupt.jsonl");
        for bad in [&b"{}"[..], &b"{ this is not valid json ]"[..]] {
            fs::write(&groups_path, bad)?;
            assert!(
                storage.load_with_groups().is_err(),
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
    fn test_storage_load_invalid_json() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());

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
        let _guard = setup_test_home(temp.path());

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
        let _guard = setup_test_home(temp.path());

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
        let _guard = setup_test_home(temp.path());

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
        let _guard = setup_test_home(temp.path());

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
        let _guard = setup_test_home(temp.path());

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
        let _guard = setup_test_home(temp.path());

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
        let _guard = setup_test_home(temp.path());

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
        let _guard = setup_test_home(temp.path());

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
        let _guard = setup_test_home(temp.path());

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
        let _guard = setup_test_home(temp.path());

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
        let _guard = setup_test_home(temp.path());

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
        let _guard = setup_test_home(temp.path());

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
        let _guard = setup_test_home(temp.path());

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
        let _guard = setup_test_home(temp.path());

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
        let _guard = setup_test_home(temp.path());

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
        let _guard = setup_test_home(temp.path());

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
        let _guard = setup_test_home(temp.path());

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
        let _guard = setup_test_home(temp.path());

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
        let _guard = setup_test_home(temp.path());

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

    #[test]
    fn recent_entry_normalizes_and_uses_basename() {
        let mut inst = Instance::new("s", "/home/me/projects/frontend/");
        inst.tool = "claude".to_string();
        let e = recent_project_entry_for(&inst).expect("single-repo session recorded");
        assert_eq!(e.path, "/home/me/projects/frontend");
        assert_eq!(e.display_name, "frontend");
        assert_eq!(e.tool, "claude");
    }

    #[test]
    fn recent_entry_skips_scratch() {
        // Workspaces hit the same `is_workspace()` early-return branch.
        let mut inst = Instance::new("s", "/tmp/scratch/x");
        inst.scratch = true;
        assert!(recent_project_entry_for(&inst).is_none());
    }

    #[test]
    fn recent_entry_prefers_last_accessed_over_created() {
        let mut inst = Instance::new("s", "/repo");
        let accessed = inst.created_at + chrono::Duration::hours(5);
        inst.last_accessed_at = Some(accessed);
        let e = recent_project_entry_for(&inst).unwrap();
        assert_eq!(e.last_used_at, accessed.to_rfc3339());
    }

    #[test]
    #[serial]
    fn record_recent_project_upserts_sorts_and_caps() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());

        // Capacity + 5 distinct projects, oldest first.
        for i in 0..(RECENT_PROJECTS_CAP + 5) {
            record_recent_project(RecentProjectEntry {
                path: format!("/p/{i}"),
                display_name: format!("{i}"),
                tool: "claude".to_string(),
                last_used_at: format!("2026-06-15T00:{:02}:00+00:00", i),
            })?;
        }
        let loaded = load_recent_projects()?;
        assert_eq!(loaded.len(), RECENT_PROJECTS_CAP, "capped");
        // Newest first; the 5 oldest were evicted.
        assert_eq!(loaded[0].path, format!("/p/{}", RECENT_PROJECTS_CAP + 4));
        assert!(loaded.iter().all(|p| p.path != "/p/0"));

        // Re-recording an existing path dedupes and refreshes recency.
        record_recent_project(RecentProjectEntry {
            path: format!("/p/{}", RECENT_PROJECTS_CAP + 1),
            display_name: "x".to_string(),
            tool: "claude".to_string(),
            last_used_at: "2026-06-15T23:59:00+00:00".to_string(),
        })?;
        let loaded = load_recent_projects()?;
        assert_eq!(
            loaded.len(),
            RECENT_PROJECTS_CAP,
            "still capped after upsert"
        );
        assert_eq!(loaded[0].path, format!("/p/{}", RECENT_PROJECTS_CAP + 1));
        assert_eq!(
            loaded
                .iter()
                .filter(|p| p.path == format!("/p/{}", RECENT_PROJECTS_CAP + 1))
                .count(),
            1,
            "no duplicate entry"
        );
        Ok(())
    }
}
