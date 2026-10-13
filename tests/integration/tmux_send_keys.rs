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
    let home = crate::common::setup_temp_home();
    let socket = tmux_socket();
    let name = format!("{}send_keys_semicolon", tmux::SESSION_PREFIX);
    let _cleanup = KillSession {
        socket: &socket,
        name: &name,
    };
    let session = Session::from_name(&name);
    session
        .create(
            home.path().to_str().expect("temporary HOME is UTF-8"),
            Some("cat -v"),
            "default",
        )
        .expect("create managed cat session");
    let target = format!("{name}:^.0");

    session.send_keys("ls;").expect("send_keys");

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let out = Command::new("tmux")
            .arg("-S")
            .arg(&socket)
            .args(["capture-pane", "-p", "-t", &target])
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
