//! Pins the real-tmux assumption behind the false-Error-latch fix
//! (`probe_session_existence`): `list-sessions` exits non-zero when the
//! tmux *server* itself is unreachable (dead socket, refused connection),
//! not only when a specific session is gone. That failure must resolve to
//! `SessionExistence::Unknown`, never `Absent`; collapsing the two is what
//! made a transient tmux hiccup look like every session died.
//!
//! Runs against the real tmux binary on the shared integration socket
//! (`common::tmux_socket`), so it needs tmux on `PATH` and auto-skips
//! otherwise, matching this suite's existing skip convention.

use agent_of_empires::tmux::{self, SessionExistence};
use serial_test::serial;
use std::process::Command;

use crate::common::tmux_socket;

fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Best-effort cleanup for the session created before the deliberate
/// `kill-server`. Harmless (and a no-op) once the server is already dead;
/// exists only to cover the case where an assertion panics before that
/// point.
struct SessionCleanup<'a> {
    socket: &'a std::path::Path,
    name: &'a str,
}

impl Drop for SessionCleanup<'_> {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .arg("-S")
            .arg(self.socket)
            .args(["kill-session", "-t", self.name])
            .output();
    }
}

#[test]
#[serial]
fn probe_session_existence_reports_unknown_after_server_killed() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }

    let socket = tmux_socket();
    let session_name = format!("{}reachability_probe", tmux::SESSION_PREFIX);
    let _cleanup = SessionCleanup {
        socket: &socket,
        name: &session_name,
    };

    let status = Command::new("tmux")
        .arg("-S")
        .arg(&socket)
        .args(["new-session", "-d", "-s", &session_name])
        .status()
        .expect("tmux new-session");
    assert!(status.success(), "tmux new-session failed");

    // Force a fresh cache read against the real server before asserting
    // Present, so a prior `#[serial]` test's stale snapshot can't leak in.
    tmux::refresh_session_cache();
    assert_eq!(
        tmux::probe_session_existence(&session_name),
        SessionExistence::Present,
        "session must be Present while the server is up and the session exists"
    );

    // Kill the whole server, not just the session, to reproduce the real
    // production scenario: a transient refusal on the socket, not a
    // genuinely-gone session.
    let status = Command::new("tmux")
        .arg("-S")
        .arg(&socket)
        .arg("kill-server")
        .status()
        .expect("tmux kill-server");
    assert!(status.success(), "tmux kill-server failed");

    tmux::refresh_session_cache();
    assert_eq!(
        tmux::probe_session_existence(&session_name),
        SessionExistence::Unknown,
        "a dead tmux server must resolve to Unknown, never Absent"
    );
}
