//! Archiving and unarchiving through the TUI and the CLI.

use std::time::{Duration, Instant};

use serde_json::Value;
use serial_test::parallel;

use crate::harness::{app_dir_in, require_tmux, write_executable, TuiTestHarness};

/// Seed `(id, title)` rows into the default profile, pointing at a real project
/// so recovery and restore can launch their agent. A non-empty `group` renders
/// one selectable group row (manual grouping is the default).
fn seed_sessions(h: &TuiTestHarness, project: &str, group: &str, rows: &[(&str, &str)]) {
    let profile_dir = app_dir_in(h.home_path()).join("profiles").join("default");
    std::fs::create_dir_all(&profile_dir).expect("create profile dir");
    let rows: Vec<String> = rows
        .iter()
        .map(|(id, title)| {
            format!(
                r#"{{"id":"{id}","title":"{title}","project_path":"{project}","group_path":"{group}","command":"","tool":"claude","yolo_mode":false,"status":"idle","created_at":"2026-01-01T00:00:00Z"}}"#,
            )
        })
        .collect();
    std::fs::write(
        profile_dir.join("sessions.json"),
        format!("[{}]", rows.join(",")),
    )
    .expect("write sessions.json");
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
    seed_sessions(
        &h,
        project.to_str().unwrap(),
        "",
        &[("arch_a", "Archivo"), ("arch_b", "Neighbor")],
    );

    h.spawn_tui();
    h.wait_for_ready();
    h.wait_for("Archivo");
    h.wait_for("Neighbor");
    // Cursor starts on the top row (Archivo); give startup recovery a beat.
    std::thread::sleep(Duration::from_millis(1200));

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
    // Ids distinct within 8 chars, so the truncated tmux names cannot collide.
    let sessions = [
        ("barch1id", "BulkAlpha"),
        ("barch2id", "BulkBeta"),
        ("barch3id", "BulkGamma"),
    ];
    seed_sessions(&h, project.to_str().unwrap(), "bulkarch", &sessions);

    let names: Vec<String> = sessions
        .iter()
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

    // Teardown is fire-and-forget, so poll for the end state.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && names.iter().any(|n| h.tmux_has_session(n)) {
        std::thread::sleep(Duration::from_millis(100));
    }
    for name in &names {
        assert!(
            !h.tmux_has_session(name),
            "off-thread bulk-archive teardown must kill '{name}' (#2186)"
        );
    }
}

/// An archived row has no pane (#1868) and the poller never revisits it
/// (#2206), so a persisted `waiting` used to stand forever and `aoe ps` kept
/// listing a pending-permission row nothing could clear. Both layers settle it:
/// the archived poll guard on read, and migration v028 once for older installs.
/// A resting `error` row is the control neither layer may touch.
#[test]
#[parallel]
fn test_archived_waiting_row_reads_idle_and_migrates_once() {
    require_tmux!();
    let h = TuiTestHarness::new("archive_waiting_zombie");
    let version_path = app_dir_in(h.home_path()).join(".schema_version");

    // First boot stamps the build's schema version, so the read below
    // exercises the in-process guard alone rather than the migration.
    h.run_cli_ok(&["ps", "--json"]);
    let stamped: u32 = std::fs::read_to_string(&version_path)
        .expect("first boot stamps .schema_version")
        .trim()
        .parse()
        .expect("schema version is a number");
    assert!(stamped >= 28, "build must carry v028, stamped {stamped}");

    // `aoe ps` joins rows to instances by the 8-char id suffix of the tmux
    // name, so the ids must be underscore-free like real session ids.
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
    std::fs::write(h.sessions_path(), &frozen).expect("write sessions.json");

    // No pane exists for either row: the guard settles the frozen Waiting on
    // read and leaves the resting Error alone.
    let rows: Value = serde_json::from_str(&h.run_cli_ok(&["ps", "--json", "--dead"]))
        .expect("aoe ps emits JSON");
    let state_of = |id: &str| {
        rows.as_array()
            .unwrap()
            .iter()
            .find(|r| r["session"] == id)
            .unwrap_or_else(|| panic!("{id} missing from aoe ps --json: {rows}"))["state"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(
        state_of("frozen0waiting01"),
        "idle",
        "an archived row must never read as a pending-permission row"
    );
    assert_eq!(state_of("resting0error001"), "dead");

    // An install predating v028 boots on the same stored rows.
    std::fs::write(h.sessions_path(), &frozen).expect("rewrite sessions.json");
    std::fs::write(&version_path, "27").expect("rewind .schema_version");
    h.run_cli_ok(&["ps", "--json"]);

    let stored = h.read_sessions();
    let stored_row = |id: &str| {
        stored
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == id)
            .unwrap_or_else(|| panic!("{id} missing from sessions.json: {stored}"))
            .clone()
    };
    let healed = stored_row("frozen0waiting01");
    assert_eq!(healed["status"], "idle", "v028 settles the stored row");
    assert_eq!(
        healed["archived_at"], "2026-07-13T22:17:21Z",
        "the archive itself survives the settle"
    );
    assert_eq!(stored_row("resting0error001")["status"], "error");
    let after: u32 = std::fs::read_to_string(&version_path)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(
        after, stamped,
        "the rewound install converges on the build's version"
    );
}
