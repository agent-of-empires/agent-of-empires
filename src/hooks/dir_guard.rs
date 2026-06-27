//! Hardened access to the AoE hook status directory.
//!
//! Issue #1844: defend against TOCTOU and symlink attacks on the world-known
//! `/tmp/aoe-hooks` path. This module is the single Rust entry point for every
//! reader, writer, and cleanup that touches a hook-status file on the host.
//!
//! ## Threat model
//!
//! - Defends against another local UID on a multi-tenant POSIX host pre-creating
//!   or symlinking the base path, racing `lstat` vs `open`, or planting hostile
//!   leaves under the per-instance directory.
//! - Does NOT defend against a co-resident attacker with the same UID (they can
//!   read/write our state directly anyway).
//! - Sandbox container is per-instance and single-tenant; the multi-tenant
//!   threat collapses there. Container-side guards live in the shell snippets
//!   in `super::mod`, not here.
//!
//! ## Algorithm
//!
//! 1. Resolve the per-user base path: `/tmp/aoe-hooks-<euid>`. The euid
//!    suffix prevents a co-tenant collision: pure `/tmp/aoe-hooks` would
//!    deny user B once user A has created it.
//! 2. `mkdir(0o700)` tolerating `EEXIST`.
//! 3. `open(O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC | O_RDONLY)`. `O_NOFOLLOW`
//!    only checks the FINAL component, so `/tmp -> /private/tmp` on macOS is
//!    fine.
//! 4. `fstat` ON THE FD. After this, the inode is pinned: any later path swap
//!    only affects the path, not our fd. Reject if not a directory, wrong uid,
//!    or any group/world bit set.
//! 5. Cache the verified `OwnedFd` in a `static OnceLock` so subsequent reads
//!    and writes ride the same fd. On error we cache an `Arc<anyhow::Error>`
//!    so retries do not silently mask the bad state.
//!
//! Per-instance subdirs and per-file I/O ride the same `*at` discipline,
//! always anchored on a fd we have already verified.
//!
//! ## Squatting DoS (documented limitation)
//!
//! An attacker who pre-creates `/tmp/aoe-hooks-<our-euid>` owned by themselves
//! cannot be cleared by us (sticky bit on `/tmp` plus alien ownership). Effect:
//! `with_hook_base` returns `Err`; AoE keeps running with hooks disabled and
//! falls back to pane-detection. Recovery requires the squatter to log out,
//! reboot, or root cooperation. Bounded DoS only; never a privilege escalation.
//!
//! ## `/tmp` reaper (documented limitation)
//!
//! systemd-tmpfiles or macOS `periodic.daily` may delete the base directory
//! while we hold the cached fd. Subsequent `*at` calls keep working against
//! the orphan inode (POSIX guarantee), but the hook shell snippets do path-
//! based `mkdir -p` and create a fresh inode at the same path. Reads via
//! the cached fd then see the orphan, writes via the shell hooks land on
//! the new inode, and status detection silently breaks until the next AoE
//! restart. Acceptable: pane-detection is the documented fallback.
//!
//! ## POSIX ACL widening (documented limitation)
//!
//! `verify_dir_metadata` inspects classic POSIX mode bits only (`mode &
//! 0o077`, plus `mode & 0o7000` for setuid/setgid/sticky). It does NOT
//! inspect POSIX ACL entries: a `setfacl -m u:other:rwx <base>` can grant
//! a co-tenant write access without flipping any bit in `st_mode`. The
//! mismatch is not exploitable in this threat model. An alien uid cannot
//! `setfacl` on a `0o700` directory we own (ACL writes require ownership
//! or write permission), and we never widen our own ACL. The shell pattern
//! `d*------|d*------.|d*------+|d*------@` tolerates the trailing `+`
//! glyph emitted by `ls -l` when a legitimate operator-applied ACL is
//! present; the mode positions still must read `------`, so an ACL that
//! widens past `r--` triggers a different glyph and the snippet rejects.

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
    /// Test-only override for the per-user base path. Each test injects its
    /// own tempdir to avoid colliding on the real `/tmp/aoe-hooks-<euid>` and
    /// to dodge the process-wide `OnceLock` pinning the first path it sees.
    /// Tests using this MUST also call `reset_for_test` and serialize via
    /// `serial_test::serial(hook_base)`.
    static HOOK_BASE_OVERRIDE: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Per-user host base path: `/tmp/aoe-hooks-<euid>`. Suffix from `geteuid()`,
/// not `getuid()`: the agent runs with the effective uid and writes through
/// `id -u` (which is also euid), so both ends agree.
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

// MUST be `static` so the cached `OwnedFd` outlives every call site:
// `with_hook_base` re-borrows it as a `BorrowedFd<'_>` scoped to the
// closure invocation, but the underlying owned fd lives in static
// storage for the program lifetime so its `close` on drop never fires.
#[cfg(not(test))]
static HOOK_BASE: OnceLock<CachedBase> = OnceLock::new();

#[cfg(test)]
thread_local! {
    /// Per-thread shadow of `HOOK_BASE`. Tests cannot reset a process-wide
    /// `OnceLock`, so we keep parallel storage gated by `cfg(test)` and route
    /// the public API through a runtime branch. Production paths NEVER touch
    /// this cell.
    static HOOK_BASE_TEST_CELL: std::cell::RefCell<Option<CachedBase>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    HOOK_BASE_TEST_CELL.with(|c| *c.borrow_mut() = None);
    OPEN_CALLS.store(0, std::sync::atomic::Ordering::Relaxed);
}

// `open`/`mkdir`/`fstat` syscall counter for test #6 (`init_caches_error`)
// and test #7 (`init_caches_success`). Production reads never observe it.
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
    // The `RefCell::borrow_mut` is held for the whole call, so a closure
    // that recursively re-enters `with_hook_base` from inside `apply`
    // would `BorrowMutError`-panic. No production caller does this and
    // every existing test invokes `with_hook_base` linearly; the comment
    // is a guard for future refactors only.
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

/// Lazily open-and-verify the per-user hook base directory and run `f` with
/// a borrowed fd to it. First caller does the real work; subsequent callers
/// reuse the cached fd (or, on the failure path, the cached `Arc<Error>`).
///
/// The borrow lifetime is bound to the closure call: the borrow checker
/// rejects any attempt to escape the fd outside `f`. This is the soundness
/// reason for the closure shape over a direct `BorrowedFd<'static>` return.
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

    // 2. open(O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC | O_RDONLY).
    //    O_NOFOLLOW only checks the FINAL component; intermediate symlinks
    //    in the prefix (macOS /tmp -> /private/tmp) are followed normally.
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

    // 3. fstat ON THE FD. After this, the inode is pinned for the lifetime of
    //    the fd. nix 0.31 wants `AsFd`; pass `&fd`.
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

/// Reject leaf names that could escape the verified parent dirfd via
/// `openat`. Absolute leaves (`/...`) cause the kernel to ignore the
/// dirfd entirely; relative components like `subdir/file` would
/// traverse into nested entries; `..` walks up; `.` is the parent
/// itself. NUL is rejected because `openat` would fail with `EINVAL`
/// on it anyway, but checking ahead surfaces the error cleanly.
///
/// Leading `.` is allowed because `write_atomic` constructs tmpfile
/// names of the form `.{name}.tmp.{pid}.{counter}`. The agent-side
/// shell snippet uses the same pattern. Validating here is
/// defense-in-depth for future callers; today every call site passes
/// a hardcoded literal (`status`, `session_id`, `attention.json`).
fn validate_hook_leaf(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." {
        bail!("invalid hook leaf name: {name:?}");
    }
    if name.as_bytes().iter().any(|&b| b == b'/' || b == 0) {
        bail!("hook leaf name contains separator or NUL: {name:?}");
    }
    Ok(())
}

/// Open a file inside an already-verified per-instance dir for reading.
/// `O_NOFOLLOW` forbids the leaf being a symlink. `ENOENT` / `ELOOP` map to
/// `Ok(None)`. Non-regular leaves (directory, FIFO, device, socket) also
/// map to `Ok(None)`: only regular files are valid hook sidecars and the
/// fstat-on-fd check is symmetric with the `S_IFREG` gate in
/// `remove_instance_dir`.
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

/// Single-shot truncating write. Suitable for `<dir>/status` (≤8 bytes,
/// monotone, last-writer-wins acceptable). Reader is stale-tolerant.
///
/// Production `status` writes happen in the agent-side shell snippet
/// (`hook_command_with_base`); this helper exists for in-process test
/// fixtures that need to plant status content via the same `*at`-anchored
/// discipline.
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

/// Atomic write via `O_CREAT|O_EXCL` tmpfile + `renameat`. Used for
/// `session_id` sidecar writes (see [`write_session_id_via_guard`]).
/// `attention.json` is written by the host shell snippet
/// `cx-script attention-urgent`, not through this Rust helper, but the
/// same atomicity-not-durability contract applies on its `mv` rename.
///
/// Atomicity, not durability: there is no `fsync`/`sync_data` before the
/// rename. After a power loss the file may revert to the previous version
/// or vanish. Acceptable because the hook status tree lives under `/tmp`
/// (wiped on reboot) and every reader is stale-tolerant: the next hook
/// fire rewrites `session_id`, the filesystem-scan fallback in
/// `claude_poll_fn` recovers when the sidecar is missing, and
/// `attention.json` is a best-effort UI flag rather than authoritative
/// state. `crate::session::atomic_write` is the durable counterpart for
/// files that must survive a crash (e.g. persistent session storage).
///
/// Tmp name carries the PID and a process-local counter so multi-thread
/// writers of the same `name` get distinct tmpfiles and cannot collide via
/// `O_EXCL`.
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

/// One-shot helper used by the host-side `aoe __extract-session-id`
/// subcommand. Validates the instance id, opens the per-instance dir with
/// `dir_guard` discipline, and atomic-renames the session id sidecar.
pub(crate) fn write_session_id_via_guard(instance_id: &str, session_id: &str) -> Result<()> {
    let dir = open_instance_dir(instance_id)?;
    write_atomic(dir.as_fd(), "session_id", session_id.as_bytes())
}

/// Symlink-safe deletion of the `session_id` sidecar via `unlinkat` against
/// a `dir_guard`-verified per-instance dirfd. Replaces path-based
/// `std::fs::remove_file(dir.join("session_id"))` so deletion participates
/// in the same `*at`-anchored, mode-checked, owner-checked discipline as
/// every other hook write.
///
/// Idempotent: a missing dir or missing leaf returns `Ok(())`. Returns
/// `Err` only on guard-validation failure (squatted/wrong-mode base) or
/// hard `unlinkat` errors. Caller policy on `Err`: best-effort cleanup
/// (the next hook fire overwrites the sidecar anyway).
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

/// Ensure the per-instance hook directory exists with `dir_guard` discipline
/// and return its host path. Used by callers that hand the path to an
/// external resolver (Docker bind-mount source, sidecar config writer)
/// rather than performing in-process I/O directly.
///
/// The function calls `open_instance_dir` to verify-and-create with
/// `*at`+`O_NOFOLLOW`+`fstat-on-fd`, then drops the fd and returns the
/// resolved path. Closes both attack vectors that an unguarded
/// `create_dir_all` would re-introduce: self-DoS at default umask 022
/// (would create `0o755`, which `with_hook_base`'s `verify_dir_metadata`
/// would reject) and the multi-tenant pre-squat + symlink-swap race
/// against Docker's bind-mount resolution.
///
/// Caller policy on `Err`: skip the bind-mount push, surface a
/// `tracing::warn!` and let the agent boot without status hooks
/// (pane-detection fallback).
pub(crate) fn ensure_instance_dir_path(instance_id: &str) -> Result<PathBuf> {
    let _fd = open_instance_dir(instance_id)?;
    Ok(hook_base_path().join(instance_id))
}

// Cleanup.

/// Remove the per-instance subdir and every file inside, never following
/// symlinks. Re-fstats each entry's fd before unlink to close the
/// swap-between-stat-and-unlink window.
///
/// Subdirectories under the per-instance dir are NEVER created by AoE; if one
/// shows up it is hostile or stale. We refuse to descend; final `RemoveDir`
/// will return `ENOTEMPTY` and we surface that as a warn-skip.
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
                // Already gone, or hostile symlink: try to unlink whatever is at
                // the path so a future open succeeds. unlinkat without RemoveDir
                // removes the symlink itself, not its target (POSIX guarantee).
                let _ = unlinkat(base, instance_id, UnlinkatFlags::NoRemoveDir);
                return Ok(());
            }
            Err(e) => return Err(e).with_context(|| format!("openat cleanup {instance_id}")),
        };
        let label = hook_base_path().join(instance_id);
        if let Err(e) = verify_dir_metadata(&dir_fd, &label) {
            // Wrong owner / mode: refuse to walk; do not unlink either, the user
            // needs to inspect manually.
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
    // `Dir::from_fd` consumes the fd. Clone first so we keep the original for
    // unlinkat afterwards.
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
        // Re-validate the entry before removal. Open with O_NOFOLLOW so a
        // symlink at the leaf rejects with ELOOP rather than chasing the
        // target. Anything that is not a regular file (subdir, fifo, device)
        // is hostile or stale; we warn and skip.
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
                // Regular file we own (parent dir was uid-checked) → safe to
                // unlink the path-name within our verified dir fd.
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

    #[test]
    #[serial(hook_base)]
    fn init_rejects_dir_mode_0o755() {
        let (_g, base, _tmp) = BaseGuard::fresh();
        std::fs::create_dir(&base).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
        let err = with_hook_base(|_| Ok(())).unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("mode"), "expected mode rejection, got: {s}");
    }

    #[test]
    #[serial(hook_base)]
    fn init_rejects_dir_mode_0o770() {
        let (_g, base, _tmp) = BaseGuard::fresh();
        std::fs::create_dir(&base).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o770)).unwrap();
        let err = with_hook_base(|_| Ok(())).unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("mode"), "expected mode rejection, got: {s}");
    }

    #[test]
    #[serial(hook_base)]
    fn init_rejects_setgid_dir() {
        let (_g, base, _tmp) = BaseGuard::fresh();
        std::fs::create_dir(&base).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o2700)).unwrap();
        let err = with_hook_base(|_| Ok(())).unwrap_err();
        let s = format!("{err:#}");
        assert!(
            s.contains("setuid/setgid/sticky"),
            "expected setgid rejection, got: {s}"
        );
    }

    #[test]
    #[serial(hook_base)]
    fn init_rejects_setuid_dir() {
        let (_g, base, _tmp) = BaseGuard::fresh();
        std::fs::create_dir(&base).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o4700)).unwrap();
        let err = with_hook_base(|_| Ok(())).unwrap_err();
        let s = format!("{err:#}");
        assert!(
            s.contains("setuid/setgid/sticky"),
            "expected setuid rejection, got: {s}"
        );
    }

    #[test]
    #[serial(hook_base)]
    fn init_rejects_sticky_dir() {
        let (_g, base, _tmp) = BaseGuard::fresh();
        std::fs::create_dir(&base).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o1700)).unwrap();
        let err = with_hook_base(|_| Ok(())).unwrap_err();
        let s = format!("{err:#}");
        assert!(
            s.contains("setuid/setgid/sticky"),
            "expected sticky rejection, got: {s}"
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
        // Each thread writes a DISTINCT 36-byte payload so the post-condition
        // catches torn writes: a regression that drops the rename and just
        // truncates would leave whatever the last (interleaved) writer wrote
        // partway through, which would not match any single thread's full
        // 36-byte payload byte-for-byte.
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
        // /tmp -> /private/tmp on macOS. O_NOFOLLOW only checks the FINAL
        // component, so a prefix symlink must not block init. Locks the
        // platform-specific resolution behavior the existing code relies on.
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
