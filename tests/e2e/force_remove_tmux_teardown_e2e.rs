//! A quiescent wedged session leaves neither a sidebar row nor its native
//! agent tmux session after force removal.

use serial_test::parallel;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::harness::{require_tmux, TuiTestHarness};

fn tmux_has_session(sock: &std::path::Path, name: &str) -> bool {
    Command::new("tmux")
        .arg("-S")
        .arg(sock)
        .args(["has-session", "-t", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn kill_tmux(sock: &std::path::Path, name: &str) {
    let _ = Command::new("tmux")
        .arg("-S")
        .arg(sock)
        .args(["kill-session", "-t", name])
        .output();
}

/// Deleting rows stay frozen until force-remove is confirmed.
fn seed_deleting_session(h: &TuiTestHarness, id: &str, title: &str, project: &str) {
    let config_dir = crate::harness::app_dir_in(h.home_path());
    let profile_dir = config_dir.join("profiles").join("default");
    std::fs::create_dir_all(&profile_dir).expect("create profile dir");
    let mut instance = agent_of_empires::session::Instance::new(title, project);
    instance.id = id.to_owned();
    instance.tool = "claude".to_owned();
    instance.status = agent_of_empires::session::Status::Deleting;
    std::fs::write(
        profile_dir.join("sessions.json"),
        serde_json::to_vec(&[instance]).expect("serialize fixture session"),
    )
    .expect("write sessions.json");
}

/// Force-removing a quiescent wedged row also removes its native agent pane.
#[test]
#[parallel]
fn test_force_remove_session_kills_agent_tmux_session() {
    require_tmux!();

    let mut h = TuiTestHarness::new("force_remove_tmux");
    let sock = h.home_path().join("tmux.sock");
    let project = h.project_path();

    let session_id = "stuckdel-e2e-1869";
    let title = "StuckDel";
    seed_deleting_session(&h, session_id, title, project.to_str().unwrap());

    let tmux_name = format!(
        "{}{}_{}",
        agent_of_empires::tmux::SESSION_PREFIX,
        title,
        &session_id[..8]
    );

    // Seed the server with the same isolated environment as the TUI.
    h.tmux_new_detached(&tmux_name, "sleep 600");
    assert!(
        tmux_has_session(&sock, &tmux_name),
        "agent tmux session should exist before force-remove"
    );

    h.spawn_tui();
    h.wait_for(" aoe ");
    h.wait_for(title);
    h.send_keys("d");
    h.wait_for("Force Remove");

    h.send_keys("y");
    h.wait_for_absent(title, Duration::from_secs(5));

    // Observe native teardown, not just sidebar removal.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut gone = false;
    while Instant::now() < deadline {
        if !tmux_has_session(&sock, &tmux_name) {
            gone = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        gone,
        "agent tmux session '{tmux_name}' should be gone after force-remove"
    );

    kill_tmux(&sock, &tmux_name);
}
