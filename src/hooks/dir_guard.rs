//! Hardened access to the AoE hook status directory, the single Rust entry
//! point for every read, write, and cleanup under `/tmp/aoe-hooks-<euid>`
//! (#1844).
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
use nix::fcntl::{open, openat, renameat, OFlag};
use nix::libc;
use nix::sys::stat::{fstat, mkdirat, Mode};
use nix::unistd::{geteuid, mkdir, unlinkat, UnlinkatFlags};

// Path resolution.

#[cfg(test)]
thread_local! {
    /// Test-only base path override. A test using it must also call
    /// `reset_for_test` and serialize via `serial_test::serial(hook_base)`.
    static HOOK_BASE_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
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

#[cfg(test)]
pub(crate) fn override_base_for_test(path: PathBuf) {
    HOOK_BASE_OVERRIDE.with(|c| *c.borrow_mut() = Some(path));
}

#[cfg(test)]
pub(crate) fn clear_base_override_for_test() {
    HOOK_BASE_OVERRIDE.with(|c| *c.borrow_mut() = None);
}

// Singleton cell.

type CachedBase = std::result::Result<OwnedFd, Arc<anyhow::Error>>;

// `static` so the owned fd lives for the program lifetime and its `close` on
// drop never fires; `with_hook_base` only lends it for a closure call.
#[cfg(not(test))]
static HOOK_BASE: OnceLock<CachedBase> = OnceLock::new();

#[cfg(test)]
thread_local! {
    /// Per-thread shadow of `HOOK_BASE`, since a test cannot reset a
    /// process-wide `OnceLock`. Production never touches it.
    static HOOK_BASE_TEST_CELL: std::cell::RefCell<Option<CachedBase>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    HOOK_BASE_TEST_CELL.with(|c| *c.borrow_mut() = None);
    OPEN_CALLS.store(0, std::sync::atomic::Ordering::Relaxed);
}

// Syscall counter the caching tests read. Production never observes it.
#[cfg(test)]
static OPEN_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) fn open_calls() -> usize {
    OPEN_CALLS.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
fn cached_get_or_init_apply<I, A, T>(init: I, apply: A) -> Result<T>
where
    I: FnOnce() -> std::result::Result<OwnedFd, Arc<anyhow::Error>>,
    A: FnOnce(&std::result::Result<OwnedFd, Arc<anyhow::Error>>) -> Result<T>,
{
    // The `borrow_mut` is held for the whole call, so a closure that
    // re-enters `with_hook_base` would panic with `BorrowMutError`.
    HOOK_BASE_TEST_CELL.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(init());
        }
        apply(slot.as_ref().unwrap())
    })
}

#[cfg(not(test))]
fn cached_get_or_init_apply<I, A, T>(init: I, apply: A) -> Result<T>
where
    I: FnOnce() -> std::result::Result<OwnedFd, Arc<anyhow::Error>>,
    A: FnOnce(&std::result::Result<OwnedFd, Arc<anyhow::Error>>) -> Result<T>,
{
    apply(HOOK_BASE.get_or_init(init))
}

/// Open and verify the per-user hook base directory once, then run `f` with
/// a borrowed fd to it; later callers reuse the cached fd or cached error.
/// The closure shape is what keeps the fd from escaping its borrow.
pub(crate) fn with_hook_base<F, T>(f: F) -> Result<T>
where
    F: FnOnce(BorrowedFd<'_>) -> Result<T>,
{
    cached_get_or_init_apply(
        || match open_and_verify_base() {
            Ok(fd) => Ok(fd),
            Err(e) => {
                tracing::error!(
                    target: "hooks.guard",
                    "hook base init failed: {e:#}. AoE will fall back to pane-detection. \
                     Recover: rm -rf {}",
                    hook_base_path().display()
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

fn open_and_verify_base() -> Result<OwnedFd> {
    #[cfg(test)]
    OPEN_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let path = hook_base_path();

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
        OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY,
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

/// `mkdirat(base, id, 0o700)` (EEXIST-tolerant) plus `openat(O_NOFOLLOW)` plus
/// `fstat`-on-fd uid/mode check. Returns an owned fd to the per-instance
/// directory.
pub(crate) fn open_instance_dir(instance_id: &str) -> Result<OwnedFd> {
    crate::session::validate_instance_id(instance_id)?;
    with_hook_base(|base| {
        match mkdirat(base, instance_id, Mode::S_IRWXU) {
            Ok(()) | Err(Errno::EEXIST) => {}
            Err(e) => {
                return Err(e).with_context(|| format!("mkdirat {instance_id}"));
            }
        }
        let fd: OwnedFd = openat(
            base,
            instance_id,
            OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY,
            Mode::empty(),
        )
        .with_context(|| format!("openat instance subdir {instance_id} (symlink or non-dir)"))?;
        let label = hook_base_path().join(instance_id);
        verify_dir_metadata(&fd, &label)?;
        Ok(fd)
    })
}

/// Read-only variant: never creates the dir. Returns `Ok(None)` on `ENOENT` /
/// `ELOOP` (legitimate transient absence or hostile symlink swap, both
/// indistinguishable from "no hook fired yet" on the polling path).
pub(crate) fn open_instance_dir_read_only(instance_id: &str) -> Result<Option<OwnedFd>> {
    crate::session::validate_instance_id(instance_id)?;
    with_hook_base(|base| {
        match openat(
            base,
            instance_id,
            OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY,
            Mode::empty(),
        ) {
            Ok(fd) => {
                let label = hook_base_path().join(instance_id);
                verify_dir_metadata(&fd, &label)?;
                Ok(Some(fd))
            }
            Err(Errno::ENOENT) | Err(Errno::ELOOP) => Ok(None),
            Err(e) => Err(e).with_context(|| format!("openat instance subdir {instance_id}")),
        }
    })
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
    use std::io::Read;
    validate_hook_leaf(name)?;
    let fd = match openat(
        dir,
        name,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::ENOENT) | Err(Errno::ELOOP) => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("openat read {name}")),
    };
    let mut file = std::fs::File::from(fd);
    if !file.metadata()?.is_file() {
        return Ok(None);
    }
    let mut buf = Vec::with_capacity(max_bytes.min(4096));
    let limit = u64::try_from(max_bytes).unwrap_or(u64::MAX);
    file.by_ref().take(limit).read_to_end(&mut buf)?;
    Ok(Some(buf))
}

/// `fstatat(AT_SYMLINK_NOFOLLOW)` view for mtime gating. Returns `Ok(None)` on
/// missing or symlinked entries.
pub(crate) fn metadata_at(dir: BorrowedFd<'_>, name: &str) -> Result<Option<Metadata>> {
    validate_hook_leaf(name)?;
    let fd = match openat(
        dir,
        name,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
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

/// Atomic write via an `O_CREAT|O_EXCL` tmpfile and `renameat`, used for the
/// `session_id` sidecar.
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

/// For `aoe __extract-session-id`: validate the instance id, open its dir
/// under `dir_guard` discipline, and atomically write the sidecar.
pub(crate) fn write_session_id_via_guard(instance_id: &str, session_id: &str) -> Result<()> {
    let dir = open_instance_dir(instance_id)?;
    write_atomic(dir.as_fd(), "session_id", session_id.as_bytes())
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
    let _fd = open_instance_dir(instance_id)?;
    let lexical = hook_base_path().join(instance_id);
    std::fs::canonicalize(&lexical)
        .with_context(|| format!("canonicalize hook dir {}", lexical.display()))
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
    with_hook_base(|base| {
        let dir_fd = match openat(
            base,
            instance_id,
            OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC | OFlag::O_RDONLY,
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
        let label = hook_base_path().join(instance_id);
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
        // Re-validate before removing: O_NOFOLLOW rejects a symlink leaf with
        // ELOOP, and anything but a regular file is hostile or stale.
        match openat(
            dir_fd,
            name_str,
            OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
            Mode::empty(),
        ) {
            Ok(child_fd) => {
                let st = match fstat(&child_fd) {
                    Ok(st) => st,
                    Err(e) => {
                        tracing::warn!(target: "hooks.guard",
                            "fstat entry {name_str}: {e}; skipping");
                        continue;
                    }
                };
                if (st.st_mode & libc::S_IFMT) != libc::S_IFREG {
                    tracing::warn!(target: "hooks.guard",
                        "non-regular entry {name_str} (mode {:o}) inside {}; \
                         skipping. The instance dir will be left non-empty; \
                         remove manually if it matters.",
                        st.st_mode,
                        hook_base_path().display());
                    continue;
                }
                // Regular file under a uid-checked parent: safe to unlink.
                drop(child_fd);
                if let Err(e) = unlinkat(dir_fd, name_str, UnlinkatFlags::NoRemoveDir) {
                    tracing::warn!(target: "hooks.guard",
                        "unlinkat {name_str}: {e}");
                }
            }
            Err(Errno::ELOOP) => {
                // Symlink at leaf: unlink it without following.
                if let Err(e) = unlinkat(dir_fd, name_str, UnlinkatFlags::NoRemoveDir) {
                    tracing::warn!(target: "hooks.guard",
                        "unlinkat symlink {name_str}: {e}");
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
    use std::os::unix::fs::PermissionsExt;
    #[cfg(target_os = "macos")]
    use tempfile::TempDir;

    #[test]
    #[serial(hook_base)]
    fn init_succeeds_on_fresh_dir() {
        let (_g, base, _tmp) = BaseGuard::fresh();
        with_hook_base(|fd| {
            assert!(base.is_dir());
            let st = fstat(fd)?;
            let mode = st.st_mode & 0o7777;
            assert_eq!(mode, 0o700, "got mode {mode:o}");
            assert_eq!(st.st_uid, geteuid().as_raw());
            Ok(())
        })
        .expect("init must succeed on fresh path");
    }

    #[test]
    #[serial(hook_base)]
    fn init_succeeds_when_base_already_correct() {
        let (_g, base, _tmp) = BaseGuard::fresh();
        make_correct_base(&base);
        with_hook_base(|_| Ok(())).expect("init must succeed when base already 0700 and ours");
    }

    #[test]
    #[serial(hook_base)]
    fn init_rejects_symlink_at_base() {
        let (_g, base, tmp) = BaseGuard::fresh();
        let target = tmp.path().join("decoy");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, &base).unwrap();
        let err = with_hook_base(|_| Ok(())).unwrap_err();
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
        for (mode, expected) in [
            (0o755, "mode"),
            (0o770, "mode"),
            (0o2700, "setuid/setgid/sticky"),
            (0o4700, "setuid/setgid/sticky"),
            (0o1700, "setuid/setgid/sticky"),
        ] {
            let (_g, base, _tmp) = BaseGuard::fresh();
            std::fs::create_dir(&base).unwrap();
            std::fs::set_permissions(&base, std::fs::Permissions::from_mode(mode)).unwrap();
            let err = with_hook_base(|_| Ok(())).unwrap_err();
            let rendered = format!("{err:#}");
            assert!(rendered.contains(expected), "{mode:o}: {rendered}");
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

        let err = with_hook_base(|_| Ok(())).unwrap_err();
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
        let _ = with_hook_base(|_| Ok(())).unwrap_err();
        let after_first = open_calls();
        let _ = with_hook_base(|_| Ok(())).unwrap_err();
        assert_eq!(
            open_calls(),
            after_first,
            "second call must reuse cached error, not re-attempt open"
        );
    }

    #[test]
    #[serial(hook_base)]
    fn init_caches_success() {
        let (_g, base, _tmp) = BaseGuard::fresh();
        make_correct_base(&base);
        let raw1 = with_hook_base(|fd| Ok(fd.as_raw_fd())).unwrap();
        let after_first = open_calls();
        let raw2 = with_hook_base(|fd| Ok(fd.as_raw_fd())).unwrap();
        assert_eq!(raw1, raw2, "cached fd must be byte-equal across calls");
        assert_eq!(
            open_calls(),
            after_first,
            "no new open syscall on second call"
        );
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
    fn instance_subdir_rejects_symlink_leaf() {
        let (_g, base, tmp) = BaseGuard::fresh();
        make_correct_base(&base);
        let decoy = tmp.path().join("decoy_for_inst");
        std::fs::create_dir_all(&decoy).unwrap();
        std::os::unix::fs::symlink(&decoy, base.join("test_inst_b")).unwrap();
        let err = open_instance_dir("test_inst_b").unwrap_err();
        let s = format!("{err:#}");
        assert!(
            s.contains("symlink") || s.contains("ELOOP") || s.contains("Too many levels"),
            "expected ELOOP, got: {s}"
        );
    }

    #[test]
    #[serial(hook_base)]
    fn write_short_then_read_file_at_roundtrip() {
        let (_g, base, _tmp) = BaseGuard::fresh();
        make_correct_base(&base);
        let dir = open_instance_dir("rt").unwrap();
        write_short(dir.as_fd(), "status", b"running").unwrap();
        let bytes = read_file_at(dir.as_fd(), "status", 64).unwrap().unwrap();
        assert_eq!(bytes, b"running");
    }

    #[test]
    #[serial(hook_base)]
    fn write_atomic_renames_atomically() {
        let (_g, base, _tmp) = BaseGuard::fresh();
        make_correct_base(&base);
        let dir = open_instance_dir("atomic_rt").unwrap();
        let uuid = b"aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        write_atomic(dir.as_fd(), "session_id", uuid).unwrap();
        let bytes = read_file_at(dir.as_fd(), "session_id", 64)
            .unwrap()
            .unwrap();
        assert_eq!(bytes, uuid);
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
    fn cleanup_does_not_follow_leaf_symlink() {
        let (_g, base, tmp) = BaseGuard::fresh();
        make_correct_base(&base);
        let _ = open_instance_dir("cleanup_sym").unwrap();
        let canary = tmp.path().join("cleanup_canary");
        std::fs::write(&canary, b"keep").unwrap();
        // Plant a symlink leaf inside the per-instance dir.
        std::os::unix::fs::symlink(&canary, base.join("cleanup_sym").join("escape")).unwrap();
        remove_instance_dir("cleanup_sym").unwrap();
        // The link is gone, the canary lives.
        assert!(!base.join("cleanup_sym").exists(), "subdir must be removed");
        let mut got = String::new();
        std::fs::File::open(&canary)
            .unwrap()
            .read_to_string(&mut got)
            .unwrap();
        assert_eq!(got, "keep");
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
    fn cleanup_rejects_subdir_symlink_at_leaf() {
        let (_g, base, tmp) = BaseGuard::ready();
        let _ = open_instance_dir("subdir_sym").unwrap();
        let target = tmp.path().join("decoy_subdir");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("witness"), b"keep").unwrap();
        std::os::unix::fs::symlink(&target, base.join("subdir_sym").join("escape")).unwrap();
        remove_instance_dir("subdir_sym").unwrap();
        assert!(target.is_dir(), "decoy directory must survive cleanup");
        assert_eq!(
            std::fs::read_to_string(target.join("witness")).unwrap(),
            "keep",
            "decoy contents must be intact"
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
        with_hook_base(|_| Ok(())).expect("prefix symlink must not block init");
        clear_base_override_for_test();
        reset_for_test();
    }
}
