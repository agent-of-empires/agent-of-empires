//! Hardened access to the AoE hook status directory, the single Rust entry
//! point for every read, write, and cleanup under `/tmp/aoe-hooks-<euid>`
//! (#1844) and the sandbox hook base under the app dir.
//!
//! A sandboxed row's container binds `<app dir>/sandbox-hooks/<id>`, which
//! VM runtimes such as Colima share because it is under `$HOME`; `/tmp` is
//! not shared there (#5960). A row's dir is the host one when it exists, else
//! the sandbox one, so a container still bound to `/tmp` is never hidden by an
//! empty sandbox dir. Only [`switch_instance_dir`] moves a row between them,
//! under a per-id lock that every creating open also takes.
//!
//! The base directory is created `0o700`, opened with `O_DIRECTORY |
//! O_NOFOLLOW`, then verified by `fstat` on the resulting fd, which pins the
//! inode against a later path swap: wrong type, wrong uid, or any group or
//! world bit rejects. That verified `OwnedFd` is cached, and every
//! per-instance subdirectory and file rides `*at` calls anchored on it. A
//! failure caches the error rather than retrying, so a bad state stays
//! visible.
//!
//! The euid suffix keeps two users on a shared host from colliding. A
//! squatter owning the path first, a `/tmp` reaper unlinking it while the fd
//! is held, or an operator-widened POSIX ACL (only mode bits are verified)
//! each degrade to hooks disabled and pane detection, never to escalation:
//! an alien uid cannot `setfacl` on a `0o700` directory we own.

use std::fs::Metadata;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(not(test))]
use std::sync::OnceLock;

#[cfg(test)]
use std::os::fd::AsRawFd;

use anyhow::{anyhow, bail, Context, Result};
use nix::errno::Errno;
use nix::fcntl::{open, openat, renameat, AtFlags, OFlag};
use nix::libc;
use nix::sys::stat::{fchmod, fstat, fstatat, mkdirat, Mode};
use nix::unistd::{geteuid, mkdir, unlinkat, UnlinkatFlags};

// Path resolution.

#[cfg(test)]
thread_local! {
    /// Test-only base path override. A test using it must also call
    /// `reset_for_test` and serialize via `serial_test::serial(hook_base)`.
    static HOOK_BASE_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
    /// Test-only sandbox base, for a fixture whose two bases must not share a parent.
    static SANDBOX_HOOK_BASE_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Per-user host base path. The suffix is `geteuid()`, not `getuid()`: the
/// agent writes through `id -u`, so both ends agree.
pub(crate) fn hook_base_path() -> PathBuf {
    #[cfg(test)]
    {
        if let Some(p) = HOOK_BASE_OVERRIDE.with(|c| c.borrow().clone()) {
            return p;
        }
    }
    PathBuf::from(format!("/tmp/aoe-hooks-{}", geteuid().as_raw()))
}

/// The two bases a row's hook dir can live under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookBase {
    Host,
    Sandbox,
}

impl HookBase {
    fn path(self) -> Result<PathBuf> {
        match self {
            Self::Host => Ok(hook_base_path()),
            Self::Sandbox => sandbox_hook_base_path(),
        }
    }

    fn other(self) -> Self {
        match self {
            Self::Host => Self::Sandbox,
            Self::Sandbox => Self::Host,
        }
    }

    fn label(self, instance_id: &str) -> PathBuf {
        self.path()
            .map(|base| base.join(instance_id))
            .unwrap_or_else(|_| PathBuf::from(instance_id))
    }
}

/// Bind source base for sandboxed rows. Tests keep it beside the (overridden) host base, unless
/// they set it apart.
pub(crate) fn sandbox_hook_base_path() -> Result<PathBuf> {
    #[cfg(test)]
    {
        if let Some(p) = SANDBOX_HOOK_BASE_OVERRIDE.with(|c| c.borrow().clone()) {
            return Ok(p);
        }
        let host = hook_base_path();
        let name = host
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(host.with_file_name(format!("{name}-sandbox")))
    }
    #[cfg(not(test))]
    {
        Ok(crate::session::get_app_dir()?.join("sandbox-hooks"))
    }
}

#[cfg(test)]
pub(crate) fn override_base_for_test(path: PathBuf) {
    HOOK_BASE_OVERRIDE.with(|c| *c.borrow_mut() = Some(path));
}

#[cfg(test)]
pub(crate) fn override_sandbox_base_for_test(path: PathBuf) {
    SANDBOX_HOOK_BASE_OVERRIDE.with(|c| *c.borrow_mut() = Some(path));
}

#[cfg(test)]
pub(crate) fn clear_base_override_for_test() {
    HOOK_BASE_OVERRIDE.with(|c| *c.borrow_mut() = None);
    SANDBOX_HOOK_BASE_OVERRIDE.with(|c| *c.borrow_mut() = None);
}

// Singleton cell.

type CachedBase = std::result::Result<OwnedFd, Arc<anyhow::Error>>;

// `static` so the owned fd lives for the program lifetime and its `close` on
// drop never fires; `with_base` only lends it for a closure call.
#[cfg(not(test))]
static HOOK_BASE: OnceLock<CachedBase> = OnceLock::new();
#[cfg(not(test))]
static SANDBOX_HOOK_BASE: OnceLock<CachedBase> = OnceLock::new();

#[cfg(test)]
thread_local! {
    /// Per-thread shadow of `HOOK_BASE` and `SANDBOX_HOOK_BASE`, since a test
    /// cannot reset a process-wide `OnceLock`. Production never touches it.
    static HOOK_BASE_TEST_CELL: std::cell::RefCell<[Option<CachedBase>; 2]> =
        const { std::cell::RefCell::new([None, None]) };

    /// Runs inside [`switch_instance_dir`] between its two steps.
    static SWITCH_PAUSE_FOR_TEST: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    HOOK_BASE_TEST_CELL.with(|c| *c.borrow_mut() = [None, None]);
    OPEN_CALLS.store(0, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn pause_switch_for_test(pause: impl FnOnce() + 'static) {
    SWITCH_PAUSE_FOR_TEST.with(|slot| *slot.borrow_mut() = Some(Box::new(pause)));
}

/// Told the lock file's path when an instance lock is contended.
#[cfg(test)]
pub(crate) static LOCK_CONTENTION_FOR_TEST: std::sync::Mutex<
    Option<std::sync::mpsc::Sender<PathBuf>>,
> = std::sync::Mutex::new(None);

// Syscall counter the caching tests read. Production never observes it.
#[cfg(test)]
static OPEN_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn open_calls() -> usize {
    OPEN_CALLS.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
fn cached_get_or_init_apply<I, A, T>(kind: HookBase, init: I, apply: A) -> Result<T>
where
    I: FnOnce() -> std::result::Result<OwnedFd, Arc<anyhow::Error>>,
    A: FnOnce(&std::result::Result<OwnedFd, Arc<anyhow::Error>>) -> Result<T>,
{
    // The `borrow_mut` is held for the whole call, so a closure that
    // re-enters `with_base` would panic with `BorrowMutError`.
    HOOK_BASE_TEST_CELL.with(|cell| {
        let mut slots = cell.borrow_mut();
        let slot = &mut slots[kind as usize];
        if slot.is_none() {
            *slot = Some(init());
        }
        apply(slot.as_ref().unwrap())
    })
}

#[cfg(not(test))]
fn cached_get_or_init_apply<I, A, T>(kind: HookBase, init: I, apply: A) -> Result<T>
where
    I: FnOnce() -> std::result::Result<OwnedFd, Arc<anyhow::Error>>,
    A: FnOnce(&std::result::Result<OwnedFd, Arc<anyhow::Error>>) -> Result<T>,
{
    let cell = match kind {
        HookBase::Host => &HOOK_BASE,
        HookBase::Sandbox => &SANDBOX_HOOK_BASE,
    };
    apply(cell.get_or_init(init))
}

/// Open and verify a hook base directory once, then run `f` with a borrowed
/// fd to it; later callers reuse the cached fd or cached error. The closure
/// shape is what keeps the fd from escaping its borrow.
fn with_base<F, T>(kind: HookBase, f: F) -> Result<T>
where
    F: FnOnce(BorrowedFd<'_>) -> Result<T>,
{
    cached_get_or_init_apply(
        kind,
        || match open_and_verify_base(kind) {
            Ok(fd) => Ok(fd),
            Err(e) => {
                tracing::error!(
                    target: "hooks.guard",
                    "hook base init failed: {e:#}. AoE will fall back to pane-detection. \
                     Recover: rm -rf {}",
                    kind.label("").display()
                );
                Err(Arc::new(e))
            }
        },
        |entry| match entry {
            Ok(fd) => f(fd.as_fd()),
            Err(e) => Err(anyhow!("{e:#}")),
        },
    )
}

fn open_and_verify_base(kind: HookBase) -> Result<OwnedFd> {
    #[cfg(test)]
    if kind == HookBase::Host {
        OPEN_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    let path = kind.path()?;

    // 1. mkdir(0o700) tolerating EEXIST.
    match mkdir(&path, Mode::S_IRWXU) {
        Ok(()) => {}
        Err(Errno::EEXIST) => {}
        Err(e) => {
            return Err(e).with_context(|| format!("mkdir {}", path.display()));
        }
    }

    // 2. open(O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC | O_RDONLY). O_NOFOLLOW
    //    checks only the final component, so a prefix symlink such as macOS
    //    /tmp -> /private/tmp is still followed.
    let fd: OwnedFd = open(
        &path,
        OFlag::O_DIRECTORY
            | OFlag::O_NOFOLLOW
            | OFlag::O_CLOEXEC
            | OFlag::O_RDONLY
            | OFlag::O_NONBLOCK,
        Mode::empty(),
    )
    .with_context(|| {
        format!(
            "open hook base {} refused (symlink or non-directory). Recover: rm -rf {}",
            path.display(),
            path.display()
        )
    })?;

    // 3. fstat on the fd, which pins the inode for the fd's lifetime.
    verify_dir_metadata(&fd, &path)?;

    Ok(fd)
}

/// Common verification: `S_IFDIR`, owned by euid, no group/other bits.
fn verify_dir_metadata(fd: &OwnedFd, label: &std::path::Path) -> Result<()> {
    let st = fstat(fd).with_context(|| format!("fstat {}", label.display()))?;
    let euid = geteuid().as_raw();
    let mode = st.st_mode & 0o7777;
    if (st.st_mode & libc::S_IFMT) != libc::S_IFDIR {
        bail!("{} is not a directory", label.display());
    }
    if st.st_uid != euid {
        bail!(
            "{} owned by uid={}, expected euid={}. Recover: rm -rf {} (or wait for owner to log out)",
            label.display(),
            st.st_uid,
            euid,
            label.display()
        );
    }
    if mode & 0o077 != 0 {
        bail!(
            "{} mode {:o} permits group/world access (expected 0o700). Recover: rm -rf {}",
            label.display(),
            mode,
            label.display()
        );
    }
    if mode & 0o7000 != 0 {
        bail!(
            "{} mode {:o} has setuid/setgid/sticky bits set (expected 0o700). \
             We never set these on hook directories; presence indicates a hostile \
             or misconfigured pre-creation. Recover: rm -rf {}",
            label.display(),
            mode,
            label.display()
        );
    }
    Ok(())
}

// Per-instance.

/// The row's existing dir, or a new host one, as an owned fd. Takes the
/// row's switch lock, so it never creates a host dir while a switch is under way.
pub(crate) fn open_instance_dir(instance_id: &str) -> Result<OwnedFd> {
    Ok(resolve_or_create(instance_id)?.1)
}

fn resolve_or_create(instance_id: &str) -> Result<(HookBase, OwnedFd)> {
    crate::session::validate_instance_id(instance_id)?;
    let _lock = lock_instance(instance_id);
    if let Some(found) = existing_instance_dir(instance_id)? {
        return Ok(found);
    }
    Ok((HookBase::Host, create_child(HookBase::Host, instance_id)?))
}

/// The host dir when it exists, else the sandbox one.
fn existing_instance_dir(instance_id: &str) -> Result<Option<(HookBase, OwnedFd)>> {
    let host = open_child(HookBase::Host, instance_id);
    if let Ok(Some(fd)) = host {
        return Ok(Some((HookBase::Host, fd)));
    }
    if let Ok(Some(fd)) = open_child(HookBase::Sandbox, instance_id) {
        return Ok(Some((HookBase::Sandbox, fd)));
    }
    host.map(|_| None)
}

fn open_child(kind: HookBase, instance_id: &str) -> Result<Option<OwnedFd>> {
    with_base(kind, |base| {
        open_instance_child(base, instance_id, &kind.label(instance_id))
    })
}

/// `mkdirat(base, id, 0o700)` (EEXIST-tolerant) plus `openat(O_NOFOLLOW)` plus
/// `fstat`-on-fd uid/mode check.
fn create_child(kind: HookBase, instance_id: &str) -> Result<OwnedFd> {
    with_base(kind, |base| {
        match mkdirat(base, instance_id, Mode::S_IRWXU) {
            Ok(()) | Err(Errno::EEXIST) => {}
            Err(e) => {
                return Err(e).with_context(|| format!("mkdirat {instance_id}"));
            }
        }
        let fd: OwnedFd = openat(
            base,
            instance_id,
            OFlag::O_DIRECTORY
                | OFlag::O_NOFOLLOW
                | OFlag::O_CLOEXEC
                | OFlag::O_RDONLY
                | OFlag::O_NONBLOCK,
            Mode::empty(),
        )
        .with_context(|| format!("openat instance subdir {instance_id} (symlink or non-dir)"))?;
        verify_dir_metadata(&fd, &kind.label(instance_id))?;
        Ok(fd)
    })
}

/// Exclusive `flock` on `<sandbox base>/.<id>.lock`, outside any bound dir. `None` when it
/// can't be taken: the sandbox base is unusable, or the lock file can't be opened or locked.
/// Cleanup unlinks the name while holding it, so a lock is kept only while the name still
/// refers to the file locked; otherwise a later opener would lock a new file beside it.
fn lock_instance(instance_id: &str) -> Option<std::fs::File> {
    let name = format!(".{instance_id}.lock");
    for _ in 0..3 {
        let file = lock_instance_file(instance_id, &name)?;
        let held = fstat(&file).ok();
        let current = with_base(HookBase::Sandbox, |base| {
            Ok(fstatat(base, name.as_str(), AtFlags::AT_SYMLINK_NOFOLLOW).ok())
        })
        .ok()
        .flatten();
        if held.zip(current).is_some_and(|(held, current)| {
            (held.st_dev, held.st_ino) == (current.st_dev, current.st_ino)
        }) {
            return Some(file);
        }
    }
    tracing::warn!(target: "hooks.guard", "switch lock for {instance_id} kept being replaced");
    None
}

fn lock_instance_file(instance_id: &str, name: &str) -> Option<std::fs::File> {
    let file = with_base(HookBase::Sandbox, |base| {
        let fd = openat(
            base,
            name,
            OFlag::O_CREAT
                | OFlag::O_RDWR
                | OFlag::O_NOFOLLOW
                | OFlag::O_CLOEXEC
                | OFlag::O_NONBLOCK,
            Mode::S_IRUSR | Mode::S_IWUSR,
        )
        .with_context(|| format!("open switch lock for {instance_id}"))?;
        Ok(std::fs::File::from(fd))
    })
    .map_err(|e| tracing::warn!(target: "hooks.guard", "switch lock unavailable: {e:#}"))
    .ok()?;
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            #[cfg(test)]
            if let Some(sender) = LOCK_CONTENTION_FOR_TEST.lock().unwrap().as_ref() {
                let _ = sender.send(HookBase::Sandbox.label(instance_id));
            }
            file.lock()
                .map_err(|e| tracing::warn!(target: "hooks.guard", "switch lock: {e}"))
                .ok()?;
        }
        Err(std::fs::TryLockError::Error(e)) => {
            tracing::warn!(target: "hooks.guard", "switch lock: {e}");
            return None;
        }
    }
    Some(file)
}

/// Moves a row's hook dir to `to`: creates it there, then removes the other, under the row's
/// switch lock so a creating open waits rather than recreating the dir being removed. Returns
/// the canonical path, for a bind-mount source. Without the lock it changes nothing.
pub(crate) fn switch_instance_dir(instance_id: &str, to: HookBase) -> Result<PathBuf> {
    crate::session::validate_instance_id(instance_id)?;
    let _lock = lock_instance(instance_id)
        .with_context(|| format!("switch lock for {instance_id} unavailable"))?;
    create_child(to, instance_id)?;
    #[cfg(test)]
    if let Some(pause) = SWITCH_PAUSE_FOR_TEST.with(|slot| slot.borrow_mut().take()) {
        pause();
    }
    remove_child(to.other(), instance_id)?;
    let lexical = to.path()?.join(instance_id);
    std::fs::canonicalize(&lexical)
        .with_context(|| format!("canonicalize hook dir {}", lexical.display()))
}

/// Read-only variant: never creates the dir. Returns `Ok(None)` on `ENOENT` /
/// `ELOOP` (legitimate transient absence or hostile symlink swap, both
/// indistinguishable from "no hook fired yet" on the polling path).
pub(crate) fn open_instance_dir_read_only(instance_id: &str) -> Result<Option<OwnedFd>> {
    crate::session::validate_instance_id(instance_id)?;
    Ok(existing_instance_dir(instance_id)?.map(|(_, fd)| fd))
}

pub(crate) fn open_recorded_instance_dir(
    instance_id: &str,
    directory: &std::path::Path,
) -> Result<Option<OwnedFd>> {
    crate::session::validate_instance_id(instance_id)?;
    anyhow::ensure!(
        directory.file_name() == Some(std::ffi::OsStr::new(instance_id)),
        "recorded hook directory does not belong to this instance"
    );
    let parent = directory
        .parent()
        .context("recorded hook directory has no parent")?;
    let base = open(
        parent,
        OFlag::O_DIRECTORY
            | OFlag::O_NOFOLLOW
            | OFlag::O_CLOEXEC
            | OFlag::O_RDONLY
            | OFlag::O_NONBLOCK,
        Mode::empty(),
    )?;
    verify_dir_metadata(&base, parent)?;
    open_instance_child(base.as_fd(), instance_id, directory)
}

fn open_instance_child(
    base: BorrowedFd<'_>,
    instance_id: &str,
    label: &std::path::Path,
) -> Result<Option<OwnedFd>> {
    match openat(
        base,
        instance_id,
        OFlag::O_DIRECTORY
            | OFlag::O_NOFOLLOW
            | OFlag::O_CLOEXEC
            | OFlag::O_RDONLY
            | OFlag::O_NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => {
            verify_dir_metadata(&fd, label)?;
            Ok(Some(fd))
        }
        Err(Errno::ENOENT) | Err(Errno::ELOOP) => Ok(None),
        Err(e) => Err(e)
            .with_context(|| format!("openat instance subdir {instance_id} (symlink or non-dir)")),
    }
}

// Per-file I/O.

/// Reject leaf names that would escape the verified parent dirfd: an
/// absolute leaf makes the kernel ignore the dirfd, a separator traverses
/// into nested entries, `..` walks up, `.` is the parent, and NUL would
/// fail `openat` anyway. A leading `.` is allowed, since `write_atomic`
/// and the agent-side snippet both name tmpfiles `.{name}.tmp.{pid}.{n}`.
fn validate_hook_leaf(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." {
        bail!("invalid hook leaf name: {name:?}");
    }
    if name.as_bytes().iter().any(|&b| b == b'/' || b == 0) {
        bail!("hook leaf name contains separator or NUL: {name:?}");
    }
    Ok(())
}

/// Open a file in an already-verified per-instance dir for reading, with
/// `O_NOFOLLOW`. `ENOENT`, `ELOOP` and any non-regular leaf map to
/// `Ok(None)`: only regular files are valid hook sidecars, matching the
/// `S_IFREG` gate in `remove_instance_dir`.
pub(crate) fn read_file_at(
    dir: BorrowedFd<'_>,
    name: &str,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>> {
    Ok(read_file_with_metadata_at(dir, name, max_bytes)?.map(|(_, bytes)| bytes))
}

/// Read a bounded regular file and return metadata from the same opened inode.
pub(crate) fn read_file_with_metadata_at(
    dir: BorrowedFd<'_>,
    name: &str,
    max_bytes: usize,
) -> Result<Option<(Metadata, Vec<u8>)>> {
    use std::io::Read;
    validate_hook_leaf(name)?;
    let fd = match openat(
        dir,
        name,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::ENOENT) | Err(Errno::ELOOP) => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("openat read {name}")),
    };
    let mut file = std::fs::File::from(fd);
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Ok(None);
    }
    let mut bytes = Vec::with_capacity(max_bytes.min(4096));
    file.by_ref()
        .take(u64::try_from(max_bytes).unwrap_or(u64::MAX))
        .read_to_end(&mut bytes)?;
    Ok(Some((metadata, bytes)))
}

/// `fstatat(AT_SYMLINK_NOFOLLOW)` view for mtime gating. Returns `Ok(None)` on
/// missing or symlinked entries.
pub(crate) fn metadata_at(dir: BorrowedFd<'_>, name: &str) -> Result<Option<Metadata>> {
    validate_hook_leaf(name)?;
    let fd = match openat(
        dir,
        name,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::ENOENT) | Err(Errno::ELOOP) => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("openat metadata {name}")),
    };
    let file = std::fs::File::from(fd);
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Ok(None);
    }
    Ok(Some(meta))
}

/// Single-shot truncating write, for last-writer-wins content like
/// `<dir>/status`. Production writes that file from the agent-side shell
/// snippet; this exists so test fixtures plant it under the same
/// `*at`-anchored discipline.
#[cfg(test)]
pub(crate) fn write_short(dir: BorrowedFd<'_>, name: &str, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    validate_hook_leaf(name)?;
    let fd = openat(
        dir,
        name,
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_TRUNC | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::S_IRUSR | Mode::S_IWUSR,
    )
    .with_context(|| format!("openat write_short {name}"))?;
    let mut file = std::fs::File::from(fd);
    file.write_all(bytes)?;
    Ok(())
}

/// Atomic `0o600` sidecar write via an `O_CREAT|O_EXCL` tmpfile and `renameat`.
///
/// Atomicity, not durability: no `fsync` before the rename, so a power loss
/// may revert or drop the file. The tree lives under `/tmp` and every reader
/// is stale-tolerant, so that is fine; `crate::session::atomic_write` is the
/// durable counterpart. The tmp name carries the pid and a process-local
/// counter so concurrent writers of one `name` cannot collide on `O_EXCL`.
pub(crate) fn write_atomic(dir: BorrowedFd<'_>, name: &str, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    validate_hook_leaf(name)?;
    static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);
    let tmp = format!(
        ".{name}.tmp.{}.{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let fd = openat(
        dir,
        tmp.as_str(),
        OFlag::O_WRONLY
            | OFlag::O_CREAT
            | OFlag::O_EXCL
            | OFlag::O_TRUNC
            | OFlag::O_NOFOLLOW
            | OFlag::O_CLOEXEC,
        Mode::S_IRUSR | Mode::S_IWUSR,
    )
    .with_context(|| format!("openat tmp {tmp}"))?;
    {
        // Reply consumers require exactly 0o600, even under a restrictive umask.
        fchmod(&fd, Mode::S_IRUSR | Mode::S_IWUSR).with_context(|| format!("fchmod tmp {tmp}"))?;
        let mut file = std::fs::File::from(fd);
        file.write_all(bytes)?;
    }
    if let Err(e) = renameat(dir, tmp.as_str(), dir, name) {
        // Best-effort cleanup so a failed rename does not leave the tmp around.
        let _ = unlinkat(dir, tmp.as_str(), UnlinkatFlags::NoRemoveDir);
        return Err(e).with_context(|| format!("renameat {tmp} -> {name}"));
    }
    Ok(())
}

pub(crate) fn session_id_leaf(source: Option<&str>) -> Result<std::borrow::Cow<'static, str>> {
    let Some(source) = source else {
        return Ok(std::borrow::Cow::Borrowed("session_id"));
    };
    let uuid = uuid::Uuid::parse_str(source).context("invalid session source UUID")?;
    let mut canonical = [0; 36];
    anyhow::ensure!(
        uuid.hyphenated().encode_lower(&mut canonical) == source,
        "session source UUID is not canonical"
    );
    Ok(std::borrow::Cow::Owned(format!("session_id.{source}")))
}

pub(crate) fn write_session_id_via_guard(
    instance_id: &str,
    session_id: &str,
    source: Option<&str>,
) -> Result<()> {
    let leaf = session_id_leaf(source)?;
    let dir = open_instance_dir(instance_id)?;
    write_atomic(dir.as_fd(), &leaf, session_id.as_bytes())
}

/// Delete the `session_id` sidecar with `unlinkat` against a verified
/// per-instance dirfd, so deletion runs under the same discipline as every
/// other hook write. Idempotent; `Err` only on guard validation or a hard
/// `unlinkat` failure, and callers treat that as best-effort.
pub(crate) fn unlink_session_id_via_guard(instance_id: &str) -> Result<()> {
    let Some(dir) = open_instance_dir_read_only(instance_id)? else {
        return Ok(());
    };
    match unlinkat(dir.as_fd(), "session_id", UnlinkatFlags::NoRemoveDir) {
        Ok(()) => Ok(()),
        Err(Errno::ENOENT) => Ok(()),
        Err(e) => Err(e).with_context(|| format!("unlinkat session_id in {instance_id}")),
    }
}

/// Create the per-instance hook directory under `dir_guard` discipline and
/// return its canonically resolved host path, for callers that hand the path
/// to an external resolver (a bind-mount source, a sidecar config writer)
/// rather than doing their own I/O.
///
/// Going through `open_instance_dir` rather than `create_dir_all` keeps both
/// guards: the default umask would create `0o755`, which `verify_dir_metadata`
/// then rejects, and an unguarded create races a pre-squat plus symlink swap
/// against the runtime's mount resolution.
///
/// The path is canonicalized before it leaves the process because a VM-backed
/// runtime resolves mount sources against real paths inside its VM (podman
/// machine shares `/private`, never `/tmp`) (#3240). A resolution error
/// propagates; callers skip the bind mount, warn, and boot with pane
/// detection.
pub(crate) fn ensure_instance_dir_path(instance_id: &str) -> Result<PathBuf> {
    let (kind, _fd) = resolve_or_create(instance_id)?;
    let lexical = kind.path()?.join(instance_id);
    std::fs::canonicalize(&lexical)
        .with_context(|| format!("canonicalize hook dir {}", lexical.display()))
}

/// The hook dir a host launch prepares its paths in: the host one, switched to now so the
/// launch's own switch removes nothing they point into. When the switch can't run, the
/// launch's can't either, so the existing dir stays and is used.
pub(crate) fn host_launch_instance_dir_path(instance_id: &str) -> Result<PathBuf> {
    switch_instance_dir(instance_id, HookBase::Host).or_else(|error| {
        tracing::warn!(target: "hooks.guard", "host hook dir switch failed: {error:#}");
        ensure_instance_dir_path(instance_id)
    })
}

// Cleanup.

/// Remove the per-instance subdir and its files without following symlinks,
/// re-fstatting each entry's fd before the unlink to close the swap window.
///
/// AoE never creates a subdirectory there, so one that appears is hostile or
/// stale: we refuse to descend and let the final `RemoveDir` fail with
/// `ENOTEMPTY`, which surfaces as a warn-skip.
pub(crate) fn remove_instance_dir(instance_id: &str) -> Result<()> {
    crate::session::validate_instance_id(instance_id)?;
    let sandbox_base = sandbox_hook_base_path().is_ok_and(|base| base.exists());
    let _lock = sandbox_base.then(|| lock_instance(instance_id)).flatten();
    let host = remove_child(HookBase::Host, instance_id);
    if sandbox_base {
        remove_child(HookBase::Sandbox, instance_id)?;
        let _ = with_base(HookBase::Sandbox, |base| {
            Ok(unlinkat(
                base,
                format!(".{instance_id}.lock").as_str(),
                UnlinkatFlags::NoRemoveDir,
            ))
        });
    }
    host
}

fn remove_child(kind: HookBase, instance_id: &str) -> Result<()> {
    with_base(kind, |base| {
        let dir_fd = match openat(
            base,
            instance_id,
            OFlag::O_DIRECTORY
                | OFlag::O_NOFOLLOW
                | OFlag::O_CLOEXEC
                | OFlag::O_RDONLY
                | OFlag::O_NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::ENOENT) | Err(Errno::ELOOP) => {
                // Gone, or a hostile symlink: unlink whatever is at the path
                // so a future open succeeds. Without `RemoveDir` that removes
                // the symlink itself, never its target.
                let _ = unlinkat(base, instance_id, UnlinkatFlags::NoRemoveDir);
                return Ok(());
            }
            Err(e) => return Err(e).with_context(|| format!("openat cleanup {instance_id}")),
        };
        let label = kind.label(instance_id);
        if let Err(e) = verify_dir_metadata(&dir_fd, &label) {
            // Wrong owner or mode: neither walk nor unlink, leave it to the user.
            tracing::warn!(target: "hooks.guard", "skip cleanup {}: {e:#}", label.display());
            return Ok(());
        }
        walk_and_unlink_entries(&dir_fd)?;
        // Final unlink of the per-instance subdir itself.
        if let Err(e) = unlinkat(base, instance_id, UnlinkatFlags::RemoveDir) {
            if e == Errno::ENOTEMPTY {
                tracing::warn!(target: "hooks.guard",
                    "skipped non-empty cleanup of {}: hostile or stale subdir present",
                    label.display());
                return Ok(());
            }
            return Err(e).with_context(|| format!("unlinkat RemoveDir {instance_id}"));
        }
        Ok(())
    })
}

fn walk_and_unlink_entries(dir_fd: &OwnedFd) -> Result<()> {
    // `Dir::from_fd` consumes the fd; keep a clone for the unlinkat below.
    let dup = dir_fd.try_clone().context("dup dir fd for readdir")?;
    let mut dir = nix::dir::Dir::from_fd(dup).context("Dir::from_fd")?;
    let names: Vec<std::ffi::CString> = dir
        .iter()
        .filter_map(|res| res.ok())
        .filter_map(|entry| {
            let name = entry.file_name();
            // Skip "." and ".." which `readdir` is allowed to surface.
            let bytes = name.to_bytes();
            if bytes == b"." || bytes == b".." {
                None
            } else {
                Some(name.to_owned())
            }
        })
        .collect();
    drop(dir); // drops the cloned fd

    for name in names {
        let name_str = match name.to_str() {
            Ok(s) => s,
            Err(_) => {
                tracing::warn!(target: "hooks.guard", "non-utf8 entry skipped");
                continue;
            }
        };
        // Classify without opening: a FIFO with no writer would block an open, and the dir is a
        // sandbox's writable bind. Only symlinks and regular files are removed.
        let kind = match fstatat(dir_fd, name_str, AtFlags::AT_SYMLINK_NOFOLLOW) {
            Ok(st) => st.st_mode & libc::S_IFMT,
            Err(Errno::ENOENT) => continue,
            Err(e) => {
                tracing::warn!(target: "hooks.guard", "fstatat entry {name_str}: {e}; skipping");
                continue;
            }
        };
        if kind == libc::S_IFLNK {
            if let Err(e) = unlinkat(dir_fd, name_str, UnlinkatFlags::NoRemoveDir) {
                tracing::warn!(target: "hooks.guard", "unlinkat symlink {name_str}: {e}");
            }
            continue;
        }
        if kind != libc::S_IFREG {
            tracing::warn!(target: "hooks.guard",
                "non-regular entry {name_str} (mode {kind:o}) inside {}; skipping. The \
                 instance dir will be left non-empty; remove manually if it matters.",
                hook_base_path().display());
            continue;
        }
        // Re-validate before removing, without blocking: an entry swapped in since must still
        // be a regular file.
        match openat(
            dir_fd,
            name_str,
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK,
            Mode::empty(),
        ) {
            Ok(child_fd) => {
                if !fstat(&child_fd).is_ok_and(|st| (st.st_mode & libc::S_IFMT) == libc::S_IFREG) {
                    tracing::warn!(target: "hooks.guard",
                        "entry {name_str} changed type during cleanup; skipping");
                    continue;
                }
                drop(child_fd);
                if let Err(e) = unlinkat(dir_fd, name_str, UnlinkatFlags::NoRemoveDir) {
                    tracing::warn!(target: "hooks.guard", "unlinkat {name_str}: {e}");
                }
            }
            Err(Errno::ENOENT) => {
                // Raced: another writer already removed it.
            }
            Err(e) => {
                tracing::warn!(target: "hooks.guard",
                    "openat entry {name_str}: {e}; skipping");
            }
        }
    }
    Ok(())
}

// Tests.

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use crate::hooks::test_support::{make_alien_owned, privdrop_test_enabled};
    use crate::hooks::test_support::{make_correct_base, BaseGuard};
    use serial_test::serial;
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    #[cfg(target_os = "macos")]
    use tempfile::TempDir;

    #[test]
    #[serial(hook_base)]
    fn init_accepts_fresh_or_correct_base_and_caches_the_fd() {
        for precreate in [false, true] {
            let (_g, base, _tmp) = BaseGuard::fresh();
            if precreate {
                make_correct_base(&base);
            }
            let raw1 = with_base(HookBase::Host, |fd| {
                let st = fstat(fd)?;
                assert_eq!(st.st_mode & 0o7777, 0o700, "precreate={precreate}");
                assert_eq!(st.st_uid, geteuid().as_raw());
                Ok(fd.as_raw_fd())
            })
            .expect("init must succeed");
            assert!(base.is_dir());
            let after_first = open_calls();
            let raw2 = with_base(HookBase::Host, |fd| Ok(fd.as_raw_fd())).unwrap();
            assert_eq!(raw1, raw2, "cached fd must be reused");
            assert_eq!(open_calls(), after_first, "no new open on second call");
        }
    }

    #[test]
    #[serial(hook_base)]
    fn instance_subdir_creates_with_0o700_when_absent() {
        let (_g, base, _tmp) = BaseGuard::fresh();
        make_correct_base(&base);
        let fd = open_instance_dir("test_inst_a").unwrap();
        let st = fstat(&fd).unwrap();
        assert_eq!(st.st_mode & 0o7777, 0o700);
    }

    #[test]
    #[serial(hook_base)]
    fn read_file_at_returns_none_when_absent() {
        let (_g, base, _tmp) = BaseGuard::fresh();
        make_correct_base(&base);
        let dir = open_instance_dir("ronone").unwrap();
        let got = read_file_at(dir.as_fd(), "missing", 64).unwrap();
        assert!(got.is_none());
    }

    #[test]
    #[serial(hook_base)]
    fn open_instance_dir_read_only_returns_none_for_absent() {
        let (_g, base, _tmp) = BaseGuard::fresh();
        make_correct_base(&base);
        // Note: read_only does NOT mkdir; absent means None.
        let got = open_instance_dir_read_only("missing_inst").unwrap();
        assert!(got.is_none());
    }

    #[test]
    #[serial(hook_base)]
    fn cleanup_handles_nonexistent_instance() {
        let (_g, base, _tmp) = BaseGuard::fresh();
        make_correct_base(&base);
        // Must not panic, must not error.
        remove_instance_dir("never_existed").unwrap();
    }

    #[test]
    #[serial(hook_base)]
    fn init_rejects_symlink_at_base() {
        let (_g, base, tmp) = BaseGuard::fresh();
        let target = tmp.path().join("decoy");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, &base).unwrap();
        let err = with_base(HookBase::Host, |_| Ok(())).unwrap_err();
        let s = format!("{err:#}");
        assert!(
            s.contains("symlink") || s.contains("ELOOP") || s.contains("Too many levels"),
            "expected symlink rejection, got: {s}"
        );
    }

    /// Any bit past `0o700`, and any of setuid, setgid or sticky, rejects the
    /// base rather than being used.
    #[test]
    #[serial(hook_base)]
    fn init_rejects_every_mode_but_0o700() {
        for ((mode, expected), kind) in [
            (0o755, "mode"),
            (0o770, "mode"),
            (0o2700, "setuid/setgid/sticky"),
            (0o4700, "setuid/setgid/sticky"),
            (0o1700, "setuid/setgid/sticky"),
        ]
        .into_iter()
        .flat_map(|case| [HookBase::Host, HookBase::Sandbox].map(|kind| (case, kind)))
        {
            let (_g, _base, _tmp) = BaseGuard::fresh();
            let base = kind.path().unwrap();
            std::fs::create_dir(&base).unwrap();
            let gid = nix::unistd::getegid().as_raw();
            if std::fs::metadata(&base).unwrap().gid() != gid {
                std::os::unix::fs::chown(&base, None, Some(gid)).unwrap();
            }
            std::fs::set_permissions(&base, std::fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                std::fs::metadata(&base).unwrap().permissions().mode() & 0o7777,
                mode
            );
            let err = with_base(kind, |_| Ok(())).unwrap_err();
            let rendered = format!("{err:#}");
            assert!(rendered.contains(expected), "{kind:?} {mode:o}: {rendered}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[serial(hook_base)]
    fn privdrop_init_rejects_alien_uid_base() {
        if !privdrop_test_enabled() {
            return;
        }
        let (_g, base, _tmp) = BaseGuard::fresh();
        make_correct_base(&base);
        let alien_uid = make_alien_owned(&base);

        let err = with_base(HookBase::Host, |_| Ok(())).unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains(&format!("owned by uid={alien_uid}, expected euid=0")),
            "expected alien base owner rejection, got: {message}"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    #[serial(hook_base)]
    fn privdrop_instance_subdir_rejects_alien_uid() {
        if !privdrop_test_enabled() {
            return;
        }
        let (_g, base, _tmp) = BaseGuard::ready();
        let instance = base.join("alien_instance");
        std::fs::create_dir(&instance).unwrap();
        std::fs::set_permissions(&instance, std::fs::Permissions::from_mode(0o700)).unwrap();
        let alien_uid = make_alien_owned(&instance);

        let err = open_instance_dir("alien_instance").unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.contains(&format!("owned by uid={alien_uid}, expected euid=0")),
            "expected alien instance owner rejection, got: {message}"
        );
    }

    #[test]
    #[serial(hook_base)]
    fn init_caches_error() {
        let (_g, base, tmp) = BaseGuard::fresh();
        let target = tmp.path().join("decoy2");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, &base).unwrap();
        let _ = with_base(HookBase::Host, |_| Ok(())).unwrap_err();
        let after_first = open_calls();
        let _ = with_base(HookBase::Host, |_| Ok(())).unwrap_err();
        assert_eq!(
            open_calls(),
            after_first,
            "second call must reuse cached error, not re-attempt open"
        );
    }

    /// A symlinked `<id>`, such as a container could plant in its sandbox base, is refused.
    #[test]
    #[serial(hook_base)]
    fn instance_subdir_rejects_symlink_leaf() {
        for kind in [HookBase::Host, HookBase::Sandbox] {
            let (_g, _base, tmp) = BaseGuard::fresh();
            let base = kind.path().unwrap();
            make_correct_base(&base);
            let decoy = tmp.path().join("decoy_for_inst");
            std::fs::create_dir_all(&decoy).unwrap();
            std::os::unix::fs::symlink(&decoy, base.join("test_inst_b")).unwrap();
            let err = match kind {
                HookBase::Host => open_instance_dir("test_inst_b").unwrap_err(),
                HookBase::Sandbox => switch_instance_dir("test_inst_b", kind).unwrap_err(),
            };
            let s = format!("{err:#}");
            assert!(
                s.contains("symlink") || s.contains("ELOOP") || s.contains("Too many levels"),
                "{kind:?}: expected ELOOP, got: {s}"
            );
        }
    }

    fn write_status(kind: HookBase, id: &str, status: &str) {
        let dir = create_child(kind, id).unwrap();
        write_atomic(dir.as_fd(), "status", status.as_bytes()).unwrap();
    }

    fn read_status(id: &str) -> Option<String> {
        let dir = open_instance_dir_read_only(id).unwrap()?;
        let bytes = read_file_at(dir.as_fd(), "status", 64).unwrap()?;
        Some(String::from_utf8(bytes).unwrap())
    }

    /// The host dir wins whenever it exists, so a container still bound to `/tmp` is never
    /// hidden by an empty sandbox dir; a row with only a sandbox dir is read and written there.
    #[test]
    #[serial(hook_base)]
    fn a_rows_hook_dir_is_the_host_one_unless_only_the_sandbox_one_exists() {
        // (host status, sandbox status, read, where a write lands)
        for (host, sandbox, want, lands) in [
            (Some("idle"), Some("running"), Some("idle"), HookBase::Host),
            (Some("idle"), None, Some("idle"), HookBase::Host),
            (None, Some("running"), Some("running"), HookBase::Sandbox),
            (None, None, None, HookBase::Host),
        ] {
            let (_g, _base, _tmp) = BaseGuard::ready();
            let id = "both_bases";
            if let Some(status) = host {
                write_status(HookBase::Host, id, status);
            }
            if let Some(status) = sandbox {
                write_status(HookBase::Sandbox, id, status);
            }
            assert_eq!(read_status(id).as_deref(), want, "{host:?} {sandbox:?}");
            write_session_id_via_guard(id, "sid", None).unwrap();
            for kind in [HookBase::Host, HookBase::Sandbox] {
                let written = kind.label(id).join("session_id").exists();
                assert_eq!(written, kind == lands, "{host:?} {sandbox:?} {kind:?}");
            }
            assert_eq!(
                kind_exists(HookBase::Host, id),
                host.is_some() || lands == HookBase::Host,
                "a write never creates a host dir beside a sandbox one: {host:?} {sandbox:?}"
            );
            remove_instance_dir(id).unwrap();
            assert!(!kind_exists(HookBase::Host, id) && !kind_exists(HookBase::Sandbox, id));
        }
    }

    fn kind_exists(kind: HookBase, id: &str) -> bool {
        kind.label(id).is_dir()
    }

    /// A switch makes the new dir before removing the old one, so a read in between still finds
    /// the old status, and a writer that arrives in between waits on the switch lock and then
    /// lands in the new dir.
    /// A waiter that wins a lock whose name was unlinked meanwhile takes the current one, so it
    /// and a later opener never both hold the row's lock.
    #[test]
    #[serial(hook_base)]
    fn a_lock_unlinked_while_held_is_not_shared() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let id = "relocked";
        let lock_path = sandbox_hook_base_path()
            .unwrap()
            .join(format!(".{id}.lock"));
        let holder = lock_instance(id).unwrap();
        let (contended, waiting) = std::sync::mpsc::channel();
        *LOCK_CONTENTION_FOR_TEST.lock().unwrap() = Some(contended);
        let (locked, held) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let waiter = std::thread::spawn(move || {
            BaseGuard::set_thread_base(base);
            let lock = lock_instance(id);
            locked.send(lock.is_some()).unwrap();
            let _ = released.recv();
            drop(lock);
        });
        waiting
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the waiter blocks on the held lock");
        *LOCK_CONTENTION_FOR_TEST.lock().unwrap() = None;
        std::fs::remove_file(&lock_path).unwrap();
        drop(holder);
        assert!(held
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap());
        let fresh = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        assert!(
            matches!(fresh.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
            "a later opener must wait on the waiter's lock"
        );
        release.send(()).unwrap();
        waiter.join().unwrap();
        remove_instance_dir(id).unwrap();
    }

    #[test]
    #[serial(hook_base)]
    fn a_writer_during_a_switch_lands_in_the_new_dir() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let id = "switching";
        write_status(HookBase::Host, id, "idle");
        let (contended, waiting) = std::sync::mpsc::channel();
        *LOCK_CONTENTION_FOR_TEST.lock().unwrap() = Some(contended);
        let (reads, read) = std::sync::mpsc::channel();
        let writer = std::sync::Arc::new(std::sync::Mutex::new(None));
        let handle = writer.clone();
        pause_switch_for_test(move || {
            reads.send(read_status(id)).unwrap();
            let thread_base = base.clone();
            let (done, finished) = std::sync::mpsc::channel();
            let joined = std::thread::spawn(move || {
                BaseGuard::set_thread_base(thread_base);
                let result = write_session_id_via_guard(id, "sid", None);
                // The switch may have stopped listening once it saw the writer wait.
                let _ = done.send(());
                result
            });
            // Either it is blocked on the lock, or (lock reverted) it already finished or
            // panicked, which the join below reports.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                if waiting.try_recv().is_ok()
                    || !matches!(
                        finished.try_recv(),
                        Err(std::sync::mpsc::TryRecvError::Empty)
                    )
                {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "the writer neither waited on the switch lock nor finished"
                );
                std::thread::yield_now();
            }
            *handle.lock().unwrap() = Some(joined);
        });
        switch_instance_dir(id, HookBase::Sandbox).unwrap();
        *LOCK_CONTENTION_FOR_TEST.lock().unwrap() = None;
        assert_eq!(read.recv().unwrap().as_deref(), Some("idle"));
        let joined = writer.lock().unwrap().take().unwrap();
        joined.join().unwrap().unwrap();
        assert!(
            !kind_exists(HookBase::Host, id),
            "the switch removed the host dir"
        );
        assert!(HookBase::Sandbox.label(id).join("session_id").exists());
        remove_instance_dir(id).unwrap();
    }

    /// A host launch of a row that was sandboxed reads the host dir at once, and the switch
    /// leaves no sandbox dir behind.
    #[test]
    #[serial(hook_base)]
    fn a_switch_to_the_host_dir_is_read_before_any_cleanup() {
        let (_g, _base, _tmp) = BaseGuard::ready();
        let id = "unsandboxed";
        write_status(HookBase::Sandbox, id, "idle");
        let path = switch_instance_dir(id, HookBase::Host).unwrap();
        assert!(!kind_exists(HookBase::Sandbox, id));
        assert_eq!(
            read_status(id),
            None,
            "the host dir is read, not the stale sandbox one"
        );
        std::fs::write(path.join("status"), "running").unwrap();
        assert_eq!(read_status(id).as_deref(), Some("running"));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o700);
        remove_instance_dir(id).unwrap();
    }

    /// A switch that can't take its lock changes nothing: it would otherwise remove a dir a
    /// creating open may be choosing.
    #[test]
    #[serial(hook_base)]
    fn a_switch_without_its_lock_changes_nothing() {
        let (_g, _base, _tmp) = BaseGuard::ready();
        let id = "unlockable";
        write_status(HookBase::Host, id, "idle");
        // A directory where the lock file goes: opening it read-write fails.
        with_base(HookBase::Sandbox, |_| Ok(())).unwrap();
        let lock = sandbox_hook_base_path()
            .unwrap()
            .join(format!(".{id}.lock"));
        std::fs::create_dir(&lock).unwrap();
        assert!(switch_instance_dir(id, HookBase::Sandbox).is_err());
        assert_eq!(read_status(id).as_deref(), Some("idle"));
        assert!(
            !kind_exists(HookBase::Sandbox, id),
            "no sandbox dir was made"
        );
        std::fs::remove_dir(&lock).unwrap();
        remove_instance_dir(id).unwrap();
    }

    /// A sandbox can leave a FIFO in its writable hook dir. Cleanup never opens it, so a switch
    /// away completes; the dir it leaves behind reads the FIFO as absent and takes a new write
    /// when the row is sandboxed again.
    #[test]
    #[serial(hook_base)]
    fn a_switch_skips_a_fifo_in_the_old_dir() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let id = "fifo";
        create_child(HookBase::Sandbox, id).unwrap();
        let fifo = HookBase::Sandbox.label(id).join("status");
        nix::unistd::mkfifo(&fifo, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        let inode = std::os::unix::fs::MetadataExt::ino(
            &std::fs::metadata(HookBase::Sandbox.label(id)).unwrap(),
        );
        let (done, finished) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            BaseGuard::set_thread_base(base);
            switch_instance_dir(id, HookBase::Host).unwrap();
            let after_host = read_status(id);
            switch_instance_dir(id, HookBase::Sandbox).unwrap();
            let reused = std::os::unix::fs::MetadataExt::ino(
                &std::fs::metadata(HookBase::Sandbox.label(id)).unwrap(),
            );
            let polluted = read_status(id);
            write_status(HookBase::Sandbox, id, "idle");
            let _ = done.send((after_host, reused, polluted, read_status(id)));
        });
        let Ok((after_host, reused, polluted, written)) =
            finished.recv_timeout(std::time::Duration::from_secs(10))
        else {
            // A writer releases an open blocked on the FIFO, so the thread can end.
            let _ = open(&fifo, OFlag::O_WRONLY | OFlag::O_NONBLOCK, Mode::empty());
            panic!("the switch or a read blocked on a FIFO");
        };
        worker.join().unwrap();
        assert_eq!(after_host, None, "the host dir is read");
        assert_eq!(reused, inode, "the sandbox dir left by the skip is reused");
        assert_eq!(polluted, None, "a FIFO reads as absent");
        assert_eq!(
            written.as_deref(),
            Some("idle"),
            "a write replaces the FIFO"
        );
        remove_instance_dir(id).unwrap();
    }

    #[test]
    #[serial(hook_base)]
    fn read_file_at_rejects_symlink_leaf() {
        let (_g, base, tmp) = BaseGuard::fresh();
        make_correct_base(&base);
        let dir = open_instance_dir("sym_read").unwrap();
        // Plant a symlink leaf using std (path-based; we own the dir 0o700).
        let canary = tmp.path().join("canary_text");
        std::fs::write(&canary, b"sensitive").unwrap();
        std::os::unix::fs::symlink(&canary, base.join("sym_read").join("status")).unwrap();
        // Reader must NOT follow.
        let res = read_file_at(dir.as_fd(), "status", 64).unwrap();
        assert!(
            res.is_none(),
            "read_file_at must refuse symlink leaves, got {res:?}"
        );
        // Canary remains intact.
        let mut s = String::new();
        std::fs::File::open(&canary)
            .unwrap()
            .read_to_string(&mut s)
            .unwrap();
        assert_eq!(s, "sensitive");
    }

    #[test]
    #[serial(hook_base)]
    fn write_short_rejects_symlink_leaf() {
        let (_g, base, tmp) = BaseGuard::fresh();
        make_correct_base(&base);
        let dir = open_instance_dir("sym_write").unwrap();
        let canary = tmp.path().join("canary_text2");
        std::fs::write(&canary, b"untouched").unwrap();
        std::os::unix::fs::symlink(&canary, base.join("sym_write").join("status")).unwrap();
        let err = write_short(dir.as_fd(), "status", b"running").unwrap_err();
        let s = format!("{err:#}");
        assert!(
            s.contains("ELOOP") || s.contains("Too many levels") || s.contains("symlink"),
            "expected ELOOP, got: {s}"
        );
        let mut got = String::new();
        std::fs::File::open(&canary)
            .unwrap()
            .read_to_string(&mut got)
            .unwrap();
        assert_eq!(got, "untouched");
    }

    #[test]
    #[serial(hook_base)]
    fn cleanup_does_not_follow_leaf_symlinks() {
        let (_g, base, tmp) = BaseGuard::ready();
        let _ = open_instance_dir("cleanup_sym").unwrap();
        let canary = tmp.path().join("cleanup_canary");
        std::fs::write(&canary, b"keep").unwrap();
        let decoy_dir = tmp.path().join("decoy_subdir");
        std::fs::create_dir_all(&decoy_dir).unwrap();
        std::fs::write(decoy_dir.join("witness"), b"keep").unwrap();
        let instance = base.join("cleanup_sym");
        std::os::unix::fs::symlink(&canary, instance.join("file_escape")).unwrap();
        std::os::unix::fs::symlink(&decoy_dir, instance.join("dir_escape")).unwrap();

        remove_instance_dir("cleanup_sym").unwrap();
        assert!(!instance.exists(), "subdir must be removed");
        assert_eq!(std::fs::read_to_string(&canary).unwrap(), "keep");
        assert_eq!(
            std::fs::read_to_string(decoy_dir.join("witness")).unwrap(),
            "keep",
            "decoy directory contents must be intact"
        );
    }

    #[test]
    #[serial(hook_base)]
    fn hook_base_path_bakes_euid_suffix() {
        clear_base_override_for_test();
        let path = hook_base_path();
        let want_suffix = format!("aoe-hooks-{}", geteuid().as_raw());
        let got = path
            .file_name()
            .and_then(|s| s.to_str())
            .expect("hook base path must end with a UTF-8 file name");
        assert_eq!(
            got,
            want_suffix,
            "production hook base must end with /tmp/aoe-hooks-<euid>; got {}",
            path.display()
        );
        assert_eq!(
            path.parent().and_then(|p| p.to_str()),
            Some("/tmp"),
            "production hook base must live under /tmp; got {}",
            path.display()
        );
    }

    #[test]
    #[serial(hook_base)]
    fn atomic_reply_stays_owner_readable_under_restrictive_umask() {
        const CHILD_BASE: &str = "AOE_HOOK_ATOMIC_UMASK_CHILD_BASE";
        const NAME: &str = "integration-omp.agent-of-empires.reply.json";
        const RESPONSE: &[u8] = br#"{"response":{"kind":"approval","choice":"allow"}}"#;
        if let Some(base) = std::env::var_os(CHILD_BASE) {
            let _guard = BaseGuard::with_base(base.into());
            let dir = open_instance_dir("atomic_reply").unwrap();
            // Only this isolated, single-test child changes its process umask.
            nix::sys::stat::umask(Mode::from_bits_truncate(0o777));
            write_atomic(dir.as_fd(), NAME, RESPONSE).unwrap();
            return;
        }

        let (_guard, base, _tmp) = BaseGuard::ready();
        let dir = open_instance_dir("atomic_reply").unwrap();
        let module = module_path!().split_once("::").unwrap().1;
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                &format!("{module}::atomic_reply_stays_owner_readable_under_restrictive_umask"),
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD_BASE, &base);
        let output = crate::process::run_with_timeout_process_group(
            &mut command,
            std::time::Duration::from_secs(20),
        )
        .unwrap()
        .expect("restrictive-umask child timed out");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let (metadata, bytes) = read_file_with_metadata_at(dir.as_fd(), NAME, 4096)
            .unwrap()
            .expect("child must publish the completed reply");
        assert_eq!(metadata.permissions().mode() & 0o7777, 0o600);
        assert_eq!(bytes, RESPONSE);
        assert_eq!(
            std::fs::read_dir(base.join("atomic_reply"))
                .unwrap()
                .count(),
            1,
            "atomic write must not leave a temporary file"
        );
    }

    #[test]
    #[serial(hook_base)]
    fn concurrent_writers_no_corruption() {
        let (_g, base, _tmp) = BaseGuard::ready();
        let dir = open_instance_dir("conc").unwrap();
        let dir_fd = dir.as_fd();
        // Distinct payloads per thread, so a regression that truncates in
        // place instead of renaming leaves bytes matching no single writer.
        let payloads: Vec<[u8; 36]> = (0..8u8)
            .map(|tid| {
                let s = format!("aaaaaaaa-bbbb-cccc-dddd-{tid:012x}");
                let mut buf = [0u8; 36];
                buf.copy_from_slice(s.as_bytes());
                buf
            })
            .collect();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        std::thread::scope(|s| {
            for (tid, payload) in payloads.iter().enumerate() {
                let b = barrier.clone();
                let p = *payload;
                s.spawn(move || {
                    b.wait();
                    for _ in 0..200 {
                        write_atomic(dir_fd, "session_id", &p).unwrap();
                    }
                    let _ = tid;
                });
            }
        });
        let got = read_file_at(dir_fd, "session_id", 64).unwrap().unwrap();
        assert_eq!(got.len(), 36, "torn write: got {} bytes", got.len());
        assert!(
            payloads.iter().any(|p| p.as_slice() == got.as_slice()),
            "final state must equal exactly one writer's payload, got: {got:?}"
        );
        let leaked: Vec<_> = std::fs::read_dir(base.join("conc"))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".session_id.tmp.")
            })
            .collect();
        assert!(leaked.is_empty(), "tmp files leaked: {leaked:?}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[serial(hook_base)]
    fn macos_tmp_prefix_symlink_works() {
        // A prefix symlink (macOS /tmp -> /private/tmp) must not block init:
        // O_NOFOLLOW checks only the final component.
        let tmp = TempDir::new().unwrap();
        let real_parent = tmp.path().join("real-parent");
        std::fs::create_dir(&real_parent).unwrap();
        let symlink_parent = tmp.path().join("via-symlink");
        std::os::unix::fs::symlink(&real_parent, &symlink_parent).unwrap();
        let base = symlink_parent.join("aoe-hooks");
        override_base_for_test(base.clone());
        reset_for_test();
        with_base(HookBase::Host, |_| Ok(())).expect("prefix symlink must not block init");
        clear_base_override_for_test();
        reset_for_test();
    }
}
