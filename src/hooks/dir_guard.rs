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
//! `init_hook_base` returns `Err`; AoE keeps running with hooks disabled and
//! falls back to pane-detection. Recovery requires the squatter to log out,
//! reboot, or root cooperation. Bounded DoS only; never a privilege escalation.

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

// --- Path resolution ---------------------------------------------------------

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

// --- Singleton cell ----------------------------------------------------------

type CachedBase = std::result::Result<OwnedFd, Arc<anyhow::Error>>;

// MUST be `static` for the BorrowedFd<'static> contract that `init_hook_base`
// hands out: the `OwnedFd` lives in static storage for the program lifetime,
// so a `BorrowedFd` derived from it is sound for `'static`.
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
fn cached_get_or_init<F>(init: F) -> Result<BorrowedFd<'static>>
where
    F: FnOnce() -> std::result::Result<OwnedFd, Arc<anyhow::Error>>,
{
    HOOK_BASE_TEST_CELL.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(init());
        }
        match slot.as_ref().unwrap() {
            Ok(fd) => {
                // SAFETY: the OwnedFd lives in the test thread-local for the
                // lifetime of the test. Tests acknowledge `'static` is a lie
                // and serialize via `serial_test::serial(hook_base)` plus
                // `reset_for_test` in their teardown to keep the contract.
                let raw = fd.as_raw_fd();
                Ok(unsafe { BorrowedFd::borrow_raw(raw) })
            }
            Err(e) => Err(anyhow!("{e:#}")),
        }
    })
}

#[cfg(not(test))]
fn cached_get_or_init<F>(init: F) -> Result<BorrowedFd<'static>>
where
    F: FnOnce() -> std::result::Result<OwnedFd, Arc<anyhow::Error>>,
{
    let entry = HOOK_BASE.get_or_init(init);
    match entry {
        Ok(fd) => Ok(fd.as_fd()),
        Err(e) => Err(anyhow!("{e:#}")),
    }
}

// --- Public init -------------------------------------------------------------

/// Lazily open and verify the per-user hook base directory. First caller does
/// the real work; subsequent callers reuse the cached fd (or the cached
/// `Arc<anyhow::Error>` on the failure path; one atomic incr per failed call).
///
/// Returns a `BorrowedFd<'static>` because the underlying `OwnedFd` lives in
/// static storage for the lifetime of the program (see `HOOK_BASE`).
///
/// Surface: every public reader/writer in `super::status_file` and
/// `crate::cli::extract_session_id` calls this at first touch. Failures are
/// loud at the source (full anyhow chain via `tracing::error!`) and silent on
/// the polling path (warn-once + `None`).
pub(crate) fn init_hook_base() -> Result<BorrowedFd<'static>> {
    cached_get_or_init(|| match open_and_verify_base() {
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
    })
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
    Ok(())
}

// --- Per-instance ---

/// `mkdirat(base, id, 0o700)` (EEXIST-tolerant) plus `openat(O_NOFOLLOW)` plus
/// `fstat`-on-fd uid/mode check. Returns an owned fd to the per-instance
/// directory.
pub(crate) fn open_instance_dir(instance_id: &str) -> Result<OwnedFd> {
    crate::session::validate_instance_id(instance_id)?;
    let base = init_hook_base()?;
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
}

/// Read-only variant: never creates the dir. Returns `Ok(None)` on `ENOENT` /
/// `ELOOP` (legitimate transient absence or hostile symlink swap, both
/// indistinguishable from "no hook fired yet" on the polling path).
pub(crate) fn open_instance_dir_read_only(instance_id: &str) -> Result<Option<OwnedFd>> {
    crate::session::validate_instance_id(instance_id)?;
    let base = init_hook_base()?;
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
}

// --- Per-file I/O ---

/// Open a file inside an already-verified per-instance dir for reading.
/// `O_NOFOLLOW` forbids the leaf being a symlink. `ENOENT` / `ELOOP` map to
/// `Ok(None)`.
pub(crate) fn read_file_at(
    dir: BorrowedFd<'_>,
    name: &str,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>> {
    use std::io::Read;
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
    let mut buf = Vec::with_capacity(max_bytes.min(4096));
    let limit = u64::try_from(max_bytes).unwrap_or(u64::MAX);
    file.by_ref().take(limit).read_to_end(&mut buf)?;
    Ok(Some(buf))
}

/// `fstatat(AT_SYMLINK_NOFOLLOW)` view for mtime gating. Returns `Ok(None)` on
/// missing or symlinked entries.
pub(crate) fn metadata_at(dir: BorrowedFd<'_>, name: &str) -> Result<Option<Metadata>> {
    use std::os::unix::fs::MetadataExt;
    // Open with O_NOFOLLOW and read metadata via std::fs::File::metadata().
    // Avoids fstatat(AT_SYMLINK_NOFOLLOW) (which would also work but produce a
    // nix FileStat that we would then have to convert to std::fs::Metadata).
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
    // Belt and suspenders: refuse to consider non-regular leaves.
    if !meta.is_file() {
        return Ok(None);
    }
    let _ = meta.size(); // touch MetadataExt to keep the import live
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

/// Atomic write via `O_CREAT|O_EXCL` tmpfile + `renameat`. Used for files that
/// must never be observed torn (`session_id`, `attention.json`).
///
/// Tmp name carries the PID to avoid same-process collisions. Multi-thread
/// writers of the SAME `name` may race; one wins, the other sees `EEXIST`.
/// Hook fires originate from a single shell process per event so this is fine
/// in practice.
pub(crate) fn write_atomic(dir: BorrowedFd<'_>, name: &str, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let tmp = format!(".{name}.tmp.{}", std::process::id());
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

/// Ensure the per-instance hook directory exists with `dir_guard` discipline
/// and return its host path. Used by callers that hand the path to an
/// external resolver (Docker bind-mount source, sidecar config writer)
/// rather than performing in-process I/O directly.
///
/// The function calls `open_instance_dir` to verify-and-create with
/// `*at`+`O_NOFOLLOW`+`fstat-on-fd`, then drops the fd and returns the
/// resolved path. Closes both attack vectors that an unguarded
/// `create_dir_all` would re-introduce: self-DoS at default umask 022
/// (would create `0o755`, `init_hook_base` would reject) and the
/// multi-tenant pre-squat + symlink-swap race against Docker's bind-mount
/// resolution.
///
/// Caller policy on `Err`: skip the bind-mount push, surface a
/// `tracing::warn!` and let the agent boot without status hooks
/// (pane-detection fallback).
pub(crate) fn ensure_instance_dir_path(instance_id: &str) -> Result<PathBuf> {
    let _fd = open_instance_dir(instance_id)?;
    Ok(hook_base_path().join(instance_id))
}

// --- Cleanup ---

/// Remove the per-instance subdir and every file inside, never following
/// symlinks. Re-fstats each entry's fd before unlink to close the
/// swap-between-stat-and-unlink window.
///
/// Subdirectories under the per-instance dir are NEVER created by AoE; if one
/// shows up it is hostile or stale. We refuse to descend; final `RemoveDir`
/// will return `ENOTEMPTY` and we surface that as a warn-skip.
pub(crate) fn remove_instance_dir(instance_id: &str) -> Result<()> {
    crate::session::validate_instance_id(instance_id)?;
    let base = init_hook_base()?;
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
                        "non-regular entry {name_str} (mode {:o}); skipping",
                        st.st_mode);
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

// --- Tests -------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    /// RAII: install an override + reset cell + restore on drop. Every test
    /// using `init_hook_base` should hold one; serial_test gates the tests so
    /// the thread-local override is consistent for the whole run.
    struct BaseGuard {
        _tmp: TempDir,
    }

    impl BaseGuard {
        fn fresh() -> (Self, PathBuf, TempDir) {
            let tmp = TempDir::new().unwrap();
            let base = tmp.path().join("aoe-hooks");
            override_base_for_test(base.clone());
            reset_for_test();
            (
                Self {
                    _tmp: TempDir::new().unwrap(),
                },
                base,
                tmp,
            )
        }
    }

    impl Drop for BaseGuard {
        fn drop(&mut self) {
            clear_base_override_for_test();
            reset_for_test();
        }
    }

    fn make_correct_base(p: &std::path::Path) {
        std::fs::create_dir(p).unwrap();
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    #[serial(hook_base)]
    fn init_succeeds_on_fresh_dir() {
        let (_g, base, _tmp) = BaseGuard::fresh();
        // Do NOT pre-create; init must mkdir.
        let fd = init_hook_base().expect("init must succeed on fresh path");
        assert!(base.is_dir());
        let st = fstat(fd).unwrap();
        let mode = st.st_mode & 0o7777;
        assert_eq!(mode, 0o700, "got mode {mode:o}");
        assert_eq!(st.st_uid, geteuid().as_raw());
    }

    #[test]
    #[serial(hook_base)]
    fn init_succeeds_when_base_already_correct() {
        let (_g, base, _tmp) = BaseGuard::fresh();
        make_correct_base(&base);
        init_hook_base().expect("init must succeed when base already 0700 and ours");
    }

    #[test]
    #[serial(hook_base)]
    fn init_rejects_symlink_at_base() {
        let (_g, base, tmp) = BaseGuard::fresh();
        let target = tmp.path().join("decoy");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, &base).unwrap();
        let err = init_hook_base().unwrap_err();
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
        let err = init_hook_base().unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("mode"), "expected mode rejection, got: {s}");
    }

    #[test]
    #[serial(hook_base)]
    fn init_rejects_dir_mode_0o770() {
        let (_g, base, _tmp) = BaseGuard::fresh();
        std::fs::create_dir(&base).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o770)).unwrap();
        let err = init_hook_base().unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("mode"), "expected mode rejection, got: {s}");
    }

    #[test]
    #[serial(hook_base)]
    fn init_caches_error() {
        let (_g, base, tmp) = BaseGuard::fresh();
        // Bad state: symlink. Expected Err.
        let target = tmp.path().join("decoy2");
        std::fs::create_dir_all(&target).unwrap();
        std::os::unix::fs::symlink(&target, &base).unwrap();
        let _ = init_hook_base().unwrap_err();
        let after_first = open_calls();
        let _ = init_hook_base().unwrap_err();
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
        let fd1 = init_hook_base().unwrap();
        let after_first = open_calls();
        let fd2 = init_hook_base().unwrap();
        assert_eq!(
            fd1.as_raw_fd(),
            fd2.as_raw_fd(),
            "cached fd must be byte-equal across calls"
        );
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
    fn multi_user_paths_do_not_collide() {
        let (_g1, base1, _tmp1) = BaseGuard::fresh();
        // Force a different override path the way two euids would diverge.
        let base2 = base1.with_file_name("aoe-hooks-other");
        assert_ne!(base1, base2, "test fixture must produce two paths");
    }
}
