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
//! Title writers additionally hold an app-global, per-session title flock
//! across persistence and the post-commit tmux rekey. It is independent of
//! profile so a cross-profile move and writers using either profile still
//! serialize. Terminal title writers and launch callers then acquire the
//! source profile's per-instance lifecycle flock before any profile Storage
//! mutex/flock: session title -> lifecycle -> Storage.
//!
//! All mutation goes through `update` (load -> mutate -> save under both
//! locks). `save_workspace_ordering` is private and only consumed by
//! `update_workspace_ordering` internally; the per-profile `save` /
//! `save_groups` helpers were removed entirely. This keeps it structurally
//! impossible to bypass the locks.
//!
//! Lock-ordering rule across the process: a mutation that can change or create
//! a `(title, project_path)` pair first acquires the app-global session identity
//! flock. A title-changing mutation of an existing session then acquires that
//! session's app-dir title flock, followed by the source profile's lifecycle
//! flock, before any profile `Storage` lock. A path-only manual edit skips the
//! title flock but still nests lifecycle and Storage beneath identity. The
//! identity lock is retained through the durable commit and cache publication,
//! then released before post-commit tmux rekey; the session-title and lifecycle
//! locks remain held through rekey. `aoe add` has no existing session id, so it
//! takes identity directly before Storage. Launch and same-profile restart do
//! not take identity: their order is session title, lifecycle, then Storage.
//! A `restart_selected_session` profile move is the exception: it acquires
//! identity before the session-title and lifecycle locks even when the
//! `(title, project_path)` pair is unchanged. Code must never acquire identity
//! or session title while holding lifecycle or Storage.
//!
//! Server callers MUST drop `AppState.instances` (tokio RwLock) before
//! acquiring any flock via `tokio::task::spawn_blocking`. A flock can park on
//! a wedged peer for arbitrary time; holding the tokio RwLock across the wait
//! would block every other reader/writer and park the worker thread. The
//! cross-process storage flock is acquired AFTER the in-process mutex and
//! released BEFORE it (RAII drop order). The closure passed to `update` is
//! `FnOnce(...) -> Result<R>` and cannot await, so `std::sync::Mutex` is safe
//! across the body even on the tokio runtime. A caller already holding the
//! outer session-title and lifecycle locks must use the internal locked launch
//! path rather than reacquiring them.
//!
//! `Storage::update` closures must remain CPU/memory only (no network, user
//! input, or tmux work). The profile-move transaction has one explicit
//! exception: its `before_commit` effect may perform an already-preflighted
//! worktree move or sandbox-container release, never a tmux rekey. Across that
//! effect BOTH per-profile mutexes and BOTH cross-process storage flocks are
//! held (they are acquired in canonical directory order before `load` and
//! released only after the final write), so every peer process is excluded from
//! both profiles. Every subprocess is bounded: Git mutations use the worktree
//! mutation timeout and container commands use the runtime timeout. The effect
//! must not re-enter storage.
//!
//! Residual window: `before_commit` can move the worktree before profile rows
//! are written. A later write or sync failure may retain source and target rows
//! with the same global session id, or retain only the source row pointing at
//! the old leaf. The TUI excludes ambiguous ids instead of selecting a profile
//! by iteration order. Effect-first ordering still makes a failed worktree move
//! a clean abort that changes no profile row.
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
/// Sidecar lock prefix for one session's launch lifecycle. The validated
/// instance id is appended verbatim, yielding one lock per (profile, instance).
const INSTANCE_LIFECYCLE_LOCK_PREFIX: &str = ".instance-lifecycle-";
/// Sidecar lock for every mutation that can create or change a session's
/// `(title, project_path)` identity. It lives at app scope because TUI renames
/// can move a row between profiles.
/// The historical filename is retained so mixed-version processes still
/// coordinate during an upgrade.
const SESSION_IDENTITY_LOCK_FILENAME: &str = ".title-mutation.lock";
/// Sidecar lock prefix for one session's title persistence plus tmux rekey.
/// Lives at the app-data root so it remains stable across profile moves.
const SESSION_TITLE_LOCK_PREFIX: &str = ".session-title-";

/// Emit a tracing warn if the cross-process `flock` is held by a peer for
/// longer than this. Surfaces a wedged peer in `aoe logs` instead of a
/// silent stall. The acquire itself blocks indefinitely; the warning is
/// observability only, not a timeout.
const FLOCK_WAIT_WARN_AFTER: Duration = Duration::from_secs(1);

/// Write `content` atomically (temp file + data/metadata fsync + rename + best-effort dir fsync).
/// Existing perms are preserved; on a fresh file the result is tempfile's 0o600 default.
/// All fallible file mutations complete before the final rename, so an error
/// means the destination was not replaced.
///
/// A symlink at `path` is resolved first and the write lands on the target, so
/// a user who symlinks `config.toml` (or any other file we own) into a dotfiles
/// repo keeps the link: `rename(2)` would otherwise replace it with a regular
/// file and silently desync the dotfile tree (#2784, #3186). Nothing in AoE
/// wants to clobber such a link, so this is the single write behavior rather
/// than an opt-in helper the next caller can forget to reach for.
pub(crate) fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    let resolved = resolve_symlink_chain(path)?;
    atomic_write_resolved(&resolved, content)
}

fn atomic_write_resolved(path: &Path, content: &[u8]) -> Result<()> {
    let dir = path.parent().ok_or_else(|| {
        anyhow!(
            "atomic_write needs a path with a parent: {}",
            path.display()
        )
    })?;
    let existing_perms = fs::metadata(path).ok().map(|m| m.permissions());
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(content)?;
    if let Some(perms) = existing_perms {
        tmp.as_file().set_permissions(perms)?;
    }
    tmp.as_file().sync_all()?;
    tmp.persist(path)?;
    // Best-effort dir fsync so the rename itself survives power loss.
    if let Ok(dir_file) = fs::File::open(dir) {
        let _ = dir_file.sync_all();
    }
    Ok(())
}

/// Replace `root`/`rel` with `content`, creating `rel`'s directories, without
/// ever traversing a symlink below `root`.
///
/// For the files AoE writes inside a bind mount the container can write to. A
/// process in the sandbox can plant a link at the destination, or swap a
/// parent directory for one, between the create and the write; a plain
/// `std::fs::write` follows either and lands on a host file of the container's
/// choosing. Every component below `root` is opened `O_NOFOLLOW` from the
/// previous directory's descriptor, so a swapped ancestor fails the walk
/// instead of redirecting it, and the temp file is created and renamed through
/// that descriptor rather than by path. `root` itself is the caller's trusted
/// anchor (an AoE-owned directory, or the host side of the mount, which the
/// container cannot replace) and is opened normally.
///
/// [`atomic_write`] is the opposite contract, resolving symlinks so a user's
/// dotfile link survives, and must not be used for these paths.
///
/// The temp name is unique per attempt, so two concurrent installs cannot
/// rename each other's half-written file, and the result is 0o644: the reader
/// is a process in the container, which need not be the uid owning the bind.
#[cfg(unix)]
pub(crate) fn replace_file_no_follow(root: &Path, rel: &Path, content: &[u8]) -> Result<()> {
    use nix::errno::Errno;
    use nix::fcntl::{open, openat, renameat, OFlag};
    use nix::sys::stat::{fchmod, mkdirat, Mode};
    use nix::unistd::{unlinkat, UnlinkatFlags};
    use std::os::fd::OwnedFd;

    let mut dirs = Vec::new();
    let mut file_name = None;
    let mut components = rel.components().peekable();
    while let Some(component) = components.next() {
        let std::path::Component::Normal(name) = component else {
            return Err(anyhow!(
                "replace_file_no_follow needs a plain relative path, got {}",
                rel.display()
            ));
        };
        if components.peek().is_some() {
            dirs.push(name);
        } else {
            file_name = Some(name);
        }
    }
    let file_name = file_name.ok_or_else(|| anyhow!("replace_file_no_follow needs a file name"))?;

    let dir_flags = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC;
    // The anchor is created the ordinary way; anything that could plant a
    // symlink above it already owns the tree AoE writes into.
    fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    let mut dir: OwnedFd = open(root, dir_flags, Mode::empty())
        .with_context(|| format!("opening {}", root.display()))?;
    for name in dirs {
        match mkdirat(&dir, name, Mode::from_bits_truncate(0o700)) {
            Ok(()) | Err(Errno::EEXIST) => {}
            Err(e) => {
                return Err(anyhow!(e)).with_context(|| {
                    format!("creating {} under {}", rel.display(), root.display())
                })
            }
        }
        dir = openat(&dir, name, dir_flags | OFlag::O_NOFOLLOW, Mode::empty()).with_context(
            || {
                format!(
                    "opening {:?} under {}: a symlink there would redirect the write",
                    name,
                    root.display()
                )
            },
        )?;
    }

    let tmp_name = format!(
        ".{}.{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id(),
        NO_FOLLOW_TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let tmp = openat(
        &dir,
        tmp_name.as_str(),
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_CLOEXEC,
        Mode::from_bits_truncate(0o644),
    )
    .with_context(|| format!("creating a temp file under {}", root.display()))?;

    let written = (|| -> Result<()> {
        let mut file = fs::File::from(tmp);
        file.write_all(content)?;
        fchmod(&file, Mode::from_bits_truncate(0o644))?;
        file.sync_all()?;
        drop(file);
        renameat(&dir, tmp_name.as_str(), &dir, file_name)?;
        Ok(())
    })();
    if written.is_err() {
        let _ = unlinkat(&dir, tmp_name.as_str(), UnlinkatFlags::NoRemoveDir);
    }
    written.with_context(|| format!("writing {} under {}", rel.display(), root.display()))
}

/// Serial for the per-attempt temp name in [`replace_file_no_follow`], so two
/// writers in one process cannot pick the same one inside a clock tick.
#[cfg(unix)]
static NO_FOLLOW_TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Windows has no `O_NOFOLLOW` walk here; the sandbox this guards against is
/// Linux and macOS only. Kept so the module compiles.
#[cfg(not(unix))]
pub(crate) fn replace_file_no_follow(root: &Path, rel: &Path, content: &[u8]) -> Result<()> {
    let path = root.join(rel);
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("replace_file_no_follow needs a path with a parent"))?;
    fs::create_dir_all(dir)?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(content)?;
    tmp.as_file().sync_all()?;
    tmp.persist(&path)?;
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
        // Best-effort here, unlike atomic_write's "every fallible mutation
        // before the rename" contract: the content is already durably committed,
        // so re-tightening a pre-existing file to 0o600 must not fail the write.
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

#[cfg(unix)]
fn same_filesystem_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_filesystem_identity(_left: &fs::Metadata, _right: &fs::Metadata) -> bool {
    // Portable metadata exposes no stable file identity. Canonical path equality
    // still catches direct aliases, but junctions or reparse points may bypass
    // these guards on non-Unix platforms.
    false
}

fn paths_share_filesystem_identity(left: &Path, right: &Path) -> Result<bool> {
    Ok(same_filesystem_identity(
        &fs::metadata(left)?,
        &fs::metadata(right)?,
    ))
}

fn existing_paths_share_filesystem_identity(left: &Path, right: &Path) -> Result<bool> {
    let left = resolve_symlink_chain(left)?;
    let right = resolve_symlink_chain(right)?;
    match (fs::metadata(&left), fs::metadata(&right)) {
        (Ok(left), Ok(right)) => Ok(same_filesystem_identity(&left, &right)),
        (Err(error), _) | (_, Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(false)
        }
        (Err(error), _) | (_, Err(error)) => Err(error.into()),
    }
}

fn open_storage_lock_file(dir: &Path, name: &str) -> Result<(fs::File, PathBuf)> {
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
    Ok((file, path))
}

fn acquire_open_storage_flock(file: fs::File, path: &Path) -> Result<StorageFlock> {
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
/// Acquire the app-wide session identity-mutation lock.
///
/// Callers must take this before loading authoritative profile storage and
/// retain it through duplicate validation, external rename effects, durable
/// writes, and any in-memory cache publication. See the module lock order.
///
/// Held across slow external effects. The tied worktree rename path
/// (`session::worktree_edit::edit_worktree_workdir`) may run `git branch -m`
/// and `git worktree move` while this lock is held. Both mutations are bounded
/// by `WORKTREE_MUTATION_TIMEOUT`, so a stalled filesystem returns an error
/// instead of blocking every identity writer indefinitely. Title writers
/// release this lock after the durable commit and retain their per-session
/// title and lifecycle locks through the bounded tmux rekey.
///
/// Imports, restores, and other creation surfaces that do not use guarded add
/// or rename paths remain outside this lock. The lock prevents participating
/// writers from introducing a duplicate; it does not repair existing rows.
pub(crate) fn acquire_session_identity_lock() -> Result<StorageFlock> {
    acquire_storage_flock(&get_app_dir()?, SESSION_IDENTITY_LOCK_FILENAME)
}

/// Serialize one session's title commit and post-commit tmux rekey across
/// profiles and processes.
/// Callers must acquire this app-dir lock before a source per-instance
/// lifecycle flock and before any profile [`Storage`] lock, then hold it until
/// rekeying finishes. Keeping it separate from lifecycle locks limits its
/// scope to title writers while still covering cross-profile moves.
pub(crate) fn acquire_session_title_lock(instance_id: &str) -> Result<StorageFlock> {
    super::validate_instance_id(instance_id)
        .context("refusing session title lock for invalid instance id")?;
    acquire_storage_flock(
        &get_app_dir()?,
        &format!("{SESSION_TITLE_LOCK_PREFIX}{instance_id}.lock"),
    )
}

// Test-only crash injection for the profile-move transaction (#3459). Tests
// arm a named point and `move_instances_to_inner` panics when it is reached,
// unwinding through the rollback paths exactly like a process death would.
// Thread-local so concurrent test threads can never trip each other's
// armed points.
#[cfg(test)]
thread_local! {
    static TEST_CRASH_POINTS: std::cell::RefCell<Vec<String>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
pub(crate) fn arm_test_crash_point(name: &str) {
    TEST_CRASH_POINTS.with(|points| points.borrow_mut().push(name.to_string()));
}

#[cfg(test)]
fn test_crash_point(name: &str) {
    let armed = TEST_CRASH_POINTS.with(|points| points.borrow().iter().any(|armed| armed == name));
    if armed {
        panic!("simulated crash at crash point `{name}`");
    }
}

#[cfg(test)]
pub(crate) fn disarm_test_crash_points() {
    TEST_CRASH_POINTS.with(|points| points.borrow_mut().clear());
}

/// RAII wrapper so a failing assertion between arm and disarm can never leave
/// an armed point panicking an unrelated test scheduled on the same thread.
#[cfg(test)]
pub(crate) struct ArmedCrashPoint;

#[cfg(test)]
impl ArmedCrashPoint {
    pub(crate) fn arm(name: &'static str) -> Self {
        arm_test_crash_point(name);
        Self
    }
}

#[cfg(test)]
impl Drop for ArmedCrashPoint {
    fn drop(&mut self) {
        disarm_test_crash_points();
    }
}

pub(crate) fn sync_parent_directory(path: &Path) -> Result<()> {
    let resolved = resolve_symlink_chain(path)?;
    sync_resolved_parent_directory(&resolved)
}

#[cfg(unix)]
fn sync_resolved_parent_directory(path: &Path) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;
    fs::File::open(parent)
        .with_context(|| format!("opening profile directory {}", parent.display()))?
        .sync_all()
        .with_context(|| format!("syncing profile directory {}", parent.display()))
}

#[cfg(not(unix))]
fn sync_resolved_parent_directory(path: &Path) -> Result<()> {
    path.parent()
        .ok_or_else(|| anyhow!("path has no parent: {}", path.display()))?;
    // Rust exposes no portable directory flush outside Unix. The file content
    // was already synced before rename; do not turn every profile move into a
    // post-publication error on platforms that cannot open directories as files.
    Ok(())
}

pub(crate) fn atomic_write_verified(path: &Path, content: &[u8]) -> Result<()> {
    atomic_write_verified_resolved(path, content).map(|_| ())
}

fn restore_file_durably<W, S>(
    path: &Path,
    content: &[u8],
    write_context: W,
    sync_context: S,
) -> Result<()>
where
    W: FnOnce() -> String,
    S: FnOnce() -> String,
{
    atomic_write_verified(path, content).with_context(write_context)?;
    sync_parent_directory(path).with_context(sync_context)
}

fn atomic_write_verified_resolved(path: &Path, content: &[u8]) -> Result<PathBuf> {
    let resolved = resolve_symlink_chain(path)?;
    if let Err(error) = atomic_write_resolved(&resolved, content) {
        if fs::read(&resolved).is_ok_and(|persisted| persisted == content) {
            tracing::warn!(
                target: "session.store",
                error = %error,
                path = %resolved.display(),
                "profile move write committed but reported an error"
            );
            return Ok(resolved);
        }
        return Err(error);
    }
    Ok(resolved)
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
    let (file, path) = open_storage_lock_file(dir, name)?;
    acquire_open_storage_flock(file, &path)
}

pub struct Storage {
    profile: String,
    sessions_path: PathBuf,
    save_lock: Arc<Mutex<()>>,
    /// Used to surface in-process writes immediately to subscribers via the
    /// kernel-event-equivalent dispatcher path; see
    /// `FileWatchService::notify_local_change`. Cheap to clone (`Arc`).
    file_watch: Arc<FileWatchService>,
    #[cfg(test)]
    fail_writes_for_test: bool,
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

#[derive(Clone, Debug)]
pub(crate) struct GroupMovePlan {
    source_path: String,
    target_path: String,
    move_subtree: bool,
}

impl GroupMovePlan {
    pub(crate) fn single(source_path: &str, target_path: &str) -> Self {
        Self {
            source_path: source_path.to_string(),
            target_path: target_path.to_string(),
            move_subtree: false,
        }
    }

    pub(crate) fn subtree(source_path: &str, target_path: &str) -> Self {
        Self {
            source_path: source_path.to_string(),
            target_path: target_path.to_string(),
            move_subtree: true,
        }
    }
}

struct MoveTransactionPlan<'a> {
    group_move: &'a GroupMovePlan,
    merge_complete_post: bool,
}

fn apply_group_move(
    plan: &GroupMovePlan,
    source_instances: &[Instance],
    source_groups: &mut Vec<Group>,
    target_instances: &[Instance],
    target_groups: &mut Vec<Group>,
) {
    let source_prefix = format!("{}/", plan.source_path);
    let matches_source = |path: &str| {
        !plan.source_path.is_empty()
            && (path == plan.source_path || (plan.move_subtree && path.starts_with(&source_prefix)))
    };
    let transfer_source_metadata = plan.move_subtree || plan.source_path == plan.target_path;
    let moving_groups: Vec<Group> = source_groups
        .iter()
        .filter(|group| transfer_source_metadata && matches_source(&group.path))
        .cloned()
        .collect();

    if !plan.target_path.is_empty() {
        for mut group in moving_groups {
            let path = if group.path == plan.source_path {
                plan.target_path.clone()
            } else {
                format!(
                    "{}{}",
                    plan.target_path,
                    &group.path[plan.source_path.len()..]
                )
            };
            if target_groups.iter().any(|existing| existing.path == path) {
                continue;
            }
            group.name = path.rsplit('/').next().unwrap_or(&path).to_string();
            group.path = path;
            group.children.clear();
            target_groups.push(group);
        }
    }

    if plan.move_subtree {
        source_groups.retain(|group| !matches_source(&group.path));
    } else if !plan.source_path.is_empty() {
        let source_still_uses_path = source_instances.iter().any(|instance| {
            instance.group_path == plan.source_path
                || instance.group_path.starts_with(&source_prefix)
        });
        let has_explicit_descendant = source_groups
            .iter()
            .any(|group| group.path.starts_with(&source_prefix));
        if !source_still_uses_path && !has_explicit_descendant {
            source_groups.retain(|group| group.path != plan.source_path);
        }
    }

    // Re-tree both sides so a group implied only by a moved instance's path
    // materialises as an explicit row. This is order-stable, not a renormalise:
    // `new_with_groups` seeds `insertion_order` from the passed groups verbatim
    // and only appends paths that were missing, and `get_all_groups` replays
    // that order, so when the input already covers every referenced group the
    // output is byte-identical to the input. That is what keeps
    // `source_groups_changed` (a byte comparison at the call site) a true
    // semantic-change signal, so an unchanged source is never rewritten or
    // fsynced. See `apply_group_move_is_byte_stable_without_semantic_change`.
    *source_groups =
        super::GroupTree::new_with_groups(source_instances, source_groups).get_all_groups();
    *target_groups =
        super::GroupTree::new_with_groups(target_instances, target_groups).get_all_groups();
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
            #[cfg(test)]
            fail_writes_for_test: false,
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
            fail_writes_for_test: false,
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
            #[cfg(test)]
            fail_writes_for_test: false,
        })
    }

    /// [`Storage::open`] wired to a noop `FileWatchService`. See
    /// [`Storage::new_unwatched`] for why CLI subprocesses want the noop
    /// watcher.
    pub fn open_unwatched(profile: &str) -> Result<Self> {
        Self::open(profile, FileWatchService::noop())
    }

    /// Serialize launch/restart and explicit resume-target mutation for one
    /// instance across every process using this profile.
    ///
    /// This lock is deliberately distinct from `.storage.lock`: lifecycle
    /// callers hold it while invoking `Storage::update`, so reusing the storage
    /// flock would deadlock. Every lifecycle caller acquires this lock first,
    /// then takes short-lived storage flocks as needed.
    pub(crate) fn acquire_instance_lifecycle_lock(
        &self,
        instance_id: &str,
    ) -> Result<StorageFlock> {
        super::validate_instance_id(instance_id)
            .context("refusing lifecycle lock for invalid instance id")?;
        let profile_dir = self
            .sessions_path
            .parent()
            .ok_or_else(|| anyhow!("sessions path has no profile directory"))?;
        acquire_storage_flock(
            profile_dir,
            &format!("{INSTANCE_LIFECYCLE_LOCK_PREFIX}{instance_id}.lock"),
        )
    }

    #[cfg(test)]
    pub(crate) fn set_fail_writes_for_test(&mut self, fail: bool) {
        self.fail_writes_for_test = fail;
    }

    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// Absolute path of this profile's `sessions.json`. Recovery and the
    /// duplicate-detection surface report it so users can act on exact files.
    pub(crate) fn sessions_path(&self) -> &Path {
        &self.sessions_path
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
        self.update_under_lock(f)
    }

    /// Apply one storage mutation while the caller already owns this profile's
    /// in-process save lock and cross-process storage flock.
    fn update_under_lock<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut Vec<Instance>, &mut Vec<Group>) -> Result<R>,
    {
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
            self.file_watch.notify_local_change(&groups_path);
        }
        #[cfg(test)]
        if self.fail_writes_for_test {
            anyhow::bail!("injected sessions write failure");
        }

        atomic_write(&self.sessions_path, &instances_buf)?;
        self.file_watch.notify_local_change(&self.sessions_path);
        Ok(result)
    }

    /// Move one session, running `before_commit` only after the authoritative
    /// target validation succeeds while both profile locks are still held.
    pub(crate) fn move_instance_to_with_effect<F, B>(
        &self,
        target: &Storage,
        before: &Instance,
        after: &Instance,
        validate_target: F,
        before_commit: B,
    ) -> Result<Instance>
    where
        F: FnOnce(&[Instance], &Instance) -> Result<()>,
        B: FnOnce(&Instance) -> Result<()>,
    {
        let changes = [(before.clone(), after.clone())];
        let group_move = GroupMovePlan::single(&before.group_path, &after.group_path);
        let mut moved = self.move_instances_to_inner(
            target,
            &changes,
            MoveTransactionPlan {
                group_move: &group_move,
                merge_complete_post: true,
            },
            |instances, candidates| validate_target(instances, &candidates[0]),
            |candidates| before_commit(&candidates[0]),
            sync_resolved_parent_directory,
        )?;
        Ok(moved.remove(0))
    }

    /// Move a batch between profiles as one dual-locked transaction.
    /// Target groups and rows are written before source metadata is removed.
    /// File contents are synced on every platform; Unix also verifies the
    /// target parent-directory rename before removing source rows. Runtime
    /// source-write failures restore both profiles when the source is clear.
    /// A durable move journal written before the first mutation lets
    /// `reconcile_profile_duplicates` arbitrate any crash residual (#3459).
    pub(crate) fn move_instances_to<F>(
        &self,
        target: &Storage,
        changes: &[(Instance, Instance)],
        group_move: &GroupMovePlan,
        validate_target: F,
    ) -> Result<Vec<Instance>>
    where
        F: FnOnce(&[Instance], &[Instance]) -> Result<()>,
    {
        self.move_instances_to_inner(
            target,
            changes,
            MoveTransactionPlan {
                group_move,
                merge_complete_post: false,
            },
            validate_target,
            |_| Ok(()),
            sync_resolved_parent_directory,
        )
    }

    fn move_instances_to_inner<F, B, S>(
        &self,
        target: &Storage,
        changes: &[(Instance, Instance)],
        plan: MoveTransactionPlan<'_>,
        validate_target: F,
        before_commit: B,
        mut sync_target_parent: S,
    ) -> Result<Vec<Instance>>
    where
        F: FnOnce(&[Instance], &[Instance]) -> Result<()>,
        B: FnOnce(&[Instance]) -> Result<()>,
        S: FnMut(&Path) -> Result<()>,
    {
        if self.profile == target.profile {
            return Err(anyhow!("source and target profile are the same"));
        }
        let source_dir = self
            .sessions_path
            .parent()
            .ok_or_else(|| anyhow!("source sessions path has no parent"))?
            .canonicalize()?;
        let target_dir = target
            .sessions_path
            .parent()
            .ok_or_else(|| anyhow!("target sessions path has no parent"))?
            .canonicalize()?;
        if source_dir == target_dir || paths_share_filesystem_identity(&source_dir, &target_dir)? {
            return Err(anyhow!(
                "source and target profiles resolve to the same physical directory"
            ));
        }

        let (first, second) = if source_dir < target_dir {
            (self, target)
        } else {
            (target, self)
        };
        let _first_mu = first
            .save_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _second_mu = second
            .save_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let first_dir = first
            .sessions_path
            .parent()
            .ok_or_else(|| anyhow!("sessions path has no parent"))?;
        let second_dir = second
            .sessions_path
            .parent()
            .ok_or_else(|| anyhow!("sessions path has no parent"))?;
        let (first_lock_file, first_lock_path) =
            open_storage_lock_file(first_dir, STORAGE_LOCK_FILENAME)?;
        let (second_lock_file, second_lock_path) =
            open_storage_lock_file(second_dir, STORAGE_LOCK_FILENAME)?;
        if same_filesystem_identity(&first_lock_file.metadata()?, &second_lock_file.metadata()?) {
            return Err(anyhow!(
                "source and target profiles resolve to the same physical storage lock"
            ));
        }
        let _first_flock = acquire_open_storage_flock(first_lock_file, &first_lock_path)?;
        let _second_flock = acquire_open_storage_flock(second_lock_file, &second_lock_path)?;

        let source_groups_path = self.sessions_path.with_file_name("groups.json");
        let target_groups_path = target.sessions_path.with_file_name("groups.json");
        if existing_paths_share_filesystem_identity(&self.sessions_path, &target.sessions_path)? {
            return Err(anyhow!(
                "source and target profiles resolve to the same physical sessions file"
            ));
        }
        if existing_paths_share_filesystem_identity(&source_groups_path, &target_groups_path)? {
            return Err(anyhow!(
                "source and target profiles resolve to the same physical groups file"
            ));
        }

        let (mut source_instances, mut source_groups) = self.load_with_groups()?;
        let (mut target_instances, mut target_groups) = target.load_with_groups()?;
        let mut ids = std::collections::HashSet::with_capacity(changes.len());
        let mut moved = Vec::with_capacity(changes.len());
        for (before, after) in changes {
            if !ids.insert(before.id.as_str()) {
                return Err(anyhow!("duplicate session id in profile move batch"));
            }
            let source = source_instances
                .iter()
                .find(|instance| instance.id == before.id)
                .ok_or_else(|| anyhow!("Session not found in source profile"))?;
            if target_instances
                .iter()
                .any(|instance| instance.id == before.id)
            {
                return Err(anyhow!("Session already exists in target profile"));
            }
            let mut candidate = source.clone();
            if plan.merge_complete_post {
                candidate.merge_profile_move_diff(before, after);
            } else {
                candidate.merge_user_action_diff(before, after);
            }
            candidate.source_profile.clone_from(&target.profile);
            moved.push(candidate);
        }
        if plan.group_move.move_subtree {
            let source_prefix = format!("{}/", plan.group_move.source_path);
            let locked_members: std::collections::HashSet<&str> = source_instances
                .iter()
                .filter(|instance| {
                    instance.group_path == plan.group_move.source_path
                        || instance.group_path.starts_with(&source_prefix)
                })
                .map(|instance| instance.id.as_str())
                .collect();
            if locked_members != ids {
                return Err(anyhow!(
                    "group membership changed while the cross-profile move was pending"
                ));
            }
        }
        validate_target(&target_instances, &moved)?;

        let source_groups_before = serde_json::to_vec_pretty(&source_groups)?;
        let target_instances_before = serde_json::to_vec_pretty(&target_instances)?;
        let target_groups_before = serde_json::to_vec_pretty(&target_groups)?;

        let source_instances_before = serde_json::to_vec_pretty(&source_instances)?;
        source_instances.retain(|instance| !ids.contains(instance.id.as_str()));
        target_instances.extend(moved.iter().cloned());
        apply_group_move(
            plan.group_move,
            &source_instances,
            &mut source_groups,
            &target_instances,
            &mut target_groups,
        );
        let source_instances_after = serde_json::to_vec_pretty(&source_instances)?;
        let source_groups_after = serde_json::to_vec_pretty(&source_groups)?;
        let target_instances_after = serde_json::to_vec_pretty(&target_instances)?;
        let target_groups_after = serde_json::to_vec_pretty(&target_groups)?;
        let source_groups_changed = source_groups_after != source_groups_before;
        let target_groups_changed = target_groups_after != target_groups_before;
        // Durable move journal (#3459): written (and fsynced) before the
        // first mutation so any crash between the target publication and the
        // source removal leaves evidence that arbitrates the duplicate
        // deterministically at recovery time. Consumed only after every
        // durability barrier below has passed; error paths deliberately
        // leave it in place, where recovery either repairs the residual or
        // verifies the state is consistent and discards it.
        let journal_entry = super::move_journal::MoveJournalEntry {
            version: super::move_journal::MOVE_JOURNAL_VERSION,
            ids: {
                let mut ids: Vec<String> = ids.iter().map(|id| (*id).to_string()).collect();
                ids.sort();
                ids
            },
            source_profile: self.profile.clone(),
            target_profile: target.profile.clone(),
            source_sessions_path: self.sessions_path.clone(),
            target_sessions_path: target.sessions_path.clone(),
            group_move_source_path: plan.group_move.source_path.clone(),
            group_move_target_path: plan.group_move.target_path.clone(),
            group_move_subtree: plan.group_move.move_subtree,
            created_at_epoch_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or_default(),
        };
        let journal_path = super::move_journal::record(&journal_entry, &self.sessions_path)
            .context(
                "recording the durable move journal failed; no move effect or profile row changed",
            )?;
        #[cfg(test)]
        test_crash_point("profile-move-journal");
        // The durable journal precedes every mutation, including the external
        // worktree/container effect. If the effect fails or partially lands,
        // the retained journal proves which profiles the recovery may inspect.
        before_commit(&moved)?;

        let resolved_target_groups_path = if target_groups_changed {
            Some(atomic_write_verified_resolved(
                &target_groups_path,
                &target_groups_after,
            )?)
        } else {
            None
        };
        let resolved_target_sessions_path = match atomic_write_verified_resolved(
            &target.sessions_path,
            &target_instances_after,
        ) {
            Ok(path) => path,
            Err(target_error) => {
                if target_groups_changed {
                    if let Err(rollback_error) = restore_file_durably(
                        &target_groups_path,
                        &target_groups_before,
                        || "target group rollback failed".to_string(),
                        || "target group rollback was not durable".to_string(),
                    ) {
                        return Err(anyhow!(
                            "target profile write failed ({target_error}); target group rollback also failed or was not durable ({rollback_error})"
                        ));
                    }
                }
                return Err(target_error);
            }
        };
        // `atomic_write` already syncs file content and attempts a directory
        // sync. Unix performs this verified parent-directory barrier before
        // source removal. Other platforms use the file sync as their portable
        // durability boundary.
        if let Some(path) = resolved_target_groups_path.as_deref() {
            sync_target_parent(path)?;
        }
        sync_target_parent(&resolved_target_sessions_path)?;
        #[cfg(test)]
        test_crash_point("profile-move-target");
        if target_groups_changed {
            target.file_watch.notify_local_change(&target_groups_path);
        }
        target.file_watch.notify_local_change(&target.sessions_path);

        if source_groups_changed {
            let source_group_result =
                atomic_write_verified(&source_groups_path, &source_groups_after)
                    .and_then(|()| sync_parent_directory(&source_groups_path));
            if let Err(source_group_error) = source_group_result {
                restore_file_durably(
                    &source_groups_path,
                    &source_groups_before,
                    || {
                        format!(
                            "source group write failed ({source_group_error}); source group rollback failed"
                        )
                    },
                    || {
                        format!(
                            "source group write failed ({source_group_error}); source group rollback was not durable"
                        )
                    },
                )?;
                restore_file_durably(
                    &target.sessions_path,
                    &target_instances_before,
                    || {
                        format!(
                            "source group write failed ({source_group_error}); target session rollback failed"
                        )
                    },
                    || {
                        format!(
                            "source group write failed ({source_group_error}); target session rollback was not durable"
                        )
                    },
                )?;
                if target_groups_changed {
                    restore_file_durably(
                        &target_groups_path,
                        &target_groups_before,
                        || {
                            format!(
                                "source group write failed ({source_group_error}); target group rollback failed"
                            )
                        },
                        || {
                            format!(
                                "source group write failed ({source_group_error}); target group rollback was not durable"
                            )
                        },
                    )?;
                }
                self.file_watch.notify_local_change(&source_groups_path);
                target.file_watch.notify_local_change(&target.sessions_path);
                if target_groups_changed {
                    target.file_watch.notify_local_change(&target_groups_path);
                }
                return Err(source_group_error);
            }
            #[cfg(test)]
            test_crash_point("profile-move-source-groups");
        }
        // Crash window #3459: dying here leaves the target published while
        // the source rows are not yet durably removed, i.e. the duplicate
        // state recovery must arbitrate.
        #[cfg(test)]
        test_crash_point("profile-move-source-sessions");
        if let Err(source_error) =
            atomic_write_verified(&self.sessions_path, &source_instances_after)
        {
            match self.load() {
                Ok(instances)
                    if moved
                        .iter()
                        .all(|candidate| instances.iter().all(|row| row.id != candidate.id)) =>
                {
                    tracing::warn!(target: "session.store", error = %source_error, "source profile write committed but could not be byte-verified");
                }
                Ok(_) => {
                    if source_groups_changed {
                        restore_file_durably(
                            &source_groups_path,
                            &source_groups_before,
                            || {
                                format!(
                                    "source session write failed ({source_error}); source group rollback failed"
                                )
                            },
                            || {
                                format!(
                                    "source session write failed ({source_error}); source group rollback was not durable"
                                )
                            },
                        )?;
                    }
                    restore_file_durably(
                        &target.sessions_path,
                        &target_instances_before,
                        || {
                            format!(
                                "source session write failed ({source_error}); target session rollback failed"
                            )
                        },
                        || {
                            format!(
                                "source session write failed ({source_error}); target session rollback was not durable"
                            )
                        },
                    )?;
                    if target_groups_changed {
                        restore_file_durably(
                            &target_groups_path,
                            &target_groups_before,
                            || {
                                format!(
                                    "source session write failed ({source_error}); target group rollback failed"
                                )
                            },
                            || {
                                format!(
                                    "source session write failed ({source_error}); target group rollback was not durable"
                                )
                            },
                        )?;
                    }
                    if source_groups_changed {
                        self.file_watch.notify_local_change(&source_groups_path);
                    }
                    target.file_watch.notify_local_change(&target.sessions_path);
                    if target_groups_changed {
                        target.file_watch.notify_local_change(&target_groups_path);
                    }
                    return Err(source_error);
                }
                Err(verify_error) => {
                    return Err(anyhow!(
                        "source profile write failed ({source_error}) and could not be verified ({verify_error}); target copies were retained"
                    ));
                }
            }
        }
        if let Err(sync_error) = sync_parent_directory(&self.sessions_path) {
            if source_groups_changed {
                restore_file_durably(
                    &source_groups_path,
                    &source_groups_before,
                    || {
                        format!(
                            "source session directory sync failed ({sync_error}); source group restore failed"
                        )
                    },
                    || {
                        format!(
                            "source session directory sync failed ({sync_error}); restored source groups were not durable"
                        )
                    },
                )?;
                self.file_watch.notify_local_change(&source_groups_path);
            }
            restore_file_durably(
                &self.sessions_path,
                &source_instances_before,
                || {
                    format!(
                        "source session directory sync failed ({sync_error}); source row restore failed"
                    )
                },
                || {
                    format!(
                        "source session directory sync failed ({sync_error}); restored source rows were not durable"
                    )
                },
            )?;
            self.file_watch.notify_local_change(&self.sessions_path);
            return Err(anyhow!(
                "source session removal was not durable ({sync_error}); source rows were restored and target copies retained"
            ));
        }
        // Every write and directory barrier above has passed: the move is
        // complete. A failed cleanup must not turn a committed move into a
        // reported failure (a retry would then hit "already exists in
        // target"); the leftover entry self-heals at the next reconcile pass.
        if let Err(error) = super::move_journal::consume(&journal_path) {
            tracing::warn!(
                target: "session.store",
                error = %error,
                "completed profile move could not consume its journal; recovery will discard it"
            );
        }
        if source_groups_changed {
            self.file_watch.notify_local_change(&source_groups_path);
        }
        self.file_watch.notify_local_change(&self.sessions_path);
        Ok(moved)
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

/// Outcome of one reconciliation pass over the loaded profiles.
#[derive(Debug, Default)]
pub(crate) struct ReconciliationOutcome {
    /// True when at least one journal-guided repair changed durable state, so
    /// the caller must reload from disk before publishing anything.
    pub(crate) repaired: bool,
    /// Duplicates that lack arbitration evidence and remain excluded.
    pub(crate) reports: Vec<DuplicateIdReport>,
}

/// One ambiguous copy of a duplicated session id.
#[derive(Debug, Clone)]
pub(crate) struct DuplicateCopy {
    pub(crate) profile: String,
    pub(crate) sessions_path: PathBuf,
    pub(crate) modified_at_epoch_ms: Option<u64>,
}

/// A duplicate id that could not be repaired automatically.
#[derive(Debug, Clone)]
pub(crate) struct DuplicateIdReport {
    pub(crate) id: String,
    pub(crate) copies: Vec<DuplicateCopy>,
}

impl DuplicateIdReport {
    /// Single-line, user-actionable summary naming every copy's profile,
    /// store file, and mtime. Written to the log by the reconciliation
    /// layer; the TUI surfaces a count marker derived from these reports.
    pub(crate) fn actionable_message(&self) -> String {
        let copies = self
            .copies
            .iter()
            .map(|copy| {
                let modified = copy
                    .modified_at_epoch_ms
                    .map(|ms| format!("mtime {ms}ms"))
                    .unwrap_or_else(|| "unknown mtime".to_string());
                format!(
                    "profile `{}` at {} ({modified})",
                    copy.profile,
                    copy.sessions_path.display()
                )
            })
            .collect::<Vec<String>>()
            .join(" and ");
        format!(
            "session id `{}` exists in multiple profiles without a usable move journal; \
             nothing was changed automatically. Resolve it manually by keeping one copy \
             and deleting the other from its sessions.json (and groups.json sidecar): {copies}",
            self.id
        )
    }
}
fn file_mtime_epoch_ms(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
}

/// Ids appearing more than once across `loaded` (within one profile or
/// across profiles), in deterministic first-seen order.
pub(crate) fn detect_duplicate_ids<'a>(
    loaded: impl IntoIterator<Item = (&'a str, &'a [Instance])>,
) -> Vec<String> {
    // Counts occurrences across every profile; an id repeated even within
    // one profile is ambiguous the same way (corrupt file or writer bug) and
    // must surface, not silently fail closed.
    let mut order: Vec<String> = Vec::new();
    let mut counts: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for (_, instances) in loaded {
        for instance in instances {
            let count = counts.entry(instance.id.as_str()).or_insert_with(|| {
                order.push(instance.id.clone());
                0
            });
            *count += 1;
        }
    }
    order
        .into_iter()
        .filter(|id| counts[id.as_str()] > 1)
        .collect()
}

/// Journal evidence older than this is insufficient for arbitration (#3459):
/// an entry that outlived its move (leaked by a consume failure, a crash
/// before the TUI ever reloaded, CLI-only usage) must never delete a copy the
/// user created or edited afterwards. Expired entries degrade to the surfaced
/// legacy path. Generous by design: live residuals are consumed within one
/// reload of the crash.
const MOVE_JOURNAL_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

/// How long after journal creation a store mtime still counts as part of the
/// crashed transaction itself rather than a later user edit. Legit residuals
/// are written within seconds of record; edits beyond this slack degrade the
/// entry to surfaced legacy instead of arbitrating.
const MOVE_JOURNAL_MTIME_SLACK_MS: u64 = 5 * 60 * 1000;

/// Paths already reported as unusable this process lifetime, so a permanently
/// broken entry cannot produce ERROR spam and lock churn on every reload tick.
static UNUSABLE_JOURNAL_ENTRIES: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn unusable_journal_entries_contains(path: &Path) -> bool {
    UNUSABLE_JOURNAL_ENTRIES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .any(|seen| seen == path)
}

/// Paths whose repair failure has already been reported at ERROR level, so a
/// persistently failing (retrying) repair logs once per process.
static REPAIR_FAILURES_REPORTED: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

fn mark_repair_failure_logged(path: &Path) -> bool {
    let mut seen = REPAIR_FAILURES_REPORTED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if seen.iter().any(|seen| seen == path) {
        false
    } else {
        seen.push(path.to_path_buf());
        true
    }
}

fn mark_unusable_journal_entry(path: &Path) {
    let mut seen = UNUSABLE_JOURNAL_ENTRIES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !seen.iter().any(|seen| seen == path) {
        seen.push(path.to_path_buf());
    }
}

fn entry_age(entry: &super::move_journal::MoveJournalEntry) -> std::time::Duration {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default();
    std::time::Duration::from_millis(now_ms.saturating_sub(entry.created_at_epoch_ms))
}

/// Detect duplicates across the loaded profiles, run journal-guided repair
/// for the cases with durable evidence, and return reports for whatever
/// remains ambiguous. Repairs happen under the app-global identity lock, the
/// sorted per-session title/lifecycle locks, and each profile's own storage
/// flock (`Storage::update`). `repaired` tells the caller to reload from disk.
pub(crate) fn reconcile_profile_duplicates(
    loaded: &[(&str, &[Instance])],
    storages: &[(&str, &Storage)],
) -> ReconciliationOutcome {
    let mut outcome = ReconciliationOutcome::default();
    // Normalize to a name-sorted view so report and copy ordering are
    // deterministic regardless of how the caller iterates its storages.
    let mut normalized: Vec<(&str, &[Instance])> = loaded.to_vec();
    normalized.sort_by(|left, right| left.0.cmp(right.0));
    let duplicated = !detect_duplicate_ids(normalized.iter().copied()).is_empty();
    let scan = super::move_journal::scan(
        storages
            .iter()
            .map(|(_, storage)| storage.sessions_path().to_path_buf()),
    );
    let mut opaque_failure = !scan.unreadable_dirs.is_empty();
    for (dir, error) in scan.unreadable_dirs {
        tracing::warn!(
            target: "session.store",
            path = %dir.display(),
            error = %error,
            "move journal directory could not be listed; all arbitration is deferred"
        );
    }
    let mut valid_entries = Vec::new();
    for (path, parsed) in scan.entries {
        match parsed {
            Ok(entry) => valid_entries.push((path, entry)),
            Err(reason) => {
                opaque_failure = true;
                let already_reported = UNUSABLE_JOURNAL_ENTRIES
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .iter()
                    .any(|unusable| unusable == &path);
                if !already_reported {
                    mark_unusable_journal_entry(&path);
                    tracing::error!(
                        target: "session.store",
                        path = %path.display(),
                        reason = %reason,
                        "opaque move journal evidence blocks arbitration for this reload"
                    );
                }
            }
        }
    }
    valid_entries.sort_by_key(|(path, entry)| {
        std::cmp::Reverse((
            entry.created_at_epoch_ms,
            super::move_journal::file_created_at_nanos(path).unwrap_or_default(),
        ))
    });
    if valid_entries.is_empty() && !duplicated {
        return outcome;
    }
    if !opaque_failure {
        let mut blocked_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (path, entry) in valid_entries {
            if entry.ids.iter().any(|id| blocked_ids.contains(id)) {
                // Shadowing is transitive across a multi-id batch: if X blocks
                // this X+Y entry, Y must also block still-older evidence.
                blocked_ids.extend(entry.ids.iter().cloned());
                tracing::debug!(
                    target: "session.store",
                    path = %path.display(),
                    "older move journal is shadowed by unresolved newer intent"
                );
                continue;
            }
            let already_unusable = UNUSABLE_JOURNAL_ENTRIES
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .iter()
                .any(|unusable| unusable == &path);
            if already_unusable {
                blocked_ids.extend(entry.ids.iter().cloned());
                continue;
            }
            if entry_age(&entry) > MOVE_JOURNAL_MAX_AGE {
                mark_unusable_journal_entry(&path);
                blocked_ids.extend(entry.ids.iter().cloned());
                tracing::error!(
                    target: "session.store",
                    path = %path.display(),
                    ids = %entry.ids.join(","),
                    "expired move journal entry is insufficient evidence for arbitration; duplicates stay surfaced"
                );
                continue;
            }
            match repair_journal_entry(&entry, storages, &path) {
                Ok(true) => {
                    outcome.repaired = true;
                    tracing::info!(
                        target: "session.store",
                        ids = %entry.ids.join(","),
                        "reconciled interrupted profile move from its journal"
                    );
                }
                Ok(false) => {
                    blocked_ids.extend(entry.ids.iter().cloned());
                    tracing::debug!(
                        target: "session.store",
                        path = %path.display(),
                        "newer unresolved move intent blocks older overlapping journals"
                    );
                }
                Err(error) => {
                    blocked_ids.extend(entry.ids.iter().cloned());
                    if mark_repair_failure_logged(&path) {
                        tracing::error!(
                            target: "session.store",
                            path = %path.display(),
                            error = %error,
                            "journal-guided repair failed; older overlapping intent is blocked"
                        );
                    } else {
                        tracing::debug!(
                            target: "session.store",
                            path = %path.display(),
                            error = %error,
                            "journal-guided repair failed again"
                        );
                    }
                }
            }
        }
    }
    if !outcome.repaired {
        // Nothing changed on disk: build reports from the caller's fresh
        // load instead of re-reading every profile again.
        outcome.reports = duplicate_reports(&normalized, storages);
        return outcome;
    }
    let (reports, reload_succeeded) = reports_after_repair(&normalized, storages);
    outcome.reports = reports;
    if !reload_succeeded {
        // Keep the caller on its pre-repair loads so fail-closed reports can be
        // published instead of immediately repeating the same failed reload.
        outcome.repaired = false;
    }
    outcome
}

fn reports_after_repair(
    fallback: &[(&str, &[Instance])],
    storages: &[(&str, &Storage)],
) -> (Vec<DuplicateIdReport>, bool) {
    let mut reloaded: Vec<(String, Vec<Instance>)> = Vec::with_capacity(storages.len());
    for (_, storage) in storages {
        match storage.load() {
            Ok(instances) => reloaded.push((storage.profile.clone(), instances)),
            Err(error) => {
                tracing::error!(
                    target: "session.store",
                    profile = %storage.profile,
                    error = %error,
                    "post-repair reload failed; preserving the pre-repair duplicate report"
                );
                return (duplicate_reports(fallback, storages), false);
            }
        }
    }
    reloaded.sort_by(|left, right| left.0.cmp(&right.0));
    let reloaded_refs: Vec<(&str, &[Instance])> = reloaded
        .iter()
        .map(|(profile, instances)| (profile.as_str(), instances.as_slice()))
        .collect();
    (duplicate_reports(&reloaded_refs, storages), true)
}

/// Build one report per duplicated id with per-copy profile, store path, and
/// mtime. `loaded` must be sorted deterministically or first-seen order is
/// used as-is; reports follow `detect_duplicate_ids` order.
fn duplicate_reports(
    loaded: &[(&str, &[Instance])],
    storages: &[(&str, &Storage)],
) -> Vec<DuplicateIdReport> {
    let mut reports: Vec<DuplicateIdReport> = Vec::new();
    for id in detect_duplicate_ids(loaded.iter().copied()) {
        let copies = loaded
            .iter()
            .filter(|(_, instances)| instances.iter().any(|instance| instance.id == id))
            .filter_map(|(profile, _)| {
                let profile = *profile;
                storages
                    .iter()
                    .find(|(name, _)| *name == profile)
                    .map(|(_, storage)| storage)
            })
            .map(|storage| DuplicateCopy {
                profile: storage.profile.clone(),
                sessions_path: storage.sessions_path().to_path_buf(),
                modified_at_epoch_ms: file_mtime_epoch_ms(storage.sessions_path()),
            })
            .collect();
        reports.push(DuplicateIdReport { id, copies });
    }
    reports
}

/// True when `candidate` equals or contains `path` as an ancestor segment.
fn group_path_covers(candidate: &str, path: &str) -> bool {
    candidate == path || path.starts_with(&format!("{candidate}/"))
}

fn validate_recovery_journal(
    entry: &super::move_journal::MoveJournalEntry,
    source: &Storage,
    target: &Storage,
) -> Result<Option<String>> {
    let mut ids = std::collections::HashSet::with_capacity(entry.ids.len());
    for id in &entry.ids {
        if let Err(error) = super::validate_instance_id(id) {
            return Ok(Some(format!("invalid session id {id:?}: {error}")));
        }
        if !ids.insert(id.as_str()) {
            return Ok(Some(format!(
                "duplicate session id {id:?} in journal batch"
            )));
        }
    }
    if std::ptr::eq(source, target) || entry.source_profile == entry.target_profile {
        return Ok(Some(
            "source and target resolve to the same loaded store".to_string(),
        ));
    }
    let source_dir = source
        .sessions_path
        .parent()
        .ok_or_else(|| anyhow!("source sessions path has no parent"))?
        .canonicalize()?;
    let target_dir = target
        .sessions_path
        .parent()
        .ok_or_else(|| anyhow!("target sessions path has no parent"))?
        .canonicalize()?;
    if source_dir == target_dir || paths_share_filesystem_identity(&source_dir, &target_dir)? {
        return Ok(Some(
            "source and target resolve to the same physical profile directory".to_string(),
        ));
    }
    if existing_paths_share_filesystem_identity(source.sessions_path(), target.sessions_path())? {
        return Ok(Some(
            "source and target resolve to the same physical sessions file".to_string(),
        ));
    }
    Ok(None)
}

fn with_two_storage_locks<F, R>(source: &Storage, target: &Storage, f: F) -> Result<R>
where
    F: FnOnce() -> Result<R>,
{
    let source_dir = source
        .sessions_path
        .parent()
        .ok_or_else(|| anyhow!("source sessions path has no parent"))?
        .canonicalize()?;
    let target_dir = target
        .sessions_path
        .parent()
        .ok_or_else(|| anyhow!("target sessions path has no parent"))?
        .canonicalize()?;
    if source_dir == target_dir || paths_share_filesystem_identity(&source_dir, &target_dir)? {
        anyhow::bail!("source and target resolve to the same physical profile directory");
    }
    let (first, second) = if source_dir < target_dir {
        (source, target)
    } else {
        (target, source)
    };
    let _first_mu = first
        .save_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _second_mu = second
        .save_lock
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let first_dir = first.sessions_path.parent().unwrap();
    let second_dir = second.sessions_path.parent().unwrap();
    let (first_file, first_path) = open_storage_lock_file(first_dir, STORAGE_LOCK_FILENAME)?;
    let (second_file, second_path) = open_storage_lock_file(second_dir, STORAGE_LOCK_FILENAME)?;
    if same_filesystem_identity(&first_file.metadata()?, &second_file.metadata()?) {
        anyhow::bail!("source and target resolve to the same physical storage lock");
    }
    let _first_flock = acquire_open_storage_flock(first_file, &first_path)?;
    let _second_flock = acquire_open_storage_flock(second_file, &second_path)?;
    f()
}

/// Apply the winner policy to one journal entry. Returns Ok(true) when the
/// entry was consumed (state repaired or already consistent) and Ok(false)
/// when it cannot be applied because a referenced store is missing or has
/// moved; those entries stay on disk and their duplicates surface as legacy.
fn repair_journal_entry(
    entry: &super::move_journal::MoveJournalEntry,
    storages: &[(&str, &Storage)],
    journal_path: &Path,
) -> Result<bool> {
    repair_journal_entry_with_sync(entry, storages, journal_path, sync_parent_directory)
}

fn repair_journal_entry_with_sync<S>(
    entry: &super::move_journal::MoveJournalEntry,
    storages: &[(&str, &Storage)],
    journal_path: &Path,
    mut sync: S,
) -> Result<bool>
where
    S: FnMut(&Path) -> Result<()>,
{
    let source_storage =
        match resolve_journal_store(&entry.source_profile, &entry.source_sessions_path, storages) {
            Some(storage) => storage,
            None => return Ok(false),
        };
    let target_storage =
        match resolve_journal_store(&entry.target_profile, &entry.target_sessions_path, storages) {
            Some(storage) => storage,
            None => return Ok(false),
        };

    if let Some(reason) = validate_recovery_journal(entry, source_storage, target_storage)? {
        mark_unusable_journal_entry(journal_path);
        tracing::error!(
            target: "session.store",
            path = %journal_path.display(),
            reason = %reason,
            "move journal entry is semantically invalid; duplicates stay surfaced"
        );
        return Ok(false);
    }

    // Freshness gate: a losing store modified well after the journal was
    // written means the user edited it since the crash; arbitrating on the
    // journal would discard those edits. Legit residuals are written within
    // seconds of record, so a slack separates them from real edits.
    for storage in [source_storage, target_storage] {
        let mtime = file_mtime_epoch_ms(storage.sessions_path()).unwrap_or_default();
        if mtime.saturating_sub(entry.created_at_epoch_ms) > MOVE_JOURNAL_MTIME_SLACK_MS {
            // Permanent: mtimes only grow relative to created_at, so this
            // entry can never become applicable again. Blacklist it like the
            // other permanent insufficiency causes to avoid tick spam.
            mark_unusable_journal_entry(journal_path);
            tracing::warn!(
                target: "session.store",
                path = %journal_path.display(),
                ids = %entry.ids.join(","),
                "store was modified after the move journal was written; entry is permanently insufficient evidence and stays surfaced"
            );
            return Ok(false);
        }
    }

    // App-global identity lock first, then sorted title/lifecycle locks, then
    // the per-profile storage flocks taken inside `Storage::update`. This is
    // the same global-to-local order every other identity mutation uses.
    let _identity_lock = acquire_session_identity_lock()?;
    let mut ids_sorted = entry.ids.clone();
    ids_sorted.sort();
    ids_sorted.dedup();
    let mut guards = Vec::with_capacity(ids_sorted.len() * 2);
    for id in &ids_sorted {
        guards.push(acquire_session_title_lock(id)?);
    }
    let source_scope = source_storage
        .sessions_path()
        .parent()
        .unwrap()
        .canonicalize()?;
    let target_scope = target_storage
        .sessions_path()
        .parent()
        .unwrap()
        .canonicalize()?;
    for id in &ids_sorted {
        guards.push(source_storage.acquire_instance_lifecycle_lock(id)?);
        if source_scope != target_scope {
            guards.push(target_storage.acquire_instance_lifecycle_lock(id)?);
        }
    }

    with_two_storage_locks(source_storage, target_storage, || {
        let (source_instances, _source_groups) = source_storage.load_with_groups()?;
        let (target_instances, _) = target_storage.load_with_groups()?;
        let plan = crate::session::GroupMovePlan {
            source_path: entry.group_move_source_path.clone(),
            target_path: entry.group_move_target_path.clone(),
            move_subtree: entry.group_move_subtree,
        };
        // Automatic arbitration requires one valid row on each side. If
        // either profile already contains repeated rows for this id, preserve
        // every copy and the journal so duplicate surfacing stays in control.
        if entry.ids.iter().any(|id| {
            source_instances.iter().filter(|row| &row.id == id).count() > 1
                || target_instances.iter().filter(|row| &row.id == id).count() > 1
        }) {
            return Ok(false);
        }
        let source_losers: Vec<String> = entry
            .ids
            .iter()
            .filter(|id| {
                target_instances.iter().any(|row| &row.id == *id)
                    && source_instances.iter().any(|row| &row.id == *id)
            })
            .cloned()
            .collect();
        if source_losers.is_empty() {
            sync_repaired_profile_durably(source_storage, &mut sync)?;
            super::move_journal::consume(journal_path)?;
            return Ok(true);
        }

        source_storage.update_under_lock(|instances, groups| {
            if !target_still_holds(target_storage.sessions_path(), &source_losers)? {
                anyhow::bail!(
                    "target copies vanished while the repair was starting; leaving the journal for a retry"
                );
            }
            backup_before_repair(source_storage.sessions_path())?;
            backup_before_repair(&source_storage.sessions_path().with_file_name("groups.json"))?;
            instances.retain(|row| !source_losers.contains(&row.id));
            let winners: Vec<crate::session::Instance> = instances
                .iter()
                .filter(|row| entry.ids.contains(&row.id))
                .cloned()
                .collect();
            reconcile_groups_after_repair(instances, groups, &winners, &plan);
            Ok(())
        })?;
        sync_repaired_profile_durably(source_storage, &mut sync)?;
        #[cfg(test)]
        test_crash_point("profile-repair-source-written");
        super::move_journal::consume(journal_path)?;
        Ok(true)
    })
}

fn sync_repaired_profile_durably<S>(storage: &Storage, mut sync: S) -> Result<()>
where
    S: FnMut(&Path) -> Result<()>,
{
    // The two files normally share a profile directory, but supported
    // symlinks may resolve them into different directories. Verify both rename
    // parents before journal removal can become durable.
    sync(storage.sessions_path()).context("repaired sessions directory was not made durable")?;
    sync(&storage.sessions_path().with_file_name("groups.json"))
        .context("repaired groups directory was not made durable")
}

/// True when the target sessions file currently holds every loser id.
/// Deliberately lock-free: it runs inside the source profile's update closure
/// and only narrows the race window; the identity/title/lifecycle locks held
/// by the caller already exclude every lifecycle-mutating surface.
fn target_still_holds(target_sessions_path: &Path, losers: &[String]) -> Result<bool> {
    // Two-phase parse mirroring `Storage::load`: a single corrupt row is
    // skipped, not a whole-file failure, so a quarantined-row file cannot
    // wedge the repair into retrying forever.
    let content = match fs::read_to_string(target_sessions_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("failed re-reading target sessions during repair"),
    };
    let rows: Vec<serde_json::Value> = serde_json::from_str(&content)
        .context("failed parsing target sessions during repair re-check")?;
    let held: Vec<String> = rows
        .iter()
        .filter_map(|row| {
            <Instance as serde::Deserialize>::deserialize(row)
                .ok()
                .map(|instance| instance.id)
        })
        .collect();
    Ok(losers
        .iter()
        .all(|loser| held.iter().any(|row_id| row_id == loser)))
}

/// Resolve one journal-recorded profile only when the loaded store still
/// names the same sessions file (including symlink aliases).
fn resolve_journal_store<'a>(
    profile: &str,
    recorded_path: &Path,
    storages: &'a [(&str, &'a Storage)],
) -> Option<&'a Storage> {
    let storage = storages
        .iter()
        .find(|(name, _)| *name == profile)
        .map(|(_, storage)| *storage)?;
    if storage.sessions_path() == recorded_path {
        return Some(storage);
    }
    // Tolerate symlinked or differently-spelled paths when they still resolve
    // to the same physical file.
    match (
        recorded_path.canonicalize(),
        storage.sessions_path().canonicalize(),
    ) {
        (Ok(recorded), Ok(live)) if recorded == live => Some(storage),
        _ => None,
    }
}

const RECOVERY_BACKUPS_TO_KEEP: usize = 3;

/// Back up one repaired file and keep only the newest bounded set for that
/// filename. Backups are durably written before old entries are pruned.
fn backup_before_repair(path: &Path) -> Result<()> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).context(format!("failed reading {} for backup", path.display()))
        }
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let mut name = path
        .file_name()
        .expect("sessions path has a file name")
        .to_os_string();
    name.push(format!(".pre-recovery-{stamp}"));
    let backup = path.with_file_name(name);
    atomic_write_verified(&backup, &bytes)?;
    sync_parent_directory(&backup)
        .context(format!("backup {} was not made durable", backup.display()))?;
    prune_old_recovery_backups(path, RECOVERY_BACKUPS_TO_KEEP)
}

fn prune_old_recovery_backups(path: &Path, keep: usize) -> Result<()> {
    prune_old_recovery_backups_with_sync(path, keep, sync_resolved_parent_directory)
}

fn prune_old_recovery_backups_with_sync<S>(path: &Path, keep: usize, mut sync: S) -> Result<()>
where
    S: FnMut(&Path) -> Result<()>,
{
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(());
    };
    let prefix = format!("{file_name}.pre-recovery-");
    let mut backups: Vec<(u128, PathBuf)> = fs::read_dir(parent)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter_map(|candidate| {
            let timestamp = candidate
                .file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.strip_prefix(&prefix))
                .and_then(|stamp| stamp.parse::<u128>().ok())?;
            Some((timestamp, candidate))
        })
        .collect();
    backups.sort_by_key(|(timestamp, _)| *timestamp);
    let remove_count = backups.len().saturating_sub(keep);
    if remove_count == 0 {
        return Ok(());
    }
    for (_, old) in backups.into_iter().take(remove_count) {
        fs::remove_file(&old)
            .with_context(|| format!("failed pruning old recovery backup {}", old.display()))?;
    }
    // Backup files are lexical siblings of path even when path itself is a
    // symlink. Sync that lexical parent, not the symlink target directory.
    sync(path).context("recovery backup pruning was not made durable")
}

/// Keep the repaired profile's groups sidecar consistent with what an
/// uninterrupted `apply_group_move` would have left on disk: explicit group
/// rows attributable to the move are dropped when their members left, except
/// that a non-subtree move keeps the moved-path row alive while an explicit
/// child row survives (apply_group_move's own rule); winning rows' groups are
/// materialized and the sidecar is re-treeed through GroupTree so ancestor
/// chains and metadata match.
/// Attributable follows `apply_group_move`'s own matching: the moved path
/// itself always, descendants only for a subtree move.
fn reconcile_groups_after_repair(
    instances: &[Instance],
    groups: &mut Vec<Group>,
    winners: &[Instance],
    plan: &crate::session::GroupMovePlan,
) {
    let source_prefix = format!("{}/", plan.source_path);
    let attributable = |path: &str| {
        !plan.source_path.is_empty()
            && (path == plan.source_path || (plan.move_subtree && path.starts_with(&source_prefix)))
    };
    let has_member = |path: &str| {
        instances
            .iter()
            .any(|instance| group_path_covers(path, &instance.group_path))
    };
    // Mirror apply_group_move's non-subtree branch: an explicitly created
    // child under the moved path keeps the parent row alive (removing it
    // would orphan the surviving child below a nonexistent ancestor).
    let existing_paths: Vec<String> = groups.iter().map(|group| group.path.clone()).collect();
    let has_explicit_descendant = |path: &str| {
        let prefix = format!("{path}/");
        existing_paths
            .iter()
            .any(|candidate| candidate.starts_with(&prefix))
    };
    groups.retain(|group| {
        !attributable(&group.path)
            || has_member(&group.path)
            || (!plan.move_subtree && has_explicit_descendant(&group.path))
    });
    for winner in winners {
        if winner.group_path.is_empty() {
            continue;
        }
        let leaf = winner
            .group_path
            .rsplit('/')
            .next()
            .unwrap_or(&winner.group_path);
        if !groups.iter().any(|group| group.path == winner.group_path) {
            groups.push(Group::new(leaf, &winner.group_path));
        }
    }
    // Same final re-tree `apply_group_move` performs, so the durable sidecar
    // carries the full explicit ancestor chain with preserved metadata.
    *groups = super::GroupTree::new_with_groups(instances, groups).get_all_groups();
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

    /// True when the effective uid is 0. Root bypasses the Unix DAC permission
    /// bits, so a test that injects a write failure by making a dir read-only
    /// cannot make the write fail and must skip rather than assert `is_err()`.
    #[cfg(unix)]
    fn running_as_root() -> bool {
        nix::unistd::geteuid().is_root()
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

    /// Every write goes through the symlink to the target. Users symlink
    /// `config.toml` and friends into a dotfiles repo, and a `rename(2)` over
    /// the link would swap it for a regular file, silently desyncing the
    /// dotfile tree (#2784, #3186).
    #[cfg(unix)]
    #[test]
    fn atomic_write_follows_symlinks() {
        let tmp = tempdir().unwrap();
        let target = tmp.path().join("real-config.toml");
        std::fs::write(&target, "old").unwrap();
        let link = tmp.path().join("config.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        atomic_write(&link, b"new").unwrap();

        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the symlink must survive; a rename over it would desync dotfile setups"
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");

        // A dangling link materialises the target, not a regular file at the
        // link path: a fresh install can symlink config.toml before it exists.
        let missing = tmp.path().join("not-yet.toml");
        let dangling = tmp.path().join("dangling.toml");
        std::os::unix::fs::symlink(&missing, &dangling).unwrap();
        atomic_write(&dangling, b"seeded").unwrap();
        assert_eq!(std::fs::read_to_string(&missing).unwrap(), "seeded");
        assert!(std::fs::symlink_metadata(&dangling)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    /// A pre-existing file's non-default mode survives the write: `atomic_write`
    /// copies the destination's permissions onto the temp file before the
    /// rename, so a 0o644 file stays 0o644 rather than reverting to
    /// `NamedTempFile`'s 0o600 default.
    ///
    /// The dual half of the contract, that a failure inside `set_permissions`
    /// leaves the destination unreplaced, is deliberately not exercised here:
    /// `set_permissions` on a freshly created temp file we own has no reachable
    /// failure mode without a fault-injection seam, so that guarantee rests on
    /// the ordering in the code (every fallible step precedes `persist`) rather
    /// than on a test.
    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("perms.txt");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        atomic_write(&path, b"new").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644,
            "a pre-existing non-default mode must survive the rename"
        );
    }

    /// The mirror of `atomic_write_follows_symlinks`, for the paths AoE writes
    /// inside a sandbox bind: a link planted there by a container process must
    /// be replaced, never written through, and a swapped parent directory must
    /// fail the write rather than redirect it out of the bind.
    #[cfg(unix)]
    #[test]
    fn replace_file_no_follow_refuses_planted_links() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("bind");
        let outside = tmp.path().join("outside");
        fs::create_dir_all(root.join("agent/extensions")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let secret = outside.join("host-secret");
        fs::write(&secret, "untouched").unwrap();
        let rel = Path::new("agent/extensions/extension.js");

        // A link at the destination is replaced, not followed.
        std::os::unix::fs::symlink(&secret, root.join(rel)).unwrap();
        replace_file_no_follow(&root, rel, b"payload").unwrap();
        assert_eq!(fs::read_to_string(&secret).unwrap(), "untouched");
        assert_eq!(fs::read_to_string(root.join(rel)).unwrap(), "payload");
        assert!(!fs::symlink_metadata(root.join(rel))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::metadata(root.join(rel)).unwrap().permissions().mode() & 0o777,
            0o644,
            "the container reader may not be the uid that owns the bind"
        );

        // A parent swapped for a link fails the walk, leaving the target dir
        // untouched: nothing is written outside the bind root.
        fs::remove_dir_all(root.join("agent")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("agent")).unwrap();
        let err = replace_file_no_follow(&root, rel, b"payload").unwrap_err();
        assert!(
            !outside.join("extensions").exists(),
            "a swapped ancestor must not be traversed: {err:#}"
        );

        // And nothing is left behind for a concurrent writer to collide with.
        fs::remove_file(root.join("agent")).unwrap();
        let leftovers: Vec<_> = fs::read_dir(root.join("agent/extensions"))
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .filter(|name| name != "extension.js")
            .collect();
        assert!(leftovers.is_empty(), "stray temp files: {leftovers:?}");
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
    fn instance_lifecycle_lock_serializes_same_profile_and_instance() -> Result<()> {
        let temp = tempdir()?;
        let _guard = setup_test_home(temp.path());
        let profile = "test-instance-lifecycle-lock";
        let instance_id = Instance::new("locked", "/tmp/locked").id;
        let storage = Storage::new_unwatched(profile)?;
        let first = storage.acquire_instance_lifecycle_lock(&instance_id)?;
        let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();

        std::thread::scope(|scope| {
            scope.spawn(|| {
                let peer = Storage::new_unwatched(profile).unwrap();
                let _second = peer.acquire_instance_lifecycle_lock(&instance_id).unwrap();
                acquired_tx.send(()).unwrap();
            });
            assert!(
                acquired_rx
                    .recv_timeout(Duration::from_millis(150))
                    .is_err(),
                "peer acquired the same lifecycle lock before release"
            );
            drop(first);
            acquired_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("peer did not acquire lifecycle lock after release");
        });
        assert!(
            storage
                .acquire_instance_lifecycle_lock("../escape")
                .is_err(),
            "lock filename must reject an unsafe instance id"
        );
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

        if running_as_root() {
            eprintln!(
                "test_update_write_failure_emits_no_notify: skipping (running as root; \
                 uid 0 bypasses the read-only dir bit, so the write cannot be made to fail)"
            );
            return Ok(());
        }

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
    fn profile_batch_move_rejects_collision_and_merges_fresh_source() -> Result<()> {
        let temp = tempdir()?;
        let source_dir = temp.path().join("source");
        let target_dir = temp.path().join("target");
        std::fs::create_dir_all(&source_dir)?;
        std::fs::create_dir_all(&target_dir)?;
        let source = Storage::new_for_test_path("move-source", source_dir.join("sessions.json"));
        let target = Storage::new_for_test_path("move-target", target_dir.join("sessions.json"));
        let mut first = Instance::new("first", "/repo/first");
        first.source_profile = "move-source".to_string();
        let mut second = Instance::new("second", "/repo/second");
        second.source_profile = "move-source".to_string();
        source.update(|instances, _groups| {
            *instances = vec![first.clone(), second.clone()];
            Ok(())
        })?;
        let owner = Instance::new("second", "/repo/second/");
        target.update(|instances, _groups| {
            instances.push(owner);
            Ok(())
        })?;

        let mut first_after = first.clone();
        first_after.group_path = "moved".to_string();
        let mut second_after = second.clone();
        second_after.group_path = "moved".to_string();
        let changes = [
            (first.clone(), first_after.clone()),
            (second.clone(), second_after.clone()),
        ];
        let group_move = GroupMovePlan::subtree("", "moved");
        let rejected =
            source.move_instances_to(&target, &changes, &group_move, |existing, candidates| {
                if candidates.iter().any(|candidate| {
                    existing.iter().any(|row| {
                        row.title == candidate.title
                            && row.project_path.trim_end_matches('/')
                                == candidate.project_path.trim_end_matches('/')
                    })
                }) {
                    return Err(anyhow!("duplicate"));
                }
                Ok(())
            });
        assert!(rejected.is_err());
        assert_eq!(source.load()?.len(), 2);
        assert_eq!(target.load()?.len(), 1);

        target.update(|instances, _groups| {
            instances.clear();
            Ok(())
        })?;
        source.update(|instances, _groups| {
            instances
                .iter_mut()
                .find(|instance| instance.id == first.id)
                .unwrap()
                .unread = true;
            Ok(())
        })?;
        let moved = source.move_instances_to(
            &target,
            &changes,
            &group_move,
            |_existing, _candidates| Ok(()),
        )?;
        assert_eq!(moved.len(), 2);
        let moved_first = moved
            .iter()
            .find(|instance| instance.id == first.id)
            .unwrap();
        assert!(moved_first.unread, "fresh peer field must survive");
        assert_eq!(moved_first.group_path, "moved");
        assert!(source.load()?.is_empty());
        assert_eq!(target.load()?.len(), 2);
        Ok(())
    }

    #[test]
    fn profile_move_runs_external_effect_only_after_locked_target_validation() -> Result<()> {
        let temp = tempdir()?;
        let source_dir = temp.path().join("source-effect");
        let target_dir = temp.path().join("target-effect");
        fs::create_dir_all(&source_dir)?;
        fs::create_dir_all(&target_dir)?;
        let source = Storage::new_for_test_path("effect-source", source_dir.join("sessions.json"));
        let target = Storage::new_for_test_path("effect-target", target_dir.join("sessions.json"));
        let before = Instance::new("collision", "/repo/collision");
        source.update(|instances, _groups| {
            instances.push(before.clone());
            Ok(())
        })?;
        target.update(|instances, _groups| {
            instances.push(Instance::new("collision", "/repo/collision/"));
            Ok(())
        })?;
        let effect_ran = std::cell::Cell::new(false);

        let result = source.move_instance_to_with_effect(
            &target,
            &before,
            &before,
            |instances, candidate| {
                if instances.iter().any(|row| {
                    row.title == candidate.title
                        && row.project_path.trim_end_matches('/')
                            == candidate.project_path.trim_end_matches('/')
                }) {
                    return Err(anyhow!("duplicate"));
                }
                Ok(())
            },
            |_| {
                effect_ran.set(true);
                Ok(())
            },
        );

        assert!(result.is_err());
        assert!(!effect_ran.get());
        assert_eq!(source.load()?.len(), 1);
        assert_eq!(target.load()?.len(), 1);
        Ok(())
    }

    #[test]
    fn profile_move_transfers_explicit_empty_group_metadata() -> Result<()> {
        let temp = tempdir()?;
        let source_dir = temp.path().join("source-empty-group");
        let target_dir = temp.path().join("target-empty-group");
        fs::create_dir_all(&source_dir)?;
        fs::create_dir_all(&target_dir)?;
        let source = Storage::new_for_test_path("empty-source", source_dir.join("sessions.json"));
        let target = Storage::new_for_test_path("empty-target", target_dir.join("sessions.json"));
        source.update(|_instances, groups| {
            let mut group = Group::new("empty", "empty");
            group.collapsed = true;
            group.archived_at = Some(chrono::Utc::now());
            groups.push(group);
            Ok(())
        })?;
        target.update(|_instances, _groups| Ok(()))?;

        let moved = source.move_instances_to(
            &target,
            &[],
            &GroupMovePlan::subtree("empty", "renamed"),
            |_existing, candidates| {
                assert!(candidates.is_empty());
                Ok(())
            },
        )?;

        assert!(moved.is_empty());
        assert!(source
            .load_with_groups()?
            .1
            .iter()
            .all(|group| group.path != "empty"));
        let target_group = target
            .load_with_groups()?
            .1
            .into_iter()
            .find(|group| group.path == "renamed")
            .expect("explicit empty group metadata transferred");
        assert!(target_group.collapsed);
        assert!(target_group.archived_at.is_some());
        Ok(())
    }

    #[test]
    fn profile_group_move_rejects_fresh_unplanned_member() -> Result<()> {
        let temp = tempdir()?;
        let source_dir = temp.path().join("source-members");
        let target_dir = temp.path().join("target-members");
        fs::create_dir_all(&source_dir)?;
        fs::create_dir_all(&target_dir)?;
        let source = Storage::new_for_test_path("member-source", source_dir.join("sessions.json"));
        let target = Storage::new_for_test_path("member-target", target_dir.join("sessions.json"));
        let mut before = Instance::new("snapshot", "/repo/snapshot");
        before.group_path = "team".to_string();
        let mut after = before.clone();
        after.group_path = "moved".to_string();
        source.update(|instances, groups| {
            instances.push(before.clone());
            groups.push(Group::new("team", "team"));
            Ok(())
        })?;
        target.update(|_instances, _groups| Ok(()))?;

        source.update(|instances, _groups| {
            let mut concurrent = Instance::new("concurrent", "/repo/concurrent");
            concurrent.group_path = "team/new".to_string();
            instances.push(concurrent);
            Ok(())
        })?;
        let error = source
            .move_instances_to(
                &target,
                &[(before, after)],
                &GroupMovePlan::subtree("team", "moved"),
                |_existing, _candidates| Ok(()),
            )
            .expect_err("fresh subtree membership must be revalidated under lock");

        assert!(error.to_string().contains("group membership changed"));
        let (source_rows, source_groups) = source.load_with_groups()?;
        assert_eq!(source_rows.len(), 2);
        assert!(source_groups.iter().any(|group| group.path == "team"));
        assert!(target.load()?.is_empty());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn profile_move_syncs_resolved_symlink_target_parent() -> Result<()> {
        use std::os::unix::fs::symlink;

        let temp = tempdir()?;
        let source_dir = temp.path().join("source-symlink");
        let target_dir = temp.path().join("target-symlink");
        let resolved_sessions_dir = temp.path().join("resolved-sessions");
        let resolved_groups_dir = temp.path().join("resolved-groups");
        fs::create_dir_all(&source_dir)?;
        fs::create_dir_all(&target_dir)?;
        fs::create_dir_all(&resolved_sessions_dir)?;
        fs::create_dir_all(&resolved_groups_dir)?;
        let source = Storage::new_for_test_path("symlink-source", source_dir.join("sessions.json"));
        let target_link = target_dir.join("sessions.json");
        let target_groups_link = target_dir.join("groups.json");
        let resolved_sessions = resolved_sessions_dir.join("sessions.json");
        let resolved_groups = resolved_groups_dir.join("groups.json");
        fs::write(&resolved_sessions, b"[]")?;
        fs::write(&resolved_groups, b"[]")?;
        symlink(&resolved_sessions, &target_link)?;
        symlink(&resolved_groups, &target_groups_link)?;
        let target = Storage::new_for_test_path("symlink-target", target_link);
        let mut before = Instance::new("session", "/repo/session");
        before.source_profile = "symlink-source".to_string();
        before.group_path = "work".to_string();
        source.update(|instances, groups| {
            instances.push(before.clone());
            groups.push(Group::new("work", "work"));
            Ok(())
        })?;

        let result = source.move_instances_to_inner(
            &target,
            &[(before.clone(), before.clone())],
            MoveTransactionPlan {
                group_move: &GroupMovePlan::single("work", "work"),
                merge_complete_post: true,
            },
            |_existing, _candidates| Ok(()),
            |_| Ok(()),
            {
                let mut synced = Vec::new();
                move |path| {
                    synced.push(path.to_path_buf());
                    if path == resolved_sessions {
                        assert_eq!(
                            synced,
                            vec![resolved_groups.clone(), resolved_sessions.clone()]
                        );
                        Err(anyhow!("forced resolved-directory sync failure"))
                    } else {
                        assert_eq!(path, resolved_groups);
                        Ok(())
                    }
                }
            },
        );

        assert!(result.is_err());
        assert_eq!(source.load()?.len(), 1);
        assert_eq!(target.load()?.len(), 1);
        Ok(())
    }

    #[test]
    fn profile_move_keeps_source_when_target_directory_sync_fails() -> Result<()> {
        let temp = tempdir()?;
        let source_dir = temp.path().join("source");
        let target_dir = temp.path().join("target");
        fs::create_dir_all(&source_dir)?;
        fs::create_dir_all(&target_dir)?;
        let source = Storage::new_for_test_path("sync-source", source_dir.join("sessions.json"));
        let target = Storage::new_for_test_path("sync-target", target_dir.join("sessions.json"));
        let mut before = Instance::new("session", "/repo/session");
        before.source_profile = "sync-source".to_string();
        before.group_path = "work".to_string();
        source.update(|instances, groups| {
            instances.push(before.clone());
            groups.push(Group::new("work", "work"));
            Ok(())
        })?;
        target.update(|_instances, _groups| Ok(()))?;
        let after = before.clone();
        let plan = GroupMovePlan::single("work", "work");

        let result = source.move_instances_to_inner(
            &target,
            &[(before.clone(), after)],
            MoveTransactionPlan {
                group_move: &plan,
                merge_complete_post: true,
            },
            |_existing, _candidates| Ok(()),
            |_| Ok(()),
            |_path| Err(anyhow!("forced target directory sync failure")),
        );
        assert!(result.is_err());
        let (source_rows, source_groups) = source.load_with_groups()?;
        assert_eq!(source_rows.len(), 1, "source row must remain durable");
        assert!(source_groups.iter().any(|group| group.path == "work"));
        let (target_rows, target_groups) = target.load_with_groups()?;
        assert_eq!(
            target_rows.len(),
            1,
            "the durable target copy is retained for recovery"
        );
        assert!(target_groups.iter().any(|group| group.path == "work"));
        Ok(())
    }

    #[test]
    fn profile_move_retains_recoverable_source_after_effect_ran_and_write_fails() -> Result<()> {
        // D2 residual window: `before_commit` moves the worktree directory before
        // any row is written, so a write failure after it aborts with the effect
        // applied. The transaction does not auto-reverse the effect; instead it
        // keeps both the source row and the durable target copy, leaving a
        // reconcilable state (never a lost row) that recovery can repair.
        let temp = tempdir()?;
        let source_dir = temp.path().join("source-effect-window");
        let target_dir = temp.path().join("target-effect-window");
        fs::create_dir_all(&source_dir)?;
        fs::create_dir_all(&target_dir)?;
        let source = Storage::new_for_test_path("window-source", source_dir.join("sessions.json"));
        let target = Storage::new_for_test_path("window-target", target_dir.join("sessions.json"));
        let mut before = Instance::new("session", "/repo/session");
        before.source_profile = "window-source".to_string();
        before.group_path = "work".to_string();
        source.update(|instances, groups| {
            instances.push(before.clone());
            groups.push(Group::new("work", "work"));
            Ok(())
        })?;
        target.update(|_instances, _groups| Ok(()))?;
        let after = before.clone();
        let plan = GroupMovePlan::single("work", "work");
        let effect_ran = std::cell::Cell::new(false);

        let result = source.move_instances_to_inner(
            &target,
            &[(before.clone(), after)],
            MoveTransactionPlan {
                group_move: &plan,
                merge_complete_post: true,
            },
            |_existing, _candidates| Ok(()),
            |_moved| {
                // Stand in for the worktree move / tmux rename effect.
                effect_ran.set(true);
                Ok(())
            },
            |_path| {
                Err(anyhow!(
                    "forced target directory sync failure after the effect"
                ))
            },
        );

        assert!(result.is_err());
        assert!(
            effect_ran.get(),
            "the external effect runs before the failing write"
        );
        let source_rows = source.load()?;
        assert_eq!(
            source_rows.len(),
            1,
            "the source row is retained so the moved directory is reconcilable, not lost"
        );
        assert_eq!(
            target.load()?.len(),
            1,
            "the durable target copy is retained"
        );
        Ok(())
    }

    #[test]
    fn apply_group_move_is_byte_stable_without_semantic_change() -> Result<()> {
        // When a group the move touches is still used by a remaining source
        // instance, `apply_group_move` must leave the source groups byte-for-byte
        // unchanged: the re-tree replays insertion order and preserves metadata,
        // so `source_groups_changed` stays a true semantic signal and an unchanged
        // source is never rewritten or fsynced (point 4 of the review).
        let mut mover = Instance::new("mover", "/repo/mover");
        mover.group_path = "work".to_string();
        let mut stayer = Instance::new("stayer", "/repo/stayer");
        stayer.group_path = "work".to_string();

        // Post-retain source still holds `stayer` in "work"; the mover has left.
        let source_instances = vec![stayer];
        let mut work_group = Group::new("work", "work");
        work_group.collapsed = true;
        work_group.archived_at = Some(chrono::Utc::now());
        let mut source_groups = vec![work_group];
        let target_instances = vec![mover];
        let mut target_groups = Vec::new();

        let before = serde_json::to_vec_pretty(&source_groups)?;
        apply_group_move(
            &GroupMovePlan::single("work", "work"),
            &source_instances,
            &mut source_groups,
            &target_instances,
            &mut target_groups,
        );
        let after = serde_json::to_vec_pretty(&source_groups)?;
        assert_eq!(
            before, after,
            "source groups must be byte-stable, including collapsed/archived metadata"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn profile_move_rejects_shared_storage_lock_inode() -> Result<()> {
        let temp = tempdir()?;
        let source_dir = temp.path().join("source");
        let target_dir = temp.path().join("target");
        fs::create_dir_all(&source_dir)?;
        fs::create_dir_all(&target_dir)?;
        let source = Storage::new_for_test_path("inode-source", source_dir.join("sessions.json"));
        let target = Storage::new_for_test_path("inode-target", target_dir.join("sessions.json"));
        let mut before = Instance::new("session", "/repo/session");
        before.source_profile = "inode-source".to_string();
        before.group_path = "work".to_string();
        source.update(|instances, groups| {
            instances.push(before.clone());
            groups.push(Group::new("work", "work"));
            Ok(())
        })?;
        target.update(|_instances, groups| {
            groups.push(Group::new("target", "target"));
            Ok(())
        })?;
        let source_lock = source_dir.join(STORAGE_LOCK_FILENAME);
        let target_lock = target_dir.join(STORAGE_LOCK_FILENAME);
        fs::remove_file(&target_lock)?;
        fs::hard_link(&source_lock, &target_lock)?;

        let error = source
            .move_instance_to_with_effect(
                &target,
                &before,
                &before,
                |_instances, _candidate| Ok(()),
                |_| Ok(()),
            )
            .expect_err("shared lock inode must be rejected before either flock can self-deadlock");

        assert!(error.to_string().contains("physical storage lock"));
        assert_eq!(source.load()?.len(), 1);
        assert!(target.load()?.is_empty());

        fs::remove_file(&target_lock)?;
        fs::File::create(&target_lock)?;
        let source_groups = source_dir.join("groups.json");
        let target_groups = target_dir.join("groups.json");
        fs::remove_file(&target_groups)?;
        fs::hard_link(&source_groups, &target_groups)?;
        let effect_ran = std::cell::Cell::new(false);
        let error = source
            .move_instance_to_with_effect(
                &target,
                &before,
                &before,
                |_instances, _candidate| Ok(()),
                |_| {
                    effect_ran.set(true);
                    Ok(())
                },
            )
            .expect_err("shared groups inode must be rejected before external effects");
        assert!(error.to_string().contains("physical groups file"));
        assert!(!effect_ran.get());
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
    #[test]
    fn profile_move_crash_after_target_publication_leaves_duplicate_id() -> Result<()> {
        // Ground truth for #3459: a process death after the target copy is
        // durable but before the source row is removed leaves two rows with
        // the same globally unique id across profiles.
        let temp = tempdir()?;
        let source_dir = temp.path().join("repro-source");
        let target_dir = temp.path().join("repro-target");
        fs::create_dir_all(&source_dir)?;
        fs::create_dir_all(&target_dir)?;
        let source = Storage::new_for_test_path("repro-source", source_dir.join("sessions.json"));
        let target = Storage::new_for_test_path("repro-target", target_dir.join("sessions.json"));
        let mut before = Instance::new("session", "/repo/session");
        before.source_profile = "repro-source".to_string();
        before.group_path = "work".to_string();
        source.update(|instances, groups| {
            instances.push(before.clone());
            groups.push(Group::new("work", "work"));
            Ok(())
        })?;
        target.update(|_instances, _groups| Ok(()))?;

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = source.move_instances_to_inner(
                &target,
                &[(before.clone(), before.clone())],
                MoveTransactionPlan {
                    group_move: &GroupMovePlan::single("work", "moved"),
                    merge_complete_post: true,
                },
                |_existing, _candidates| Ok(()),
                |_| Ok(()),
                |_path| panic!("simulated crash after target publication"),
            );
        }));
        assert!(result.is_err(), "the simulated crash must abort the move");

        let source_rows = source.load()?;
        let target_rows = target.load()?;
        assert_eq!(source_rows.len(), 1, "source row survives the crash");
        assert_eq!(target_rows.len(), 1, "target copy is durable");
        assert_eq!(
            source_rows[0].id, target_rows[0].id,
            "both profiles hold the same session id: ambiguous state"
        );
        Ok(())
    }

    /// Shared harness for the #3459 recovery tests: stores, journals, and
    /// app-global identity/title locks all live below one isolated temp root.
    fn setup_recovery_env(
        tag: &str,
    ) -> Result<(
        tempfile::TempDir,
        AppDirGuard,
        Storage,
        Storage,
        Instance,
        Instance,
    )> {
        let temp = tempfile::TempDir::new()?;
        let guard = isolate_app_dir_at(temp.path());
        let source_dir = temp.path().join(format!("{tag}-source"));
        let target_dir = temp.path().join(format!("{tag}-target"));
        fs::create_dir_all(&source_dir)?;
        fs::create_dir_all(&target_dir)?;
        let source =
            Storage::new_for_test_path(&format!("{tag}-source"), source_dir.join("sessions.json"));
        let target =
            Storage::new_for_test_path(&format!("{tag}-target"), target_dir.join("sessions.json"));
        let mut before = Instance::new("session", "/repo/session");
        before.source_profile = format!("{tag}-source");
        before.group_path = "work".to_string();
        source.update(|instances, groups| {
            instances.push(before.clone());
            groups.push(Group::new("work", "work"));
            Ok(())
        })?;
        target.update(|_instances, _groups| Ok(()))?;
        let mut after = before.clone();
        after.group_path = "moved".to_string();
        Ok((temp, guard, source, target, before, after))
    }

    fn run_crashing_move(source: &Storage, target: &Storage, point: &'static str) {
        let _crash = ArmedCrashPoint::arm(point);
        // Panic output for the simulated crash is expected noise.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut after = source.load().unwrap().remove(0);
            after.group_path = "moved".to_string();
            let before = source.load().unwrap().remove(0);
            let _ = source.move_instances_to_inner(
                target,
                &[(before, after)],
                MoveTransactionPlan {
                    group_move: &GroupMovePlan::single("work", "moved"),
                    merge_complete_post: true,
                },
                |_existing, _candidates| Ok(()),
                |_| Ok(()),
                sync_resolved_parent_directory,
            );
        }));
    }

    #[test]
    fn profile_move_crash_recovery_target_wins_from_each_crash_point() -> Result<()> {
        // One case per crash point #3459 requires: target write/fsync,
        // source group write/fsync, source session write/fsync. In all
        // three the target copy is already durable when the process dies,
        // so the journal arbitrates: target wins, source copy removed,
        // sidecars consistent, journal consumed, backups left behind.
        for point in [
            "profile-move-target",
            "profile-move-source-groups",
            "profile-move-source-sessions",
        ] {
            let (_temp, _guard, source, target, _before, _after) =
                setup_recovery_env(point.replace('-', "_").as_str())?;
            run_crashing_move(&source, &target, point);

            assert_eq!(source.load()?.len(), 1, "{point}: source row remains");
            assert_eq!(target.load()?.len(), 1, "{point}: target copy durable");
            assert_eq!(
                journal_entry_count(&source),
                1,
                "{point}: exactly one journal entry guards the residual"
            );

            let view: Vec<(&str, &Storage)> =
                vec![(source.profile(), &source), (target.profile(), &target)];
            let outcome = reconcile_loaded(&[&source, &target], &view);

            assert!(outcome.repaired, "{point}: repair must run");
            assert!(
                outcome.reports.is_empty(),
                "{point}: no legacy ambiguity may remain: {reports:?}",
                reports = outcome
                    .reports
                    .iter()
                    .map(|r| r.actionable_message())
                    .collect::<Vec<_>>()
            );
            assert!(source.load()?.is_empty(), "{point}: losing source emptied");
            let target_rows = target.load()?;
            assert_eq!(target_rows.len(), 1, "{point}");
            assert_eq!(target_rows[0].group_path, "moved", "{point}");
            let target_groups = target.load_with_groups()?.1;
            assert!(
                target_groups.iter().any(|group| group.path == "moved"),
                "{point}: winning sidecar keeps the moved group"
            );
            let source_groups = source.load_with_groups()?.1;
            assert!(
                !source_groups.iter().any(|group| group.path == "work"),
                "{point}: losing attributable group entry pruned"
            );
            assert_eq!(journal_entry_count(&source), 0, "{point}: journal consumed");
            let backup_count = count_recovery_backups(source.sessions_path());
            assert!(
                backup_count >= 1,
                "{point}: sessions.json backed up before repair"
            );

            // Idempotence: reconciling an already-repaired state is a no-op.
            let outcome = reconcile_loaded(&[&source, &target], &view);
            assert!(!outcome.repaired && outcome.reports.is_empty(), "{point}");
        }
        Ok(())
    }

    #[test]
    fn profile_move_crash_before_publication_source_wins() -> Result<()> {
        // Crash right after the journal is written but before any row was
        // touched: the evidence says the target never published, so the
        // source row wins and nothing is removed. The leaked journal entry
        // is still consumed as consistent.
        let (temp, _guard, source, target, _before, _after) = setup_recovery_env("prepub")?;
        assert!(
            crate::session::get_app_dir()?.starts_with(temp.path()),
            "identity/title locks must stay below the fixture temp root"
        );
        source.update(|_instances, groups| {
            let mut bystander = Group::new("moved", "moved");
            bystander.collapsed = true;
            groups.push(bystander);
            Ok(())
        })?;
        target.update(|_instances, groups| {
            let mut existing = Group::new("moved", "moved");
            existing.collapsed = true;
            groups.push(existing);
            Ok(())
        })?;
        let source_groups_before = source.load_with_groups()?.1;
        let target_groups_before = target.load_with_groups()?.1;
        run_crashing_move(&source, &target, "profile-move-journal");

        assert_eq!(source.load()?.len(), 1, "source row untouched");
        assert!(target.load()?.is_empty(), "target never published");
        assert_eq!(journal_entry_count(&source), 1);

        let view: Vec<(&str, &Storage)> =
            vec![(source.profile(), &source), (target.profile(), &target)];
        let outcome = reconcile_loaded(&[&source, &target], &view);

        assert!(outcome.repaired, "the leaked journal must be consumed");
        assert!(outcome.reports.is_empty());
        assert_eq!(source.load()?.len(), 1, "source wins, nothing removed");
        assert!(target.load()?.is_empty());
        assert_eq!(source.load_with_groups()?.1, source_groups_before);
        assert_eq!(target.load_with_groups()?.1, target_groups_before);
        assert_eq!(journal_entry_count(&source), 0);
        Ok(())
    }

    #[test]
    fn legacy_duplicate_without_journal_is_surfaced_never_arbitrated() -> Result<()> {
        // Two copies of one id with no usable journal evidence: neither may
        // be chosen by iteration order. Both rows stay on disk, both are
        // excluded upstream, and the report names profiles, files, mtimes.
        let (_temp, _guard, source, target, before, _after) = setup_recovery_env("legacy")?;
        let id = before.id.clone();
        target.update(|instances, _| {
            let mut copy = before.clone();
            copy.source_profile = target.profile().to_string();
            instances.push(copy);
            Ok(())
        })?;

        let view: Vec<(&str, &Storage)> =
            vec![(source.profile(), &source), (target.profile(), &target)];
        let outcome = reconcile_loaded(&[&source, &target], &view);

        assert!(!outcome.repaired);
        assert_eq!(
            outcome.reports.len(),
            1,
            "exactly the duplicated id surfaces"
        );
        let report = &outcome.reports[0];
        assert_eq!(report.id, id);
        assert_eq!(report.copies.len(), 2);
        let message = report.actionable_message();
        assert!(message.contains(&id), "message names the session id");
        assert!(
            message.contains("sessions.json"),
            "message names store files"
        );
        for storage in [&source, &target] {
            assert!(
                message.contains(storage.profile()),
                "message names profile {}: {message}",
                storage.profile()
            );
            assert_eq!(storage.load()?.len(), 1, "no automatic arbitration");
        }
        assert_eq!(journal_entry_count(&source), 0);
        Ok(())
    }

    #[test]
    fn insufficient_evidence_journal_is_surfaced_never_consumed() -> Result<()> {
        // Table over the two permanent insufficiency causes: an entry from
        // another version and an entry older than MOVE_JOURNAL_MAX_AGE.
        // Neither may arbitrate; both stay on disk untouched.
        let week_ago_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as u64
            - 8 * 24 * 3600 * 1000;
        let cases = [
            (
                "insuff-wrong-version",
                super::super::move_journal::MOVE_JOURNAL_VERSION + 1,
                0,
            ),
            (
                "insuff-expired",
                super::super::move_journal::MOVE_JOURNAL_VERSION,
                week_ago_ms,
            ),
        ];
        for (tag, version, created_at) in cases {
            let (_temp, _guard, source, target, before, _after) = setup_recovery_env(tag)?;
            target.update(|instances, _| {
                let mut copy = before.clone();
                copy.source_profile = target.profile().to_string();
                instances.push(copy);
                Ok(())
            })?;
            let entry = super::super::move_journal::MoveJournalEntry {
                version,
                created_at_epoch_ms: created_at,
                ..fresh_journal_entry(&source, &target, &before.id)
            };
            super::super::move_journal::record(&entry, source.sessions_path())?;
            assert_eq!(journal_entry_count(&source), 1, "{tag}");

            let view: Vec<(&str, &Storage)> =
                vec![(source.profile(), &source), (target.profile(), &target)];
            let outcome = reconcile_loaded(&[&source, &target], &view);

            assert!(
                !outcome.repaired,
                "{tag}: insufficient evidence must not arbitrate"
            );
            assert_eq!(outcome.reports.len(), 1, "{tag}: duplicate stays surfaced");
            assert_eq!(source.load()?.len(), 1, "{tag}");
            assert_eq!(target.load()?.len(), 1, "{tag}");
            assert_eq!(
                journal_entry_count(&source),
                1,
                "{tag}: entry stays on disk"
            );
        }
        Ok(())
    }

    #[test]
    fn resolve_miss_is_transient_not_permanent() -> Result<()> {
        // A journal whose target store is missing from the loaded view is a
        // transient skip, not corruption: the same entry must repair once the
        // missing profile appears, which is exactly the single-profile ->
        // unified switch flow.
        let (_temp, _guard, source, target, before, _after) = setup_recovery_env("resolvemiss")?;
        target.update(|instances, _| {
            let mut copy = before.clone();
            copy.source_profile = target.profile().to_string();
            instances.push(copy);
            Ok(())
        })?;
        super::super::move_journal::record(
            &fresh_journal_entry(&source, &target, &before.id),
            source.sessions_path(),
        )?;

        // First pass: only the source profile is loaded (single-profile mode).
        let source_only_view: Vec<(&str, &Storage)> = vec![(source.profile(), &source)];
        let outcome = reconcile_loaded(&[&source], &source_only_view);
        assert!(!outcome.repaired, "nothing to arbitrate without the target");
        assert_eq!(
            journal_entry_count(&source),
            1,
            "entry must survive the miss"
        );

        // Second pass: unified view. The previously skipped entry repairs.
        let full_view: Vec<(&str, &Storage)> =
            vec![(source.profile(), &source), (target.profile(), &target)];
        let outcome = reconcile_loaded(&[&source, &target], &full_view);
        assert!(
            outcome.repaired,
            "resolve-miss must not poison later passes"
        );
        assert!(source.load()?.is_empty());
        assert_eq!(journal_entry_count(&source), 0);
        Ok(())
    }

    #[test]
    fn multi_id_batch_arbitrates_surviving_ids_and_skips_vanished_ones() -> Result<()> {
        // Mixed batch: id `a` is duplicated (target published -> target wins,
        // source copy removed); id `b` vanished from both stores (hand
        // resolved). The batch arbitrates `a`, consumes the journal, and
        // touches nothing for `b`.
        let (_temp, _guard, source, target, before, _after) = setup_recovery_env("multi")?;
        // Never persisted anywhere: hand-resolved before recovery ran.
        let vanished_id = Instance::new("vanished", "/repo/vanished").id;
        target.update(|instances, _| {
            let mut copy = before.clone();
            copy.source_profile = target.profile().to_string();
            instances.push(copy);
            Ok(())
        })?;
        let mut entry = fresh_journal_entry(&source, &target, &before.id);
        entry.ids.push(vanished_id.clone());
        entry.ids.sort();
        super::super::move_journal::record(&entry, source.sessions_path())?;

        let view: Vec<(&str, &Storage)> =
            vec![(source.profile(), &source), (target.profile(), &target)];
        let outcome = reconcile_loaded(&[&source, &target], &view);

        assert!(outcome.repaired, "the duplicated sibling must arbitrate");
        assert!(outcome.reports.is_empty());
        assert!(
            !source.load()?.iter().any(|row| row.id == before.id),
            "loser copy removed"
        );
        assert_eq!(target.load()?.len(), 1, "winner kept");
        assert!(
            !journal_entry_scan_ids(&source)
                .iter()
                .any(|id| id == &before.id),
            "journal consumed despite the vanished sibling"
        );
        Ok(())
    }

    #[test]
    fn post_journal_store_edits_degrade_to_legacy() -> Result<()> {
        // A losing store edited after the journal was written carries user
        // changes the journal must never overwrite: degrade instead of
        // arbitrating even though the entry itself is still young.
        let (_temp, _guard, source, target, before, _after) = setup_recovery_env("edited")?;
        target.update(|instances, _| {
            let mut copy = before.clone();
            copy.source_profile = target.profile().to_string();
            instances.push(copy);
            Ok(())
        })?;
        let mut entry = fresh_journal_entry(&source, &target, &before.id);
        entry.created_at_epoch_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as u64
            - 10 * 60 * 1000;
        super::super::move_journal::record(&entry, source.sessions_path())?;

        let view: Vec<(&str, &Storage)> =
            vec![(source.profile(), &source), (target.profile(), &target)];
        let outcome = reconcile_loaded(&[&source, &target], &view);

        assert!(
            !outcome.repaired,
            "post-journal edits must block arbitration"
        );
        assert_eq!(outcome.reports.len(), 1);
        assert_eq!(source.load()?.len(), 1);
        assert_eq!(target.load()?.len(), 1);
        assert_eq!(journal_entry_count(&source), 1);
        // The degradation is permanent (mtimes only grow relative to the
        // journal timestamp), so it lands in the log-once registry instead of
        // re-warning on every reload tick.
        let journal_path = super::super::move_journal::scan([source.sessions_path().to_path_buf()])
            .entries
            .into_iter()
            .next()
            .map(|(path, _)| path)
            .expect("entry still on disk");
        assert!(
            super::unusable_journal_entries_contains(&journal_path),
            "mtime-degraded entry is blacklisted like other permanent causes"
        );
        Ok(())
    }

    #[test]
    fn duplicate_ids_and_aliased_endpoints_are_rejected_before_locks() -> Result<()> {
        for case in ["duplicate-ids", "aliased-endpoints"] {
            let (_temp, _guard, source, target, before, _after) = setup_recovery_env(case)?;
            let mut entry = fresh_journal_entry(&source, &target, &before.id);
            let stores: Vec<(&str, &Storage)> = if case == "duplicate-ids" {
                entry.ids.push(before.id.clone());
                vec![(source.profile(), &source), (target.profile(), &target)]
            } else {
                entry.target_profile = source.profile().to_string();
                entry.target_sessions_path = source.sessions_path().to_path_buf();
                vec![(source.profile(), &source)]
            };
            let journal_path = super::super::move_journal::record(&entry, source.sessions_path())?;

            let outcome = if case == "duplicate-ids" {
                reconcile_loaded(&[&source, &target], &stores)
            } else {
                reconcile_loaded(&[&source], &stores)
            };

            assert!(!outcome.repaired, "{case}");
            assert_eq!(journal_entry_count(&source), 1, "{case}: evidence remains");
            assert!(
                super::unusable_journal_entries_contains(&journal_path),
                "{case}: semantic invalidity is permanently recorded"
            );
        }
        Ok(())
    }

    #[test]
    fn dual_storage_lock_blocks_target_only_writer() -> Result<()> {
        let (_temp, _guard, source, target, _before, _after) = setup_recovery_env("dual-lock")?;
        let target_path = target.sessions_path().to_path_buf();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let mut writer = None;

        with_two_storage_locks(&source, &target, || {
            writer = Some(std::thread::spawn(move || {
                let target = Storage::new_for_test_path("dual-lock-target", target_path);
                started_tx.send(()).unwrap();
                target
                    .update(|instances, _| {
                        instances.clear();
                        Ok(())
                    })
                    .unwrap();
                done_tx.send(()).unwrap();
            }));
            started_rx
                .recv_timeout(std::time::Duration::from_secs(1))
                .expect("writer reached the target update attempt");
            assert!(
                done_rx
                    .recv_timeout(std::time::Duration::from_millis(150))
                    .is_err(),
                "target-only writer must block while repair owns both storage flocks"
            );
            Ok(())
        })?;

        done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("target writer proceeds after dual lock release");
        writer.unwrap().join().unwrap();
        Ok(())
    }

    #[test]
    fn unresolved_newer_intent_blocks_older_overlapping_journal() -> Result<()> {
        for case in ["resolve-miss", "opaque"] {
            let (_temp, _guard, a, b, before, _after) = setup_recovery_env(case)?;
            b.update(|instances, _| {
                let mut copy = before.clone();
                copy.source_profile = b.profile().to_string();
                instances.push(copy);
                Ok(())
            })?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis() as u64;
            let mut older = fresh_journal_entry(&a, &b, &before.id);
            older.created_at_epoch_ms = now - 60_000;
            super::super::move_journal::record(&older, a.sessions_path())?;
            if case == "resolve-miss" {
                let mut newer = fresh_journal_entry(&b, &a, &before.id);
                newer.created_at_epoch_ms = now;
                newer.target_profile = "missing-profile".to_string();
                newer.target_sessions_path = a.sessions_path().with_file_name("missing.json");
                super::super::move_journal::record(&newer, b.sessions_path())?;
            } else {
                let journal_dir = b.sessions_path().parent().unwrap().join(".move-journal");
                fs::create_dir_all(&journal_dir)?;
                fs::write(
                    journal_dir.join("move-99999999999999999999-1.json"),
                    b"not-json",
                )?;
            }
            let stores: Vec<(&str, &Storage)> = vec![(a.profile(), &a), (b.profile(), &b)];

            let outcome = reconcile_loaded(&[&a, &b], &stores);

            assert!(!outcome.repaired, "{case}: older intent must not apply");
            assert_eq!(outcome.reports.len(), 1, "{case}: duplicate stays surfaced");
            assert_eq!(a.load()?.len(), 1, "{case}: newer target copy remains");
            assert_eq!(b.load()?.len(), 1, "{case}: no copy is removed");
        }
        Ok(())
    }

    #[test]
    fn shadowed_batch_propagates_block_to_every_id() -> Result<()> {
        let (_temp, _guard, a, b, x, _after) = setup_recovery_env("transitive")?;
        let mut y = Instance::new("y", "/repo/y");
        y.source_profile = a.profile().to_string();
        a.update(|instances, _| {
            instances.push(y.clone());
            Ok(())
        })?;
        b.update(|instances, _| {
            let mut x_copy = x.clone();
            x_copy.source_profile = b.profile().to_string();
            let mut y_copy = y.clone();
            y_copy.source_profile = b.profile().to_string();
            instances.extend([x_copy, y_copy]);
            Ok(())
        })?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as u64;

        let mut j1 = fresh_journal_entry(&a, &b, &y.id);
        j1.created_at_epoch_ms = now - 120_000;
        let mut j2 = fresh_journal_entry(&b, &a, &x.id);
        j2.ids.push(y.id.clone());
        j2.ids.sort();
        j2.created_at_epoch_ms = now - 60_000;
        let mut j3 = fresh_journal_entry(&a, &b, &x.id);
        j3.target_profile = "missing".to_string();
        j3.target_sessions_path = a.sessions_path().with_file_name("missing.json");
        j3.created_at_epoch_ms = now;
        super::super::move_journal::record(&j1, a.sessions_path())?;
        super::super::move_journal::record(&j2, b.sessions_path())?;
        super::super::move_journal::record(&j3, a.sessions_path())?;
        let stores: Vec<(&str, &Storage)> = vec![(a.profile(), &a), (b.profile(), &b)];

        let outcome = reconcile_loaded(&[&a, &b], &stores);

        assert!(!outcome.repaired);
        assert_eq!(outcome.reports.len(), 2);
        assert_eq!(a.load()?.len(), 2, "newer X+Y target remains intact");
        assert_eq!(b.load()?.len(), 2, "no stale journal deletes either copy");
        Ok(())
    }

    #[test]
    fn duplicate_target_rows_never_become_an_automatic_winner() -> Result<()> {
        let (_temp, _guard, source, target, before, _after) = setup_recovery_env("target-dup")?;
        target.update(|instances, _| {
            for _ in 0..2 {
                let mut copy = before.clone();
                copy.source_profile = target.profile().to_string();
                instances.push(copy);
            }
            Ok(())
        })?;
        let entry = fresh_journal_entry(&source, &target, &before.id);
        super::super::move_journal::record(&entry, source.sessions_path())?;
        let stores: Vec<(&str, &Storage)> =
            vec![(source.profile(), &source), (target.profile(), &target)];

        let outcome = reconcile_loaded(&[&source, &target], &stores);

        assert!(!outcome.repaired);
        assert_eq!(outcome.reports.len(), 1);
        assert_eq!(source.load()?.len(), 1, "source copy is preserved");
        assert_eq!(
            target.load()?.len(),
            2,
            "ambiguous target copies remain surfaced"
        );
        assert_eq!(
            journal_entry_count(&source),
            1,
            "evidence remains for manual resolution"
        );
        Ok(())
    }

    #[test]
    fn invalid_id_entry_is_permanently_insufficient() -> Result<()> {
        // An id that cannot pass validation would fail the title/lifecycle
        // lock acquisition inside every repair attempt: permanently
        // insufficient, so it blacklists like parse/version/expired causes.
        let (_temp, _guard, source, target, before, _after) = setup_recovery_env("badid")?;
        target.update(|instances, _| {
            let mut copy = before.clone();
            copy.source_profile = target.profile().to_string();
            instances.push(copy);
            Ok(())
        })?;
        let entry = super::super::move_journal::MoveJournalEntry {
            ids: vec!["../escape".to_string()],
            ..fresh_journal_entry(&source, &target, &before.id)
        };
        super::super::move_journal::record(&entry, source.sessions_path())?;
        assert_eq!(journal_entry_count(&source), 1);

        let view: Vec<(&str, &Storage)> =
            vec![(source.profile(), &source), (target.profile(), &target)];
        let outcome = reconcile_loaded(&[&source, &target], &view);

        assert!(!outcome.repaired);
        assert_eq!(journal_entry_count(&source), 1, "entry stays on disk");
        let journal_path = super::super::move_journal::scan([source.sessions_path().to_path_buf()])
            .entries
            .into_iter()
            .next()
            .map(|(path, _)| path)
            .expect("entry present");
        assert!(
            super::unusable_journal_entries_contains(&journal_path),
            "invalid-id entry is blacklisted"
        );
        Ok(())
    }

    #[test]
    fn post_repair_load_error_prevents_home_reload_and_keeps_report() -> Result<()> {
        let (temp, _guard, source, target, before, _after) = setup_recovery_env("reload-fallback")?;
        target.update(|instances, _| {
            let mut copy = before.clone();
            copy.source_profile = target.profile().to_string();
            instances.push(copy);
            Ok(())
        })?;
        let entry = fresh_journal_entry(&source, &target, &before.id);
        super::super::move_journal::record(&entry, source.sessions_path())?;
        let bad_dir = temp.path().join("bad");
        fs::create_dir_all(&bad_dir)?;
        let bad = Storage::new_for_test_path("bad", bad_dir.join("sessions.json"));
        fs::write(bad.sessions_path(), b"not-json")?;
        let stores: Vec<(&str, &Storage)> = vec![
            (source.profile(), &source),
            (target.profile(), &target),
            (bad.profile(), &bad),
        ];

        let outcome = reconcile_loaded(&[&source, &target, &bad], &stores);

        assert!(source.load()?.is_empty(), "repair reached disk");
        assert_eq!(target.load()?.len(), 1);
        assert!(
            !outcome.repaired,
            "Home must keep pre-repair loads instead of repeating the failed reload"
        );
        assert_eq!(outcome.reports.len(), 1, "ambiguity remains surfaced");
        Ok(())
    }

    #[test]
    fn target_still_holds_checks_every_loser_id() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("sessions.json");
        let winner = Instance::new("winner", "/repo/winner");
        let bystander = Instance::new("bystander", "/repo/bystander");
        let rows = vec![winner.clone(), bystander.clone()];
        fs::write(&path, serde_json::to_vec_pretty(&rows)?)?;

        // Both present -> holds. One missing -> does not hold. No file -> no.
        assert!(target_still_holds(&path, std::slice::from_ref(&winner.id))?);
        assert!(!target_still_holds(
            &path,
            &[winner.id.clone(), "gone".to_string()]
        )?);

        // A corrupt row is skipped the way Storage::load quarantines it: the
        // surviving winner still holds instead of wedging the repair into
        // retrying forever.
        let mut corrupt_row = serde_json::Map::new();
        corrupt_row.insert("id".to_string(), serde_json::Value::from(42));
        let mixed = vec![
            serde_json::Value::Object(corrupt_row),
            serde_json::to_value(&winner)?,
        ];
        fs::write(&path, serde_json::to_vec_pretty(&mixed)?)?;
        assert!(target_still_holds(&path, std::slice::from_ref(&winner.id))?);

        fs::remove_file(&path)?;
        assert!(!target_still_holds(&path, &[winner.id])?);
        Ok(())
    }

    #[test]
    fn same_profile_duplicate_id_is_surfaced() -> Result<()> {
        // An id repeated inside ONE profile (corrupt file or writer bug) is
        // ambiguous exactly like a cross-profile duplicate: it must surface,
        // not silently fail closed without a report or title marker.
        let (_temp, _guard, source, _target, before, _after) = setup_recovery_env("intraprofile")?;
        source.update(|instances, _| {
            instances.push(before.clone());
            Ok(())
        })?;

        let view: Vec<(&str, &Storage)> = vec![(source.profile(), &source)];
        let outcome = reconcile_loaded(&[&source], &view);

        assert!(!outcome.repaired);
        assert_eq!(outcome.reports.len(), 1, "the repeated id surfaces");
        let report = &outcome.reports[0];
        assert_eq!(report.id, before.id);
        assert!(report.actionable_message().contains(&before.id));
        assert_eq!(source.load()?.len(), 2, "nothing is deleted automatically");
        Ok(())
    }

    fn journal_entry_count(source: &Storage) -> usize {
        super::super::move_journal::scan([source.sessions_path().to_path_buf()])
            .entries
            .len()
    }

    fn journal_entry_scan_ids(source: &Storage) -> Vec<String> {
        super::super::move_journal::scan([source.sessions_path().to_path_buf()])
            .entries
            .into_iter()
            .filter_map(|(_, parsed)| parsed.ok())
            .flat_map(|entry| entry.ids)
            .collect()
    }

    #[test]
    fn group_repair_scope_matches_apply_group_move() -> Result<()> {
        // Table over subtree mode: an explicit memberless child under the
        // moved path survives a single-group repair (apply_group_move's
        // non-subtree branch preserves explicit descendants) and is pruned
        // for a subtree move. Either way the losing path itself is pruned.
        // Single move: apply_group_move's non-subtree branch keeps the
        // moved-path row alive while an explicit child survives. Subtree
        // move: the whole moved namespace goes.
        let cases = [
            ("gscope-single", false, true, true),
            ("gscope-subtree", true, false, false),
        ];
        for (tag, move_subtree, path_survives, child_survives) in cases {
            let (_temp, _guard, source, target, before, _after) = setup_recovery_env(tag)?;
            source.update(|_instances, groups| {
                let mut child = Group::new("archive", "work/archive");
                child.collapsed = true;
                groups.push(child);
                Ok(())
            })?;
            target.update(|instances, _| {
                let mut copy = before.clone();
                copy.source_profile = target.profile().to_string();
                instances.push(copy);
                Ok(())
            })?;
            let entry = super::super::move_journal::MoveJournalEntry {
                group_move_subtree: move_subtree,
                ..fresh_journal_entry(&source, &target, &before.id)
            };
            super::super::move_journal::record(&entry, source.sessions_path())?;

            let view: Vec<(&str, &Storage)> =
                vec![(source.profile(), &source), (target.profile(), &target)];
            let outcome = reconcile_loaded(&[&source, &target], &view);

            assert!(outcome.repaired, "{tag}");
            assert!(outcome.reports.is_empty(), "{tag}");
            assert!(source.load()?.is_empty(), "{tag}: loser emptied");
            let source_groups = source.load_with_groups()?.1;
            assert_eq!(
                source_groups.iter().any(|group| group.path == "work"),
                path_survives,
                "{tag}: moved-path row must mirror apply_group_move"
            );
            assert_eq!(
                source_groups
                    .iter()
                    .any(|group| group.path == "work/archive"),
                child_survives,
                "{tag}: explicit descendant handling must mirror apply_group_move"
            );
            assert_eq!(journal_entry_count(&source), 0, "{tag}");
        }
        Ok(())
    }

    #[test]
    fn chained_move_journals_apply_newest_intent_first() -> Result<()> {
        // J1 records A -> B and leaks after completion. J2 is the newer
        // reverse B -> A move and crashes after publishing A, leaving both
        // stores populated. The newest intent must win.
        let (_temp, _guard, a, b, before, _after) = setup_recovery_env("chain")?;
        b.update(|instances, _| {
            let mut copy = before.clone();
            copy.source_profile = b.profile().to_string();
            instances.push(copy);
            Ok(())
        })?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as u64;
        let mut j1 = fresh_journal_entry(&a, &b, &before.id);
        j1.created_at_epoch_ms = now;
        let mut j2 = fresh_journal_entry(&b, &a, &before.id);
        j2.group_move_source_path = "moved".to_string();
        j2.group_move_target_path = "work".to_string();
        j2.created_at_epoch_ms = now;
        super::super::move_journal::record(&j1, a.sessions_path())?;
        super::super::move_journal::record(&j2, b.sessions_path())?;

        let view: Vec<(&str, &Storage)> = vec![(a.profile(), &a), (b.profile(), &b)];
        let outcome = reconcile_loaded(&[&a, &b], &view);

        assert!(outcome.repaired);
        assert!(outcome.reports.is_empty());
        assert_eq!(a.load()?.len(), 1, "new reverse-move target survives");
        assert!(b.load()?.is_empty(), "superseded profile is emptied");
        assert_eq!(journal_entry_count(&a), 0);
        assert_eq!(journal_entry_count(&b), 0);
        Ok(())
    }

    #[test]
    fn group_repair_preserves_indirect_metadata_and_other_side_name() -> Result<()> {
        let (_temp, _guard, source, target, before, _after) = setup_recovery_env("gparity")?;
        let archived_at = chrono::Utc::now();
        source.update(|_instances, groups| {
            let work = groups
                .iter_mut()
                .find(|group| group.path == "work")
                .unwrap();
            work.collapsed = true;
            work.archived_at = Some(archived_at);
            let mut indirect = Group::new("b", "work/a/b");
            indirect.collapsed = true;
            groups.push(indirect);
            let mut bystander = Group::new("moved", "moved");
            bystander.collapsed = true;
            groups.push(bystander);
            Ok(())
        })?;
        target.update(|instances, _| {
            let mut copy = before.clone();
            copy.source_profile = target.profile().to_string();
            instances.push(copy);
            Ok(())
        })?;
        let entry = fresh_journal_entry(&source, &target, &before.id);
        super::super::move_journal::record(&entry, source.sessions_path())?;

        let view: Vec<(&str, &Storage)> =
            vec![(source.profile(), &source), (target.profile(), &target)];
        let outcome = reconcile_loaded(&[&source, &target], &view);

        assert!(outcome.repaired);
        let groups = source.load_with_groups()?.1;
        let work = groups.iter().find(|group| group.path == "work").unwrap();
        assert!(
            work.collapsed,
            "indirect descendant preserves parent metadata"
        );
        assert_eq!(work.archived_at, Some(archived_at));
        assert!(groups.iter().any(|group| group.path == "work/a/b"));
        let bystander = groups.iter().find(|group| group.path == "moved").unwrap();
        assert!(
            bystander.collapsed,
            "other-side name is unrelated on source"
        );
        Ok(())
    }

    #[test]
    fn journal_is_durable_before_external_effect_runs() -> Result<()> {
        let (_temp, _guard, source, target, before, after) = setup_recovery_env("effect-order")?;
        let effect_ran = std::cell::Cell::new(false);
        let crash = ArmedCrashPoint::arm("profile-move-journal");
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = source.move_instances_to_inner(
                &target,
                &[(before, after)],
                MoveTransactionPlan {
                    group_move: &GroupMovePlan::single("work", "moved"),
                    merge_complete_post: true,
                },
                |_existing, _candidates| Ok(()),
                |_| {
                    effect_ran.set(true);
                    Ok(())
                },
                sync_resolved_parent_directory,
            );
        }));
        drop(crash);
        assert!(result.is_err(), "journal crash point must fire");
        assert!(
            !effect_ran.get(),
            "journal must precede the external effect"
        );
        assert_eq!(journal_entry_count(&source), 1);
        Ok(())
    }

    #[test]
    fn failed_repair_directory_sync_retains_journal() -> Result<()> {
        let (_temp, _guard, source, target, before, _after) = setup_recovery_env("sync-fail")?;
        target.update(|instances, _| {
            let mut copy = before.clone();
            copy.source_profile = target.profile().to_string();
            instances.push(copy);
            Ok(())
        })?;
        let entry = fresh_journal_entry(&source, &target, &before.id);
        let journal_path = super::super::move_journal::record(&entry, source.sessions_path())?;
        let stores: Vec<(&str, &Storage)> =
            vec![(source.profile(), &source), (target.profile(), &target)];
        let error = repair_journal_entry_with_sync(&entry, &stores, &journal_path, |_path| {
            Err(anyhow!("forced repaired-profile sync failure"))
        })
        .expect_err("failed durability barrier must fail recovery completion");
        assert!(error.to_string().contains("not made durable"));
        assert!(source.load()?.is_empty(), "repair row write reached disk");
        assert_eq!(journal_entry_count(&source), 1, "evidence must remain");

        let retry_error = repair_journal_entry_with_sync(&entry, &stores, &journal_path, |_path| {
            Err(anyhow!("forced retry sync failure"))
        })
        .expect_err("no-loser retry must repeat the durability barrier");
        assert!(retry_error.to_string().contains("not made durable"));
        assert_eq!(journal_entry_count(&source), 1, "retry keeps evidence too");

        let outcome = reconcile_loaded(&[&source, &target], &stores);
        assert!(outcome.repaired, "rerun consumes retained evidence safely");
        assert_eq!(journal_entry_count(&source), 0);
        Ok(())
    }

    #[test]
    fn repaired_profile_sync_covers_sessions_and_groups_paths() -> Result<()> {
        let (_temp, _guard, source, _target, _before, _after) = setup_recovery_env("sync-both")?;
        let mut calls = Vec::new();

        sync_repaired_profile_durably(&source, |path| {
            calls.push(path.to_path_buf());
            Ok(())
        })?;

        assert_eq!(
            calls,
            vec![
                source.sessions_path().to_path_buf(),
                source.sessions_path().with_file_name("groups.json"),
            ]
        );
        Ok(())
    }

    #[test]
    fn backup_pruning_syncs_the_lexical_backup_directory() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("profile/sessions.json");
        fs::create_dir_all(path.parent().unwrap())?;
        for stamp in 1..=4 {
            fs::write(
                path.with_file_name(format!("sessions.json.pre-recovery-{stamp}")),
                stamp.to_string(),
            )?;
        }
        let mut synced = Vec::new();

        prune_old_recovery_backups_with_sync(&path, 3, |candidate| {
            synced.push(candidate.to_path_buf());
            Ok(())
        })?;

        assert_eq!(synced, vec![path]);
        Ok(())
    }

    #[test]
    fn recovery_backup_retention_keeps_newest_three() -> Result<()> {
        let temp = tempdir()?;
        let path = temp.path().join("sessions.json");
        fs::write(&path, b"[]")?;
        for stamp in 1..=5 {
            fs::write(
                temp.path()
                    .join(format!("sessions.json.pre-recovery-{stamp}")),
                stamp.to_string(),
            )?;
        }
        backup_before_repair(&path)?;
        let mut stamps: Vec<u128> = fs::read_dir(temp.path())?
            .filter_map(|entry| entry.ok().map(|entry| entry.file_name()))
            .filter_map(|name| {
                name.to_string_lossy()
                    .strip_prefix("sessions.json.pre-recovery-")
                    .and_then(|value| value.parse().ok())
            })
            .collect();
        stamps.sort();
        assert_eq!(stamps.len(), RECOVERY_BACKUPS_TO_KEEP);
        assert_eq!(&stamps[..2], &[4, 5], "oldest backups are pruned");
        Ok(())
    }

    #[test]
    fn post_repair_load_error_preserves_pre_repair_report() -> Result<()> {
        let temp = tempdir()?;
        let good_dir = temp.path().join("good");
        let bad_dir = temp.path().join("bad");
        fs::create_dir_all(&good_dir)?;
        fs::create_dir_all(&bad_dir)?;
        let good = Storage::new_for_test_path("good", good_dir.join("sessions.json"));
        let bad = Storage::new_for_test_path("bad", bad_dir.join("sessions.json"));
        let row = Instance::new("duplicate", "/repo/duplicate");
        good.update(|instances, _| {
            instances.push(row.clone());
            Ok(())
        })?;
        fs::write(bad.sessions_path(), b"not-json")?;
        let good_rows = good.load()?;
        let fallback_bad = vec![row.clone()];
        let fallback: Vec<(&str, &[Instance])> = vec![
            ("good", good_rows.as_slice()),
            ("bad", fallback_bad.as_slice()),
        ];
        let stores: Vec<(&str, &Storage)> = vec![("good", &good), ("bad", &bad)];

        let (reports, reload_succeeded) = reports_after_repair(&fallback, &stores);

        assert!(!reload_succeeded, "Home must keep its pre-repair loads");
        assert_eq!(reports.len(), 1, "load error keeps ambiguity surfaced");
        assert_eq!(reports[0].id, row.id);
        Ok(())
    }

    fn fresh_journal_entry(
        source: &Storage,
        target: &Storage,
        id: &str,
    ) -> super::super::move_journal::MoveJournalEntry {
        super::super::move_journal::MoveJournalEntry {
            version: super::super::move_journal::MOVE_JOURNAL_VERSION,
            ids: vec![id.to_string()],
            source_profile: source.profile().to_string(),
            target_profile: target.profile().to_string(),
            source_sessions_path: source.sessions_path().to_path_buf(),
            target_sessions_path: target.sessions_path().to_path_buf(),
            group_move_source_path: "work".to_string(),
            group_move_target_path: "moved".to_string(),
            group_move_subtree: false,
            created_at_epoch_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or_default(),
        }
    }

    #[test]
    fn recovery_survives_a_panic_mid_repair_and_stays_idempotent() -> Result<()> {
        // A panic between the repair write and the journal consumption must
        // leave a state a plain rerun converges on, and further reruns are
        // exact no-ops.
        let (_temp, _guard, source, target, _before, _after) = setup_recovery_env("midpanic")?;
        run_crashing_move(&source, &target, "profile-move-source-sessions");
        assert_eq!(journal_entry_count(&source), 1);

        let view: Vec<(&str, &Storage)> =
            vec![(source.profile(), &source), (target.profile(), &target)];
        {
            // Guard scoped to the interrupted pass only, so its Drop runs
            // before the converging rerun below.
            let _crash = ArmedCrashPoint::arm("profile-repair-source-written");
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                reconcile_loaded(&[&source, &target], &view);
            }));
        }
        // The interrupted pass wrote the repaired source but died before
        // consuming the journal; the rerun finishes the job.
        let outcome = reconcile_loaded(&[&source, &target], &view);
        assert!(outcome.repaired || journal_entry_count(&source) == 0);
        assert!(outcome.reports.is_empty());
        assert!(source.load()?.is_empty());
        assert_eq!(target.load()?.len(), 1);
        assert_eq!(journal_entry_count(&source), 0);

        let outcome = reconcile_loaded(&[&source, &target], &view);
        assert!(
            !outcome.repaired && outcome.reports.is_empty(),
            "rerun is a no-op"
        );
        Ok(())
    }

    /// Run one reconciliation pass over freshly loaded copies of the given
    /// storages, matching the production call shape (borrowed views only).
    fn reconcile_loaded(
        storages: &[&Storage],
        view: &[(&str, &Storage)],
    ) -> super::ReconciliationOutcome {
        let loaded = collect_loaded(storages);
        let refs: Vec<(&str, &[Instance])> = loaded
            .iter()
            .map(|(name, instances)| (name.as_str(), instances.as_slice()))
            .collect();
        super::reconcile_profile_duplicates(&refs, view)
    }

    fn collect_loaded(storages: &[&Storage]) -> Vec<(String, Vec<Instance>)> {
        storages
            .iter()
            .map(|storage| {
                (
                    storage.profile().to_string(),
                    storage.load().unwrap_or_default(),
                )
            })
            .collect()
    }

    fn count_recovery_backups(sessions_path: &Path) -> usize {
        let dir = match sessions_path.parent() {
            Some(dir) => dir,
            None => return 0,
        };
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .filter(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .contains(".pre-recovery-")
                    })
                    .count()
            })
            .unwrap_or(0)
    }
}
