//! `start_with_size_opts` must return `LaunchSidOutcome::Skipped` when the
//! tmux session already exists, short-circuiting before `apply_session_flags`.

use agent_of_empires::session::{Instance, LaunchSidOutcome};
use agent_of_empires::tmux;
use serial_test::serial;
use std::process::Command;

use crate::common::{setup_temp_home, tmux_socket};

const VALID_CLAUDE_UUID: &str = "019342ab-1234-7def-8901-abcdef012345";

struct TmuxCleanup<'a>(&'a str);

impl Drop for TmuxCleanup<'_> {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .arg("-S")
            .arg(tmux_socket())
            .args(["kill-session", "-t", self.0])
            .output();
    }
}

fn tmux_available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
#[serial]
fn start_with_size_opts_returns_skipped_when_pane_preexists() {
    if !tmux_available() {
        eprintln!("skipping: tmux not on PATH");
        return;
    }
    let _temp = setup_temp_home();
    let mut inst = Instance::new("F1Regression", "/tmp/aoe-f1-regression");
    inst.tool = "claude".to_string();
    // Make is_existing=true the path acquire would take if reached, so any
    // fall-through regression builds a real launch command and fails loudly.
    inst.agent_session_id = Some(VALID_CLAUDE_UUID.to_string());
    let session_name = tmux::Session::generate_name(&inst.id, &inst.title);

    let status = Command::new("tmux")
        .arg("-S")
        .arg(tmux_socket())
        .args(["new-session", "-d", "-s", &session_name])
        .status()
        .expect("tmux new-session");
    assert!(
        status.success(),
        "tmux new-session failed for {session_name}"
    );
    let _cleanup = TmuxCleanup(&session_name);

    // `Session::exists()` consults a 2s-TTL cache; refresh after the raw
    // `new-session` so a prior `#[serial]` test's stale snapshot can't
    // make `exists()` miss our session.
    tmux::refresh_session_cache();

    let outcome = inst
        .start_with_size_opts(None, false)
        .expect("start_with_size_opts must succeed on preexisting pane");

    assert_eq!(
        outcome,
        LaunchSidOutcome::Skipped,
        "preexisting pane must short-circuit before apply_session_flags"
    );
}
