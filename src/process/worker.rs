//! Protocol-agnostic plumbing for supervised worker subprocesses.
//!
//! This is the neutral substrate that both `src/acp/` and the future
//! plugin host build on: process-group signalling and liveness probes,
//! the on-disk layout helpers for a `<dir>/<id>.{json,sock,log,restart}`
//! worker directory, and the runner self-inspection state machine. None
//! of it knows about ACP, agents, or any specific worker payload; the
//! consumer supplies the base directory and (for record inspection) how
//! to pull a pid out of its own record format. The dependency arrow runs
//! consumer -> here, never the reverse.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Probe whether `pid` is still alive. On Unix: `kill(pid, 0)` returns
/// `Ok(())` for live and `Err(ESRCH)` for dead. Other errors (EPERM,
/// etc.) mean the process exists but we lack permission to signal it,
/// still alive.
#[cfg(unix)]
pub fn is_pid_alive(pid: u32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    match kill(Pid::from_raw(pid as i32), None) {
        Ok(()) => true,
        Err(Errno::ESRCH) => false,
        Err(_) => true,
    }
}

#[cfg(not(unix))]
pub fn is_pid_alive(_pid: u32) -> bool {
    false
}

/// Ask the kernel which process is listening on this Unix domain socket
/// by connecting and reading the peer's credentials. Returns `Some(pid)`
/// if the path resolves to a live UDS with a valid peer, `None` otherwise
/// (path missing, wrong file type, peer already gone, connect timeout,
/// or a target other than Linux/Android/macOS).
///
/// Callers (`worker_registry::terminate`, `shutdown_and_wait`) use this
/// as the fallback source of the runner PID when the on-disk record is
/// unreadable at the I/O layer (permissions, wrong file type, transient
/// failure), so an unreadable record no longer means the runner escapes
/// both SIGTERM and the wait. Note: `worker_registry::load` coerces
/// `serde_json` parse errors to `Ok(None)`, so corrupt JSON stays in
/// the "runner already gone" bucket and does not reach this fallback.
///
/// Timeout: connect is bounded at 100ms via non-blocking `connect(2)`
/// plus `poll(POLLOUT)`. Prevents a wedged runner (D-state kernel
/// thread, accept loop hung, kernel memory pressure) from stalling the
/// calling tokio worker thread. Timeouts and completed-but-failed
/// connects (`SO_ERROR != 0`) fall into the same `None` bucket as any
/// other failure.
///
/// See #2102, #2621.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn peer_pid_from_socket(path: &Path) -> Option<u32> {
    use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
    let stream = connect_with_timeout(path)?;
    let creds = getsockopt(&stream, PeerCredentials).ok()?;
    let pid = creds.pid();
    (pid > 0).then_some(pid as u32)
}

#[cfg(target_os = "macos")]
pub fn peer_pid_from_socket(path: &Path) -> Option<u32> {
    use nix::sys::socket::{getsockopt, sockopt::LocalPeerPid};
    let stream = connect_with_timeout(path)?;
    let pid = getsockopt(&stream, LocalPeerPid).ok()?;
    (pid > 0).then_some(pid as u32)
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
pub fn peer_pid_from_socket(_path: &Path) -> Option<u32> {
    None
}

/// Non-blocking `connect(2)` to a Unix domain socket, capped at 100ms via
/// `poll(POLLOUT)`. Returns the connected `UnixStream` on success, or
/// `None` on any failure (unreachable path, refused, timeout, syscall
/// error), preserving the best-effort semantics of
/// [`peer_pid_from_socket`]. See #2621.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
fn connect_with_timeout(path: &Path) -> Option<std::os::unix::net::UnixStream> {
    use std::os::fd::{AsFd, AsRawFd};
    use std::os::unix::net::UnixStream;

    use nix::errno::Errno;
    use nix::fcntl::{fcntl, FcntlArg, FdFlag, OFlag};
    use nix::poll::{poll, PollFd, PollFlags};
    use nix::sys::socket::{
        connect, getsockopt, socket, sockopt::SocketError, AddressFamily, SockFlag, SockType,
        UnixAddr,
    };

    let addr = UnixAddr::new(path).ok()?;
    let fd = socket(
        AddressFamily::Unix,
        SockType::Stream,
        SockFlag::empty(),
        None,
    )
    .ok()?;
    // `SockFlag::SOCK_NONBLOCK` and `SOCK_CLOEXEC` are gated to
    // linux_android/BSD in nix 0.31, so set `FD_CLOEXEC` and
    // `O_NONBLOCK` via fcntl for portability with macOS. Matches
    // `std::os::unix::net::UnixStream::connect`, which sets
    // `FD_CLOEXEC` on the returned fd.
    fcntl(fd.as_fd(), FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).ok()?;
    fcntl(fd.as_fd(), FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).ok()?;

    match connect(fd.as_raw_fd(), &addr) {
        Ok(()) => {}
        // `EAGAIN` is Linux AF_UNIX's variant of `EINPROGRESS`
        // (`unix(7)`): connect cannot complete immediately; the
        // same POLLOUT wait applies.
        Err(Errno::EINPROGRESS | Errno::EAGAIN) => {
            let mut pfds = [PollFd::new(fd.as_fd(), PollFlags::POLLOUT)];
            // 100ms: same-host UDS connect completes in microseconds
            // against a healthy listener; this cap tolerates light
            // scheduler contention while bounding the pathological case
            // (hung accept loop, kernel memory pressure).
            if poll(&mut pfds, 100u16).ok()? == 0 {
                return None;
            }
            // POLLOUT also fires on connect failure (ECONNREFUSED, etc.);
            // check `SO_ERROR` before trusting the socket.
            if getsockopt(&fd, SocketError).ok()? != 0 {
                return None;
            }
        }
        Err(_) => return None,
    }

    Some(UnixStream::from(fd))
}

/// Signal a worker's entire process group, then the worker pid itself.
///
/// A worker is spawned `setsid` (a fresh session, so it is the leader of
/// a process group whose id equals its pid), and any subprocess it
/// launches plus their children inherit that group. Signalling the group
/// reaps the whole tree in one shot. Signalling only the leader pid leaves
/// descendants orphaned under PID 1, which is the process-leak that
/// accumulated across daemon restarts and superseded spawns (#1689). The
/// trailing single-pid signal is a belt-and-suspenders for the unlikely
/// case `setsid` failed and the leader is not a group leader. Best-effort;
/// errors are ignored.
#[cfg(unix)]
fn signal_process_group(pid: u32, sig: nix::sys::signal::Signal) {
    use nix::sys::signal::{kill, killpg};
    use nix::unistd::Pid;
    let p = Pid::from_raw(pid as i32);
    let _ = killpg(p, sig);
    let _ = kill(p, sig);
}

/// SIGTERM the worker's process group (leader + descendants).
pub fn terminate_process_group(pid: u32) {
    #[cfg(unix)]
    signal_process_group(pid, nix::sys::signal::Signal::SIGTERM);
    #[cfg(not(unix))]
    let _ = pid;
}

/// SIGKILL the worker's process group; the escalation path when SIGTERM
/// does not take.
pub fn kill_process_group(pid: u32) {
    #[cfg(unix)]
    signal_process_group(pid, nix::sys::signal::Signal::SIGKILL);
    #[cfg(not(unix))]
    let _ = pid;
}

/// SIGKILL the calling process's own group, but only when this process is
/// its group leader (i.e. `setsid` succeeded). Returns `true` when the
/// group was killed, `false` when the caller must fall back to killing its
/// direct child (either `setsid` failed and the group is shared, or the
/// platform is non-unix). Guarding on leadership prevents a failed `setsid`
/// from SIGKILLing an inherited group, e.g. the daemon's.
#[cfg(unix)]
pub fn kill_own_process_group_if_leader(own_pid: u32) -> bool {
    use nix::unistd::{getpgrp, getpid};
    if getpgrp() == getpid() {
        kill_process_group(own_pid);
        true
    } else {
        false
    }
}

#[cfg(not(unix))]
pub fn kill_own_process_group_if_leader(_own_pid: u32) -> bool {
    false
}

/// Reap a worker process group with SIGKILL escalation: SIGTERM the group,
/// wait `grace` for it to exit, then SIGKILL the group. A bare SIGTERM can
/// leave a grandchild that ignores it alive under PID 1, so the escalation
/// is what actually guarantees the tree dies. `killpg` ignores ESRCH, so
/// the SIGKILL on an already-empty group is a no-op. See #1921.
#[cfg(unix)]
pub async fn reap_group_escalating(pid: u32, grace: std::time::Duration) {
    terminate_process_group(pid);
    tokio::time::sleep(grace).await;
    kill_process_group(pid);
}

/// Defense-in-depth check on a worker id before it is interpolated into
/// any `<dir>/<id>.<ext>` path. Production ids come from `Uuid::new_v4()`
/// so they satisfy this trivially, but ids can also arrive from a CLI arg,
/// and we don't want an id like `"../../foo"` to write files outside the
/// dedicated worker directory. Not a privilege escalation (same UID), but
/// a basic input-validation gap worth closing.
///
/// Accepts: alphanumeric, `-`, `_`. Rejects: empty, `/`, `\`, `.` (so
/// `..` and leading-dot hidden files are both out), null bytes, and
/// anything longer than 128 bytes (UUIDs are 36; this leaves room for
/// prefixed test ids without permitting arbitrarily-long inputs).
pub fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() {
        anyhow::bail!("worker id must not be empty");
    }
    if id.len() > 128 {
        anyhow::bail!("worker id too long ({} bytes, max 128)", id.len());
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        anyhow::bail!(
            "worker id contains disallowed characters: must be ASCII alphanumeric, '-', or '_'"
        );
    }
    Ok(())
}

/// Create the worker directory if absent and enforce owner-only (0700) so
/// other users on a shared host cannot enumerate worker ids. The permission
/// is (re)applied on every call, including a pre-existing directory, and a
/// failure to set it is propagated rather than swallowed: the isolation
/// guarantee is load-bearing, so callers fail closed instead of proceeding
/// with a world-readable worker dir.
pub fn ensure_dir(dir: &Path) -> Result<()> {
    if !dir.exists() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating worker dir at {}", dir.display()))?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).with_context(
            || {
                format!(
                    "setting owner-only perms on worker dir at {}",
                    dir.display()
                )
            },
        )?;
    }
    Ok(())
}

/// `<dir>/<id>.json`, the worker record path.
pub fn record_path(dir: &Path, id: &str) -> Result<PathBuf> {
    validate_id(id)?;
    Ok(dir.join(format!("{id}.json")))
}

/// `<dir>/<id>.sock`. The caller threads the same path into both the
/// worker spawn and the connect side.
pub fn socket_path(dir: &Path, id: &str) -> Result<PathBuf> {
    validate_id(id)?;
    Ok(dir.join(format!("{id}.sock")))
}

/// `<dir>/<id>.log`, the worker-side stderr drain.
pub fn log_path(dir: &Path, id: &str) -> Result<PathBuf> {
    validate_id(id)?;
    Ok(dir.join(format!("{id}.log")))
}

/// `<dir>/<id>.restart`, a sentinel that distinguishes a restart-driven
/// teardown from a stop/kill so the reaper can react accordingly.
pub fn restart_marker_path(dir: &Path, id: &str) -> Result<PathBuf> {
    validate_id(id)?;
    Ok(dir.join(format!("{id}.restart")))
}

/// What a worker's own registry record looks like from its watchdog's
/// point of view. Computed from a non-creating read of a path captured at
/// startup; see [`inspect_record_for_runner`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerRecordState {
    /// Record present and its pid matches ours: we are still the owner.
    Matches,
    /// Record file is gone (HOME deleted, or someone `delete`d it).
    Missing,
    /// Record present but owned by a different pid: a fresh worker has
    /// superseded us. We must exit without touching the files, which now
    /// belong to the new owner.
    Superseded,
    /// Read or parse failed for a reason other than absence. Treated as
    /// non-fatal (transient FS hiccup) by the watchdog.
    Unreadable,
}

/// Inspect a worker's registry record WITHOUT creating its directory.
///
/// The watchdog cannot go through the normal load path, because that path
/// recreates the worker directory whose deletion is the watchdog's primary
/// self-destruct signal (and would resurrect a temp `$HOME` a test just
/// removed). The caller captures the concrete record path once at startup,
/// while the dir still exists, and polls it here. See #1921.
///
/// `extract_pid` pulls the owner pid out of the consumer's own record
/// bytes; returning `None` means the bytes did not parse and the record is
/// reported [`RunnerRecordState::Unreadable`]. Keeping the parse on the
/// consumer side is what lets this module stay payload-agnostic while
/// preserving the consumer's exact "malformed record is non-fatal"
/// semantics.
pub fn inspect_record_for_runner(
    record_path: &Path,
    own_pid: u32,
    extract_pid: impl FnOnce(&[u8]) -> Option<u32>,
) -> RunnerRecordState {
    let bytes = match std::fs::read(record_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return RunnerRecordState::Missing,
        Err(_) => return RunnerRecordState::Unreadable,
    };
    match extract_pid(&bytes) {
        Some(pid) if pid == own_pid => RunnerRecordState::Matches,
        Some(_) => RunnerRecordState::Superseded,
        None => RunnerRecordState::Unreadable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // On non-Unix `is_pid_alive` deliberately returns false, so this
    // liveness expectation only holds under Unix.
    #[cfg(unix)]
    #[test]
    fn is_pid_alive_self() {
        assert!(is_pid_alive(std::process::id()));
    }

    #[test]
    fn is_pid_alive_unlikely_pid() {
        // A very high value that won't realistically be allocated.
        assert!(!is_pid_alive(2_000_000_000));
    }

    #[test]
    fn validate_id_accepts_uuids_and_test_ids() {
        assert!(validate_id("550e8400-e29b-41d4-a716-446655440000").is_ok());
        assert!(validate_id("test_session_42").is_ok());
        assert!(validate_id("a").is_ok());
        assert!(validate_id("Z-0").is_ok());
    }

    #[test]
    fn validate_id_rejects_path_traversal_and_separators() {
        for bad in [
            "",
            "..",
            "../../etc/passwd",
            "foo/bar",
            "foo\\bar",
            ".hidden",
            "with space",
            "with\0null",
            "trailing.",
            "good-then/../bad",
        ] {
            assert!(validate_id(bad).is_err(), "expected rejection for {bad:?}");
        }
    }

    #[test]
    fn validate_id_rejects_overlong() {
        assert!(validate_id(&"a".repeat(129)).is_err());
        assert!(validate_id(&"a".repeat(128)).is_ok());
    }

    /// The path builders are parameterized by an arbitrary base dir, not a
    /// hardcoded ACP one: this is what makes them reusable by the plugin
    /// host. Prove they compose against a non-ACP directory and reject bad
    /// ids regardless of dir.
    #[test]
    fn path_builders_use_arbitrary_dir_and_validate() {
        let dir = Path::new("/var/lib/example-workers");
        assert_eq!(
            record_path(dir, "abc").unwrap(),
            PathBuf::from("/var/lib/example-workers/abc.json")
        );
        assert_eq!(
            socket_path(dir, "abc").unwrap(),
            PathBuf::from("/var/lib/example-workers/abc.sock")
        );
        assert_eq!(
            log_path(dir, "abc").unwrap(),
            PathBuf::from("/var/lib/example-workers/abc.log")
        );
        assert_eq!(
            restart_marker_path(dir, "abc").unwrap(),
            PathBuf::from("/var/lib/example-workers/abc.restart")
        );
        assert!(record_path(dir, "../escape").is_err());
        assert!(socket_path(dir, "foo/bar").is_err());
        assert!(log_path(dir, "").is_err());
        assert!(restart_marker_path(dir, ".hidden").is_err());
    }

    #[test]
    fn ensure_dir_creates_owner_only() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join("workers");
        assert!(!dir.exists());
        ensure_dir(&dir).unwrap();
        assert!(dir.is_dir());
        // Idempotent.
        ensure_dir(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }

    /// The pid extractor closure preserves the consumer's exact semantics:
    /// a parse failure is non-fatal (`Unreadable`), a matching pid is
    /// `Matches`, a foreign pid is `Superseded`, and an absent file is
    /// `Missing`.
    #[test]
    fn inspect_record_states() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("rec.json");
        // Parses to our pid -> Matches.
        std::fs::write(&path, br#"{"pid":42}"#).unwrap();
        let extract = |b: &[u8]| -> Option<u32> {
            serde_json::from_slice::<serde_json::Value>(b)
                .ok()
                .and_then(|v| v.get("pid").and_then(|p| p.as_u64()).map(|p| p as u32))
        };
        assert_eq!(
            inspect_record_for_runner(&path, 42, extract),
            RunnerRecordState::Matches
        );
        // Parses to a foreign pid -> Superseded.
        assert_eq!(
            inspect_record_for_runner(&path, 7, extract),
            RunnerRecordState::Superseded
        );
        // Unparseable bytes -> Unreadable (non-fatal), NOT Matches.
        std::fs::write(&path, b"{not json").unwrap();
        assert_eq!(
            inspect_record_for_runner(&path, 42, extract),
            RunnerRecordState::Unreadable
        );
        // Absent file -> Missing.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            inspect_record_for_runner(&path, 42, extract),
            RunnerRecordState::Missing
        );
    }

    #[test]
    fn peer_pid_from_socket_missing_path_returns_none_bounded() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("does-not-exist.sock");
        let start = std::time::Instant::now();
        assert_eq!(peer_pid_from_socket(&path), None);
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn peer_pid_from_socket_non_socket_file_returns_none_bounded() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("not-a-socket");
        std::fs::write(&path, b"regular file").unwrap();
        let start = std::time::Instant::now();
        assert_eq!(peer_pid_from_socket(&path), None);
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    fn peer_pid_from_socket_healthy_listener_returns_our_pid_bounded() {
        use std::os::unix::net::UnixListener;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("healthy.sock");
        let _listener = UnixListener::bind(&path).unwrap();
        let start = std::time::Instant::now();
        let pid = peer_pid_from_socket(&path);
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(pid, Some(std::process::id()));
    }
}
