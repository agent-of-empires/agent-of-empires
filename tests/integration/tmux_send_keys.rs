//! `Session::send_keys` against a real tmux pane: a short message ending in `;`
//! must reach the pane intact, since `send-keys -l` reads a trailing `;` as a
//! command separator (#1942).

use agent_of_empires::tmux::{self, Session};
use serial_test::serial;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::common::tmux_socket;

struct KillSession<'a> {
    socket: &'a std::path::Path,
    name: &'a str,
}

impl Drop for KillSession<'_> {
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
fn send_keys_keeps_a_trailing_semicolon() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let socket = tmux_socket();
    let name = format!("{}send_keys_semicolon", tmux::SESSION_PREFIX);
    let _cleanup = KillSession {
        socket: &socket,
        name: &name,
    };
    // `cat -v` echoes each submitted line back, so the pane shows what arrived.
    let status = Command::new("tmux")
        .arg("-S")
        .arg(&socket)
        .args([
            "new-session",
            "-d",
            "-s",
            &name,
            "cat -v",
            ";",
            "set-window-option",
            "-t",
            &name,
            "pane-base-index",
            "0",
        ])
        .status()
        .expect("tmux new-session");
    assert!(status.success(), "tmux new-session failed");
    tmux::refresh_session_cache();

    Session::from_name(&name)
        .send_keys("ls;")
        .expect("send_keys");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let out = Command::new("tmux")
            .arg("-S")
            .arg(&socket)
            .args(["capture-pane", "-p", "-t", &name])
            .output()
            .expect("tmux capture-pane");
        let pane = String::from_utf8_lossy(&out.stdout);
        if pane.lines().filter(|l| l.contains("ls;")).count() >= 2 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "pane never echoed `ls;`:\n{pane}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
