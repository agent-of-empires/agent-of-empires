//! Protocol-agnostic plumbing for supervised worker subprocesses: process-group signals,
//! liveness probes, `<dir>/<id>.{json,sock,log,restart}` paths, and record self-inspection.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// `EPERM` means the process exists but belongs to someone else, so it counts as alive.
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

/// `EPERM` counts as dead here: a pid we cannot signal is a reused pid, not our runner.
#[cfg(unix)]
pub fn is_pid_alive_and_ours(pid: u32) -> bool {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    kill(Pid::from_raw(pid as i32), None).is_ok()
}

#[cfg(not(unix))]
pub fn is_pid_alive_and_ours(_pid: u32) -> bool {
    false
}

/// The pid listening on a Unix socket via peer credentials, used when the record is
/// unreadable. Connect is capped at 100ms so a wedged runner cannot stall the caller.
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
    // `SOCK_NONBLOCK`/`SOCK_CLOEXEC` are Linux/BSD-only in nix, so set them via fcntl.
    fcntl(fd.as_fd(), FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).ok()?;
    fcntl(fd.as_fd(), FcntlArg::F_SETFL(OFlag::O_NONBLOCK)).ok()?;

    match connect(fd.as_raw_fd(), &addr) {
        Ok(()) => {}
        // Linux AF_UNIX reports `EAGAIN` where others report `EINPROGRESS`.
        Err(Errno::EINPROGRESS | Errno::EAGAIN) => {
            let mut pfds = [PollFd::new(fd.as_fd(), PollFlags::POLLOUT)];
            if poll(&mut pfds, 100u16).ok()? == 0 {
                return None;
            }
            // POLLOUT also fires on connect failure; check `SO_ERROR`.
            if getsockopt(&fd, SocketError).ok()? != 0 {
                return None;
            }
        }
        Err(_) => return None,
    }

    Some(UnixStream::from(fd))
}

/// Workers are spawned with `setsid`, so the group reaps the whole tree; the trailing
/// single-pid signal covers a failed `setsid`.
#[cfg(unix)]
fn signal_process_group(pid: u32, sig: nix::sys::signal::Signal) {
    use nix::sys::signal::{kill, killpg};
    use nix::unistd::Pid;
    let p = Pid::from_raw(pid as i32);
    let _ = killpg(p, sig);
    let _ = kill(p, sig);
}

pub fn terminate_process_group(pid: u32) {
    #[cfg(unix)]
    signal_process_group(pid, nix::sys::signal::Signal::SIGTERM);
    #[cfg(not(unix))]
    let _ = pid;
}

pub fn kill_process_group(pid: u32) {
    #[cfg(unix)]
    signal_process_group(pid, nix::sys::signal::Signal::SIGKILL);
    #[cfg(not(unix))]
    let _ = pid;
}

/// Only when this process leads its group, so a failed `setsid` never kills an inherited
/// group such as the daemon's.
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

/// A bare SIGTERM can leave a grandchild alive under PID 1; the SIGKILL guarantees the tree dies.
#[cfg(unix)]
pub async fn reap_group_escalating(pid: u32, grace: std::time::Duration) {
    terminate_process_group(pid);
    tokio::time::sleep(grace).await;
    kill_process_group(pid);
}

/// Ids are interpolated into paths: allow only alphanumerics, `-`, `_`, up to 128 bytes.
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

/// Enforces 0700 on every call and fails closed if it cannot.
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

pub fn record_path(dir: &Path, id: &str) -> Result<PathBuf> {
    validate_id(id)?;
    Ok(dir.join(format!("{id}.json")))
}

pub fn socket_path(dir: &Path, id: &str) -> Result<PathBuf> {
    validate_id(id)?;
    Ok(dir.join(format!("{id}.sock")))
}

pub fn log_path(dir: &Path, id: &str) -> Result<PathBuf> {
    validate_id(id)?;
    Ok(dir.join(format!("{id}.log")))
}

pub fn control_socket_sibling(main_socket: &Path) -> PathBuf {
    // Self-application would yield `x.control.control.sock`.
    debug_assert!(
        !main_socket.to_string_lossy().ends_with(".control.sock"),
        "control_socket_sibling called on an already-derived control path: {}",
        main_socket.display()
    );
    main_socket.with_extension("control.sock")
}

pub fn restart_marker_path(dir: &Path, id: &str) -> Result<PathBuf> {
    validate_id(id)?;
    Ok(dir.join(format!("{id}.restart")))
}

pub fn read_restart_marker(path: &Path) -> Option<u64> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunnerRecordState {
    Matches,
    Missing,
    /// A fresh worker owns the files now; exit without touching them.
    Superseded,
    Unreadable,
}

/// Never creates the directory: its deletion is the watchdog's self-destruct signal.
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

    #[test]
    fn is_pid_alive_separates_this_process_from_an_unused_pid() {
        // On non-Unix `is_pid_alive` always returns false.
        #[cfg(unix)]
        assert!(is_pid_alive(std::process::id()));
        assert!(!is_pid_alive(2_000_000_000));
    }

    #[test]
    fn validate_id_accepts_ids_and_rejects_path_shapes() {
        for ok in [
            // The production session_id shape.
            "550e8400-e29b-41d4-a716-446655440000",
            "test_session_42",
            "a",
            "Z-0",
            &"a".repeat(128),
        ] {
            assert!(validate_id(ok).is_ok(), "expected {ok:?} to pass");
        }
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
            &"a".repeat(129),
        ] {
            assert!(validate_id(bad).is_err(), "expected rejection for {bad:?}");
        }
    }

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
        ensure_dir(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }

    #[test]
    fn inspect_record_states() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("rec.json");
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
        assert_eq!(
            inspect_record_for_runner(&path, 7, extract),
            RunnerRecordState::Superseded
        );
        std::fs::write(&path, b"{not json").unwrap();
        assert_eq!(
            inspect_record_for_runner(&path, 42, extract),
            RunnerRecordState::Unreadable
        );
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            inspect_record_for_runner(&path, 42, extract),
            RunnerRecordState::Missing
        );
    }

    /// A missing path or a non-socket answers `None`, and every answer comes
    /// back well inside the probe bound.
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    fn peer_pid_from_socket_answers_within_its_bound() {
        use std::os::unix::net::UnixListener;
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("not-a-socket"), b"regular file").unwrap();
        let _listener = UnixListener::bind(tmp.path().join("healthy.sock")).unwrap();
        for (name, expected) in [
            ("does-not-exist.sock", None),
            ("not-a-socket", None),
            ("healthy.sock", Some(std::process::id())),
        ] {
            let start = std::time::Instant::now();
            assert_eq!(
                peer_pid_from_socket(&tmp.path().join(name)),
                expected,
                "{name}"
            );
            assert!(
                start.elapsed() < std::time::Duration::from_secs(1),
                "{name}"
            );
        }
    }
}
