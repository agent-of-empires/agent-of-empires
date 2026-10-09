//! Archiving and unarchiving through the TUI and the CLI.

use std::time::{Duration, Instant};

use serde_json::Value;
use serial_test::parallel;

use crate::harness::{app_dir_in, require_tmux, write_executable, TuiTestHarness};

fn seed_sessions(h: &TuiTestHarness, project: &str, group: &str, titles: &[&str]) -> Vec<String> {
    titles
        .iter()
        .map(|title| h.add_session(&[project, "-t", title, "-g", group]))
        .collect()
}

/// The four tmux session kinds archive tears down for `session_id`.
fn tmux_session_kinds(session_id: &str, title: &str) -> Vec<String> {
    use agent_of_empires::tmux::{ContainerTerminalSession, Session, TerminalSession, ToolSession};
    vec![
        Session::generate_name(session_id, title),
        TerminalSession::generate_name(session_id, title),
        ContainerTerminalSession::generate_name(session_id, title),
        ToolSession::new(session_id, title, "lazygit")
            .session_name()
            .to_string(),
    ]
}

/// Archiving advances the cursor to the neighbour (no "parked" preview for the
/// row just dismissed), the collapsed Archived header reports the count, and
/// unarchiving returns the row to the active list, still selected and reading
/// as calmly Stopped rather than as a crashed pane.
#[test]
#[parallel]
fn test_archive_then_unarchive_cycle() {
    require_tmux!();
    let mut h = TuiTestHarness::new("archive_restore");
    // Shadow the exit-0 stub so a revived session stays Running.
    let bin = h.install_path_command("claude");
    write_executable(&bin.join("claude"), "#!/bin/sh\nexec sleep 600\n");

    let project = h.project_path();
    // Two sessions so "cursor advances to the neighbour" is meaningful.
    let _ = seed_sessions(&h, project.to_str().unwrap(), "", &["Neighbor", "Archivo"]);

    h.spawn_tui();
    h.wait_for_ready();
    h.wait_for("Archivo");
    h.wait_for("Neighbor");

    h.send_keys("z");
    h.wait_for("Archived (");
    let after_archive = h.capture_screen();
    assert!(
        !after_archive.contains("is parked"),
        "preview must follow the cursor to the next session, not the archived row\n{after_archive}"
    );

    // Down to the header, expand it, down onto the parked row.
    h.send_keys("j");
    h.send_keys("l");
    h.send_keys("j");
    h.wait_for("is parked");
    let parked = h.capture_screen();
    assert!(
        parked.contains("to unarchive"),
        "archived preview should point at z to unarchive\n{parked}"
    );

    h.send_keys("z");
    h.wait_for_absent("is parked", Duration::from_secs(5));
    // Unarchiving clears and redraws, so `wait_for_absent` can satisfy on the
    // blank frame; wait for the repaint before asserting on one capture.
    h.wait_for("Archivo");
    h.assert_screen_not_contains("Archived (");

    // Archive killed the pane, so the row is Stopped: the preview must be the
    // calm placeholder, not the red "tmux session is gone" error.
    h.wait_for("isn't running");
    let stopped = h.capture_screen();
    assert!(
        !stopped.contains("tmux session is gone"),
        "stopped preview must not show the red corpse error\n{stopped}"
    );
    assert!(
        stopped.contains("Stopped") && stopped.contains("Press Enter to start"),
        "stopped preview should explain the state and point at Enter\n{stopped}"
    );
}

/// #1868: `aoe session archive` kills all four tmux session kinds, and
/// `--no-kill` skips every one of them while still archiving the row.
#[test]
#[parallel]
fn test_cli_archive_tmux_teardown_honors_no_kill() {
    require_tmux!();
    for no_kill in [false, true] {
        let h = TuiTestHarness::new("cli_archive_teardown");
        let project = h.project_path();
        let title = "ArchiveTeardown";
        let session_id = h.add_session(&[project.to_str().unwrap(), "-t", title]);

        let names = tmux_session_kinds(&session_id, title);
        for name in &names {
            h.tmux_new_detached(name, "sleep 600");
        }

        let mut args = vec!["session", "archive", &session_id];
        if no_kill {
            args.push("--no-kill");
        }
        h.run_cli_ok(&args);

        for name in &names {
            assert_eq!(
                h.tmux_has_session(name),
                no_kill,
                "no_kill={no_kill}: tmux session '{name}' (#1868)"
            );
        }
        let sessions = h.read_sessions();
        let archived_at = sessions[0]["archived_at"].as_str();
        assert!(
            archived_at.is_some_and(|at| !at.is_empty()),
            "the row must be archived on disk either way: {archived_at:?}"
        );
    }
}

/// #2186: archiving a whole group from the TUI tears every member's tmux down
/// off-thread while the persist stays on the input thread.
#[test]
#[parallel]
fn test_tui_bulk_archive_group_tears_down_all_tmux_off_thread() {
    require_tmux!();
    let mut h = TuiTestHarness::new("tui_bulk_archive_group");
    let project = h.project_path();
    let titles = ["BulkAlpha", "BulkBeta", "BulkGamma"];
    let sessions = seed_sessions(&h, project.to_str().unwrap(), "bulkarch", &titles);

    let names: Vec<String> = sessions
        .iter()
        .zip(titles)
        .map(|(id, title)| agent_of_empires::tmux::Session::generate_name(id, title))
        .collect();
    // Pre-created under the name the instance computes, so TUI startup sees
    // them running and does not relaunch. They start the tmux server, so they
    // must go through the harness, which pins `spawn_tui`'s env onto it.
    for name in &names {
        h.tmux_new_detached(name, "sleep 600");
    }

    h.spawn_tui();
    h.wait_for_ready();
    // "name (count)" proves the group loaded with all three members.
    h.wait_for("bulkarch (3)");
    for name in &names {
        assert!(
            h.tmux_has_session(name),
            "precondition: '{name}' should be alive before archive"
        );
    }

    // `Home` rather than repeated `k`: a mixed run of printable keys arriving
    // back to back is coalesced into a paste burst (src/tui/app.rs), which
    // would swallow the `z`. `Home` is not a burst candidate.
    h.send_keys("Home");
    h.send_keys("z");
    h.wait_for("Archive all 3 sessions");
    h.send_keys("y");

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let persisted = h.read_sessions();
        let persisted = persisted.as_array().expect("sessions array");
        let all_archived = sessions.iter().all(|id| {
            persisted.iter().any(|row| {
                row["id"].as_str() == Some(id.as_str())
                    && row["archived_at"].as_str().is_some()
                    && row["lifecycle_reservation"].is_null()
            })
        });
        if all_archived && names.iter().all(|name| !h.tmux_has_session(name)) {
            break;
        }
        assert!(Instant::now() < deadline, "bulk archive must persist every original row, release its Stop and terminate its panes");
        std::thread::sleep(Duration::from_millis(100));
    }
    for name in &names {
        assert!(
            !h.tmux_has_session(name),
            "off-thread bulk-archive teardown must kill '{name}' (#2186)"
        );
    }
}

/// CLI upgrade normalizes archived transient statuses once, preserving error rows.
#[test]
#[parallel]
fn test_legacy_archived_waiting_status_migrates_once() {
    let h = TuiTestHarness::new("archive_waiting_zombie");
    let version_path = app_dir_in(h.home_path()).join(".schema_version");
    let project = h.project_path();
    let project = project.to_str().unwrap();
    let row = |id: &str, title: &str, status: &str| {
        format!(
            r#"{{"id":"{id}","title":"{title}","project_path":"{project}","group_path":"","command":"","tool":"claude","yolo_mode":false,"status":"{status}","archived_at":"2026-07-13T22:17:21Z","created_at":"2026-01-01T00:00:00Z"}}"#
        )
    };
    let frozen = format!(
        "[{},{}]",
        row("frozen0waiting01", "Frozen", "waiting"),
        row("resting0error001", "Resting", "error")
    );
    std::fs::write(h.sessions_path(), &frozen).expect("seed legacy sessions");
    std::fs::write(&version_path, "27").expect("seed actual pre-v028 schema");
    let listed: Value = serde_json::from_str(&h.run_cli_ok(&["list", "--json"]))
        .expect("persisted session listing");
    for id in ["frozen0waiting01", "resting0error001"] {
        assert!(listed.as_array().unwrap().iter().any(|row| row["id"] == id));
    }
    let mut stored = h.read_sessions();
    let rows = stored.as_array_mut().unwrap();
    let healed = rows
        .iter_mut()
        .find(|row| row["id"] == "frozen0waiting01")
        .unwrap();
    assert_eq!(healed["status"], "idle");
    assert_eq!(healed["archived_at"], "2026-07-13T22:17:21Z");
    assert_eq!(healed["runner_journal"]["coverage"], "unknown");
    healed["status"] = serde_json::json!("waiting");
    assert_eq!(
        rows.iter()
            .find(|row| row["id"] == "resting0error001")
            .unwrap()["status"],
        "error"
    );
    std::fs::write(h.sessions_path(), serde_json::to_vec(&stored).unwrap()).unwrap();
    h.run_cli_ok(&["list", "--json"]);
    assert_eq!(
        h.read_sessions(),
        stored,
        "CLI reads must not repeat the archived-status migration"
    );
}
