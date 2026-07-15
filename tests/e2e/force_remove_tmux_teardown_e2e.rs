//! e2e coverage for the `force_remove_session` TUI flow (#1869, validating
//! the #1867 wiring). A session wedged in `Status::Deleting` (the background
//! deletion thread never returned) can be force-removed from the sidebar with
//! `d` -> confirm. The cardinal contract is "session removed implies tmux
//! gone": force-remove tears down the agent tmux session even though the row
//! never reached a clean deletion.
//!
//! This drives the real TUI from key press through the fire-and-forget kill
//! thread that `force_remove_session` spawns, asserting the agent tmux session
//! is reaped afterward. It complements `cli.rs::test_cli_rm_kills_agent_tmux_session`
//! (which exercises the same `Instance::kill_all_tmux_sessions` helper from the
//! CLI) at the input-dispatch level.

use serial_test::serial;
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

/// Seed a single session already stuck in `Status::Deleting`. The status
/// poller freezes Deleting rows (tier 0), so the row keeps this status on
/// startup and pressing `d` opens the force-remove confirm rather than the
/// normal delete/trash dialog.
fn seed_deleting_session(h: &TuiTestHarness, id: &str, title: &str, project: &str) {
    let config_dir = crate::harness::app_dir_in(h.home_path());
    let profile_dir = config_dir.join("profiles").join("default");
    std::fs::create_dir_all(&profile_dir).expect("create profile dir");
    let row = format!(
        r#"{{"id":"{id}","title":"{title}","project_path":"{project}","group_path":"","command":"","tool":"claude","yolo_mode":false,"status":"deleting","created_at":"2026-01-01T00:00:00Z"}}"#,
    );
    std::fs::write(profile_dir.join("sessions.json"), format!("[{row}]"))
        .expect("write sessions.json");
}

/// #1869: force-removing a wedged `Deleting` session from the TUI tears down
/// its agent tmux session. Drives the confirm dialog through the keyboard, then
/// waits for the fire-and-forget teardown thread to reap the pane.
#[test]
#[serial]
fn test_force_remove_session_kills_agent_tmux_session() {
    require_tmux!();

    let mut h = TuiTestHarness::new("force_remove_tmux");
    let sock = h.home_path().join("tmux.sock");
    let project = h.project_path();

    // id8 = "stuckdel" (first 8 chars); the agent tmux name the TUI computes
    // from this id + title must match the session we pre-create below.
    let session_id = "stuckdel-e2e-1869";
    let title = "StuckDel";
    seed_deleting_session(&h, session_id, title, project.to_str().unwrap());

    let tmux_name = format!(
        "{}{}_{}",
        agent_of_empires::tmux::SESSION_PREFIX,
        title,
        &session_id[..8]
    );

    // Stand up a real agent tmux session so there is something to reap. This
    // runs before `spawn_tui` and so starts the tmux server, which is why it
    // goes through the harness helper: the server's global environment is
    // fixed by whichever client starts it, so the helper pins the same env
    // `spawn_tui` uses (isolated HOME, harness tmux socket) onto it.
    h.tmux_new_detached(&tmux_name, "sleep 600");
    assert!(
        tmux_has_session(&sock, &tmux_name),
        "agent tmux session should exist before force-remove"
    );

    h.spawn_tui();
    h.wait_for(" aoe ");
    h.wait_for(title);
    // Let startup recovery settle; Deleting rows are frozen but give the event
    // loop a beat before driving keys.
    std::thread::sleep(Duration::from_millis(800));

    // The single seeded row is selected; `d` on a Deleting row opens the
    // force-remove confirm dialog.
    h.send_keys("d");
    h.wait_for("Force Remove");

    // The dialog defaults to "No"; `y` submits the destructive confirm and
    // dispatches `force_remove_session`, which spawns the tmux teardown thread.
    h.send_keys("y");

    // The row is removed from the sidebar synchronously.
    h.wait_for_absent(title, Duration::from_secs(5));

    // The tmux teardown is fire-and-forget on a spawned thread; poll for the
    // agent session to disappear.
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
