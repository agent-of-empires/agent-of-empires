use serial_test::parallel;
use std::process::Command;
use std::time::Duration;

use crate::harness::{require_tmux, TuiTestHarness};

/// Read sessions.json from the harness's isolated home.
fn read_sessions_json(h: &TuiTestHarness) -> serde_json::Value {
    let sessions_path =
        crate::harness::app_dir_in(h.home_path()).join("profiles/default/sessions.json");
    let content = std::fs::read_to_string(&sessions_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", sessions_path.display(), e));
    serde_json::from_str(&content).expect("invalid sessions JSON")
}

/// Best-effort cleanup so failures don't leak into the next `#[parallel]` test.
fn kill_tmux(sock: &std::path::Path, name: &str) {
    let _ = Command::new("tmux")
        .arg("-S")
        .arg(sock)
        .args(["kill-session", "-t", name])
        .output();
}

fn mark_tool_owned(h: &TuiTestHarness, name: &str, session_id: &str, tool_name: &str) {
    let target = format!("={name}:");
    let owner = serde_json::to_string(&(session_id, tool_name)).unwrap();
    let mark = h
        .tmux()
        .args([
            "set-option",
            "-t",
            &target,
            "@aoe_tool_owner",
            &owner,
            ";",
            "set-option",
            "-t",
            &target,
            "@aoe_kind",
            "tool",
        ])
        .output()
        .expect("mark archive tool fixture ownership");
    assert!(
        mark.status.success(),
        "mark archive tool fixture failed: {}",
        String::from_utf8_lossy(&mark.stderr)
    );
}

fn tmux_has_session(sock: &std::path::Path, name: &str) -> bool {
    Command::new("tmux")
        .arg("-S")
        .arg(sock)
        .args(["has-session", "-t", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Seed sessions in the default profile under a single manual group, pointing
/// at a real project dir. Manual mode (the default) renders a group header from
/// each session's `group_path`, so the TUI shows one selectable group row.
fn seed_group_sessions(h: &TuiTestHarness, project: &str, group: &str, sessions: &[(&str, &str)]) {
    let config_dir = crate::harness::app_dir_in(h.home_path());
    let profile_dir = config_dir.join("profiles").join("default");
    std::fs::create_dir_all(&profile_dir).expect("create profile dir");
    let rows: Vec<String> = sessions
        .iter()
        .map(|(id, title)| {
            format!(
                r#"{{"id":"{id}","title":"{title}","project_path":"{project}","group_path":"{group}","command":"","tool":"claude","yolo_mode":false,"status":"idle","terminal_scopes":[],"created_at":"2026-01-01T00:00:00Z"}}"#,
            )
        })
        .collect();
    std::fs::write(
        profile_dir.join("sessions.json"),
        format!("[{}]", rows.join(",")),
    )
    .expect("write sessions.json");
}

/// Seed sessions in the default profile pointing at a real project dir, so
/// startup recovery / restore can actually launch their (persistent) agent.
fn seed_sessions(h: &TuiTestHarness, project: &str, titles: &[(&str, &str)]) {
    let config_dir = crate::harness::app_dir_in(h.home_path());
    let profile_dir = config_dir.join("profiles").join("default");
    std::fs::create_dir_all(&profile_dir).expect("create profile dir");
    let rows: Vec<String> = titles
        .iter()
        .map(|(id, title)| {
            format!(
                r#"{{"id":"{id}","title":"{title}","project_path":"{project}","group_path":"","command":"","tool":"claude","yolo_mode":false,"status":"idle","terminal_scopes":[],"created_at":"2026-01-01T00:00:00Z"}}"#,
            )
        })
        .collect();
    std::fs::write(
        profile_dir.join("sessions.json"),
        format!("[{}]", rows.join(",")),
    )
    .expect("write sessions.json");
}

/// Install a persistent `claude` (shadows the exit-0 stub) so a revived session
/// stays Running instead of dying immediately.
fn install_persistent_claude(h: &mut TuiTestHarness) {
    let bin = h.install_path_command("claude");
    let claude = bin.join("claude");
    std::fs::write(&claude, "#!/bin/sh\nexec sleep 600\n").expect("write persistent claude");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755))
            .expect("chmod claude");
    }
}

/// Drive a full archive -> unarchive cycle through the real TUI.
///
/// Verifies the user-visible contract end to end: archiving advances the
/// cursor to the next active session (the preview follows it; no "parked"
/// placeholder for a row the user just dismissed) while the collapsed
/// Archived section header appears with the count as feedback; navigating
/// into the section and unarchiving brings the row back to the active list
/// and keeps it selected.
#[test]
#[parallel]
fn test_archive_then_unarchive_cycle() {
    require_tmux!();

    let mut h = TuiTestHarness::new("archive_restore");
    install_persistent_claude(&mut h);

    let project = h.project_path();
    // Two sessions so "cursor advances to the neighbour" is meaningful.
    seed_sessions(
        &h,
        project.to_str().unwrap(),
        &[("arch_a", "Archivo"), ("arch_b", "Neighbor")],
    );

    h.spawn_tui();
    // The native pane preparation refuses an unhealthy runtime, so wait for the
    // subscription to report readiness before driving panes.
    h.wait_for_runtime_ready();
    h.wait_for_ready();
    h.wait_for("Archivo");
    h.wait_for("Neighbor");

    // Archive the selected session.
    h.send_keys("z");
    h.wait_for("Archived (");
    let after_archive = h.capture_screen();

    // The selection advanced to Neighbor, so the preview must NOT render the
    // archived "parked" placeholder; the collapsed Archived section header
    // (with its count) is the only trace of the dismissed row.
    assert!(
        !after_archive.contains("is parked"),
        "preview must follow the cursor to the next session, not the archived row\n{after_archive}"
    );
    assert!(
        after_archive.contains("Archived ("),
        "the Archived section header should appear with the count\n{after_archive}"
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

    h.wait_for_runtime_ready();
    h.send_keys("z");
    h.wait_for_absent("is parked", Duration::from_secs(5));
    // The unarchive is a daemon mutation now, so the row rises when the applied
    // snapshot carries the cleared stamp: wait for the empty section to go with
    // it before asserting on a single frame.
    h.wait_for_absent("Archived (", Duration::from_secs(5));
    // The unarchive triggers a full clear+redraw. `wait_for_absent` above can
    // satisfy on the transient blank frame mid-redraw, so a bare capture here
    // races the repaint and sometimes catches an empty screen (the same blank
    // capture `assert_screen_contains` retries around). Poll for the row to
    // actually repaint into the active list before asserting on a single frame.
    h.wait_for("Archivo");
    let after_unarchive = h.capture_screen();
    assert!(
        after_unarchive.contains("Archivo"),
        "unarchived row should be back in the active list\n{after_unarchive}"
    );
    assert!(
        !after_unarchive.contains("Archived ("),
        "the Archived section should be gone once empty\n{after_unarchive}"
    );

    // The unarchived row is Stopped (archive killed its pane). Once the poller
    // stamps the gone-error, the preview must show the calm Stopped placeholder,
    // not the red "tmux session is gone" crash error.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !h.capture_screen().contains("isn't running") && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(150));
    }
    let stopped = h.capture_screen();
    assert!(
        stopped.contains("isn't running"),
        "the unarchived row should show the calm stopped preview\n{stopped}"
    );
    assert!(
        !stopped.contains("tmux session is gone"),
        "stopped preview must not show the red corpse error\n{stopped}"
    );
    assert!(
        stopped.contains("Stopped") && stopped.contains("Press Enter to start"),
        "stopped preview should explain the state and point at Enter\n{stopped}"
    );
}

/// Locks #1868: archive kills all four tmux session kinds. Pre-creates real
/// sessions, runs the CLI, asserts each kind is gone.
#[test]
#[parallel]
fn test_cli_archive_kills_agent_and_terminal_tmux_sessions() {
    require_tmux!();

    let h = TuiTestHarness::new("cli_archive_full_teardown");
    let project = h.project_path();

    let add_output = h.run_cli(&["add", project.to_str().unwrap(), "-t", "ArchiveTeardown"]);
    assert!(
        add_output.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );

    let sessions = read_sessions_json(&h);
    let session_id = sessions[0]["id"]
        .as_str()
        .expect("session should have id")
        .to_string();
    let truncated_id = session_id[..8.min(session_id.len())].to_string();

    let agent_tmux_name = format!(
        "{}ArchiveTeardown_{}",
        agent_of_empires::tmux::SESSION_PREFIX,
        truncated_id
    );
    let terminal_tmux_name =
        agent_of_empires::tmux::TerminalSession::generate_name(&session_id, "ArchiveTeardown");
    let cterm_tmux_name = agent_of_empires::tmux::ContainerTerminalSession::generate_name(
        &session_id,
        "ArchiveTeardown",
    );
    let tool_tmux_name =
        agent_of_empires::tmux::ToolSession::new(&session_id, "ArchiveTeardown", "lazygit")
            .session_name()
            .to_string();

    // Pre-create the four tmux kinds so archive can find and kill them. Created
    // on the harness's tmux socket (`AOE_TMUX_SOCKET`), the same one the CLI
    // resolves (#2608), so archive's teardown sweeps them.
    let sock = h.home_path().join("tmux.sock");
    let names = [
        &agent_tmux_name,
        &terminal_tmux_name,
        &cterm_tmux_name,
        &tool_tmux_name,
    ];
    for name in names {
        let target = format!("={name}:");
        let create = Command::new("tmux")
            .arg("-S")
            .arg(&sock)
            .args([
                "new-session",
                "-d",
                "-s",
                name,
                "-x",
                "80",
                "-y",
                "24",
                "sleep",
                "600",
                ";",
                "set-option",
                "-t",
                &target,
                "pane-base-index",
                "0",
            ])
            .output()
            .expect("tmux new-session");
        assert!(
            create.status.success(),
            "failed to create tmux session {}: {}",
            name,
            String::from_utf8_lossy(&create.stderr)
        );
    }

    mark_tool_owned(&h, &tool_tmux_name, &session_id, "lazygit");

    let archive_output = h.run_cli(&["session", "archive", &session_id]);
    assert!(
        archive_output.status.success(),
        "aoe session archive failed: {}",
        String::from_utf8_lossy(&archive_output.stderr)
    );

    let alive: Vec<(&&String, bool)> = names
        .iter()
        .map(|n| (n, tmux_has_session(&sock, n)))
        .collect();

    // Cleanup BEFORE asserting so a single failure cannot leak survivors
    // into the next serial test.
    for (name, is_alive) in &alive {
        if *is_alive {
            kill_tmux(&sock, name);
        }
    }

    for (name, is_alive) in &alive {
        assert!(
            !is_alive,
            "tmux session '{}' must be killed by archive (#1868)",
            name
        );
    }
}

/// Locks the widened `--no-kill` semantic from #1868: skip ALL tmux
/// teardown. The agent assertion is the pre/post differentiator.
#[test]
#[parallel]
fn test_cli_archive_no_kill_preserves_all_tmux_sessions() {
    require_tmux!();

    let h = TuiTestHarness::new("cli_archive_no_kill");
    let project = h.project_path();

    let add_output = h.run_cli(&["add", project.to_str().unwrap(), "-t", "ArchiveNoKill"]);
    assert!(
        add_output.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add_output.stderr)
    );

    let sessions = read_sessions_json(&h);
    let session_id = sessions[0]["id"]
        .as_str()
        .expect("session should have id")
        .to_string();
    let truncated_id = session_id[..8.min(session_id.len())].to_string();

    let agent_tmux_name = format!(
        "{}ArchiveNoKill_{}",
        agent_of_empires::tmux::SESSION_PREFIX,
        truncated_id
    );
    let terminal_tmux_name =
        agent_of_empires::tmux::TerminalSession::generate_name(&session_id, "ArchiveNoKill");
    let cterm_tmux_name = agent_of_empires::tmux::ContainerTerminalSession::generate_name(
        &session_id,
        "ArchiveNoKill",
    );
    let tool_tmux_name =
        agent_of_empires::tmux::ToolSession::new(&session_id, "ArchiveNoKill", "lazygit")
            .session_name()
            .to_string();

    let sock = h.home_path().join("tmux.sock");
    let names = [
        &agent_tmux_name,
        &terminal_tmux_name,
        &cterm_tmux_name,
        &tool_tmux_name,
    ];
    for name in names {
        let target = format!("={name}:");
        let create = Command::new("tmux")
            .arg("-S")
            .arg(&sock)
            .args([
                "new-session",
                "-d",
                "-s",
                name,
                "-x",
                "80",
                "-y",
                "24",
                "sleep",
                "600",
                ";",
                "set-option",
                "-t",
                &target,
                "pane-base-index",
                "0",
            ])
            .output()
            .expect("tmux new-session");
        assert!(
            create.status.success(),
            "failed to create tmux session {}: {}",
            name,
            String::from_utf8_lossy(&create.stderr)
        );
    }

    mark_tool_owned(&h, &tool_tmux_name, &session_id, "lazygit");

    let archive_output = h.run_cli(&["session", "archive", &session_id, "--no-kill"]);
    assert!(
        archive_output.status.success(),
        "aoe session archive --no-kill failed: {}",
        String::from_utf8_lossy(&archive_output.stderr)
    );

    let alive: Vec<(&&String, bool)> = names
        .iter()
        .map(|n| (n, tmux_has_session(&sock, n)))
        .collect();

    // Cleanup explicitly: --no-kill leaves the survivors so they would
    // pollute the next serial test.
    for name in &names {
        kill_tmux(&sock, name);
    }

    for (name, is_alive) in &alive {
        assert!(
            *is_alive,
            "tmux session '{}' must survive --no-kill archive (#1868)",
            name
        );
    }

    let post_archive = read_sessions_json(&h);
    let archived_at = post_archive[0]["archived_at"].as_str();
    assert!(
        archived_at.is_some() && !archived_at.unwrap().is_empty(),
        "session must still be archived on disk even with --no-kill: archived_at = {:?}",
        archived_at
    );
}

/// Locks the #2179 widened TUI bulk-archive teardown (#2186). Archiving a whole
/// group runs the tmux teardown off-thread (`std::thread::spawn` +
/// `catch_unwind`, mirroring `force_remove_session`) while the persist stays on
/// the input thread. The off-thread half was structurally correct but had no
/// deterministic assertion: unit tests only proved the persist completes
/// synchronously, and a wall-clock "returned within Nms" check is flaky.
///
/// This drives the real TUI group-archive confirm flow against pre-created real
/// tmux sessions, then polls for the end state (every member's tmux session
/// gone) rather than asserting a timing deadline, so it stays deterministic on
/// slow CI. It verifies all N rows complete teardown off-thread, closing the
/// gap called out in #2186.
#[test]
#[parallel]
fn test_tui_bulk_archive_group_tears_down_all_tmux_off_thread() {
    require_tmux!();

    let mut h = TuiTestHarness::new("tui_bulk_archive_group");
    let project = h.project_path();

    let group = "bulkarch";
    // Distinct within the first 8 chars so the truncated-id tmux names don't
    // collide.
    let sessions = [
        ("barch1id", "BulkAlpha"),
        ("barch2id", "BulkBeta"),
        ("barch3id", "BulkGamma"),
    ];
    seed_group_sessions(&h, project.to_str().unwrap(), group, &sessions);

    // The exact agent tmux name the archive teardown targets for each member.
    let sock = h.home_path().join("tmux.sock");
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
    // The native pane preparation refuses an unhealthy runtime, so wait for the
    // subscription to report readiness before driving panes.
    h.wait_for_runtime_ready();
    h.wait_for_ready();
    // "name (count)" proves the group loaded with all three members.
    h.wait_for("bulkarch (3)");

    // Pre-condition: every agent session is alive before we archive, so a later
    // "all gone" cannot pass trivially.
    for name in &names {
        assert!(
            tmux_has_session(&sock, name),
            "precondition: tmux session '{}' should be alive before archive",
            name
        );
    }

    // `Home` rather than repeated `k`: a mixed run of printable keys arriving
    // back to back is coalesced into a paste burst (src/tui/app.rs), which
    // would swallow the `z`. `Home` is not a burst candidate.
    h.send_keys("Home");
    h.send_keys("z");
    h.wait_for("Archive all 3 sessions");
    h.send_keys("y");

    // The teardown is fire-and-forget on a spawned thread, so poll for the end
    // state rather than asserting a timing deadline. Generous 10s ceiling.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if names.iter().all(|n| !tmux_has_session(&sock, n)) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    let alive: Vec<(&String, bool)> = names
        .iter()
        .map(|n| (n, tmux_has_session(&sock, n)))
        .collect();

    // Clean up any survivors BEFORE asserting so a single failure cannot leak
    // into the next serial test.
    for (name, is_alive) in &alive {
        if *is_alive {
            kill_tmux(&sock, name);
        }
    }

    for (name, is_alive) in &alive {
        assert!(
            !is_alive,
            "off-thread bulk-archive teardown must kill group member tmux session '{}' (#2186)",
            name
        );
    }
}

/// A row archived while `Waiting` has no pane behind it (archive tears the
/// tmux down, #1868) and the poller never revisits archived rows (#2206), so
/// the persisted status used to stand forever: `aoe ps` kept listing a
/// pending-permission row that nothing could clear. Seeds that persisted
/// shape straight onto disk and checks both layers on the real binary: the
/// archived poll guard settles the row on read, and migration v028 settles
/// the stored row once for installs that predate the fix. A resting status
/// (`error`) on an archived row is the control: neither layer touches it.
#[test]
#[parallel]
fn test_archived_waiting_row_reads_idle_and_migrates_once() {
    require_tmux!();

    let h = TuiTestHarness::new("archive_waiting_zombie");
    let app_dir = crate::harness::app_dir_in(h.home_path());
    let profile_dir = app_dir.join("profiles").join("default");
    std::fs::create_dir_all(&profile_dir).expect("create profile dir");
    let sessions_path = profile_dir.join("sessions.json");
    let version_path = app_dir.join(".schema_version");

    // First boot stamps the build's schema version, so the read below
    // exercises the in-process guard alone rather than the migration.
    let boot = h.run_cli(&["ps", "--json"]);
    assert!(
        boot.status.success(),
        "aoe ps failed: {}",
        String::from_utf8_lossy(&boot.stderr)
    );
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
    std::fs::write(&sessions_path, &frozen).expect("write sessions.json");

    // No tmux pane exists for either row. The archived guard must settle the
    // frozen Waiting to idle on read and leave the resting Error alone.
    let ps = h.run_cli(&["ps", "--json", "--dead"]);
    assert!(
        ps.status.success(),
        "aoe ps failed: {}",
        String::from_utf8_lossy(&ps.stderr)
    );
    let rows: serde_json::Value =
        serde_json::from_slice(&ps.stdout).expect("aoe ps --json emits JSON");
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

    // An install that predates v028 boots on the same stored rows: the
    // migration settles the frozen row once and leaves the control alone.
    std::fs::write(&sessions_path, &frozen).expect("rewrite sessions.json");
    std::fs::write(&version_path, "27").expect("rewind .schema_version");
    let migrated = h.run_cli(&["ps", "--json"]);
    assert!(
        migrated.status.success(),
        "aoe ps failed: {}",
        String::from_utf8_lossy(&migrated.stderr)
    );
    let stored = read_sessions_json(&h);
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

#[test]
#[parallel]
fn trash_retained_transcript_is_read_only() {
    use crate::harness::{app_dir_in, wait_until};
    use agent_of_empires::{
        acp::{
            event_store::EventStore,
            state::{Event, ToolCall},
        },
        session::{Instance, Status},
    };
    require_tmux!();
    let mut h = TuiTestHarness::new("trash_retained");
    h.stop_daemon_on_drop();
    h.append_config("[session]\nlive_send_on_view_switch = true\nshow_diagnostics_pane = true");
    let project = h.project_path();
    let mut retained = Instance::new("TrashRetained", project.to_str().unwrap());
    retained.trashed_at = Some(chrono::Utc::now());
    retained.status = Status::Stopped;
    retained.mark_unread();
    let id = retained.id.clone();
    let mut empty = Instance::new("TrashEmpty", project.to_str().unwrap());
    empty.trashed_at = Some(chrono::Utc::now());
    empty.archived_at = empty.trashed_at;
    empty.status = Status::Stopped;
    let empty_id = empty.id.clone();
    std::fs::write(
        h.sessions_path(),
        serde_json::to_vec(&[retained, empty]).unwrap(),
    )
    .unwrap();
    let app_dir = app_dir_in(h.home_path());
    let store = EventStore::open(&app_dir.join("acp_events.db"), 0).unwrap();
    store
        .record(
            &id,
            1,
            &Event::AvailableCommandsUpdated {
                commands: Vec::new(),
            },
        )
        .unwrap();
    store
        .record(
            &id,
            8,
            &Event::ToolCallStarted {
                tool_call: ToolCall {
                    id: "unfinished-tool".into(),
                    name: "RetainedPendingTool".into(),
                    kind: "execute".into(),
                    args_preview: "{}".into(),
                    started_at: chrono::Utc::now(),
                    parent_tool_call_id: None,
                    memory_recall: None,
                    diffs: Vec::new(),
                },
            },
        )
        .unwrap();
    for seq in 10..=1014 {
        let event = if seq == 1008 {
            Event::ToolCallContent {
                tool_call_id: "retained-tool".into(),
                content: (0..40)
                    .map(|i| format!("success-output-{i:02}\n"))
                    .collect::<String>()
                    + "SUCCESS-OUTPUT-END",
            }
        } else {
            Event::AgentMessageChunk {
                text: format!("retained-{seq:04}\n\n"),
                message_id: None,
            }
        };
        store.record(&id, seq, &event).unwrap();
    }
    store
        .record(
            &id,
            1015,
            &Event::ToolCallCompleted {
                tool_call_id: "retained-tool".into(),
                is_error: false,
                content: String::new(),
                output: Vec::new(),
                completed_at: chrono::Utc::now(),
                async_subagent: false,
            },
        )
        .unwrap();
    store
        .record(
            &id,
            1016,
            &Event::ToolCallContent {
                tool_call_id: "failed-tool".into(),
                content: (0..40)
                    .map(|i| format!("error-output-{i:02}\n"))
                    .collect::<String>()
                    + "ERROR-OUTPUT-END",
            },
        )
        .unwrap();
    store
        .record(
            &id,
            1017,
            &Event::ToolCallCompleted {
                tool_call_id: "failed-tool".into(),
                is_error: true,
                content: String::new(),
                output: Vec::new(),
                completed_at: chrono::Utc::now(),
                async_subagent: false,
            },
        )
        .unwrap();
    for (seq, content) in [
        (1018, "superseded unfinished output".to_string()),
        (1019, "a".repeat(62) + "♥️x\nUNFINISHED-OUTPUT-END"),
    ] {
        store
            .record(
                &id,
                seq,
                &Event::ToolCallContent {
                    tool_call_id: "unfinished-tool".into(),
                    content,
                },
            )
            .unwrap();
    }
    store
        .record(
            &id,
            1020,
            &Event::ToolCallContent {
                tool_call_id: "media-tool".into(),
                content: "MEDIA-RETAINED-TEXT".into(),
            },
        )
        .unwrap();
    store
        .record(
            &id,
            1021,
            &Event::ToolCallCompleted {
                tool_call_id: "media-tool".into(),
                is_error: false,
                content: String::new(),
                output: vec![agent_of_empires::acp::state::ToolOutputBlock::Image {
                    mime_type: "image/png".into(),
                    data: None,
                    uri: None,
                }],
                completed_at: chrono::Utc::now(),
                async_subagent: false,
            },
        )
        .unwrap();
    for seq in [1, 3] {
        store
            .record(
                &empty_id,
                seq,
                &Event::AvailableCommandsUpdated {
                    commands: Vec::new(),
                },
            )
            .unwrap();
    }
    drop(store);
    let launch_marker = h.home_path().join("agent-launched");
    let bin = h.install_path_command("claude");
    std::fs::write(
        bin.join("claude"),
        format!(
            "#!/bin/sh\nprintf started >> {}\n",
            shell_words::quote(launch_marker.to_str().unwrap())
        ),
    )
    .unwrap();
    let clipboard = h.home_path().join("copied");
    for command in ["wl-copy", "xclip", "xsel", "pbcopy"] {
        let bin = h.install_path_command(command);
        std::fs::write(
            bin.join(command),
            format!(
                "#!/bin/sh\ncat > {}\n",
                shell_words::quote(clipboard.to_str().unwrap())
            ),
        )
        .unwrap();
    }
    h.start_daemon();
    h.spawn_tui();
    h.wait_for_runtime_ready();
    h.wait_for("Trash (");
    h.send_keys("End");
    h.send_keys("Right");
    h.wait_for("TrashRetained");
    let screen = h.capture_screen();
    let row = screen
        .lines()
        .position(|line| line.contains("TrashRetained"))
        .unwrap() as u16
        + 1;
    h.send_mouse_click(0, 7, row);
    h.wait_for("UNFINISHED-OUTPUT-END");
    h.send_keys("S-PageUp");
    h.wait_for_absent("UNFINISHED-OUTPUT-END", Duration::from_secs(5));
    h.assert_screen_contains("Trash · read-only");
    h.send_keys("S-PageDown");
    h.wait_for("UNFINISHED-OUTPUT-END");
    let screen = h.capture_screen();
    let (y, line) = screen
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("Earlier"))
        .unwrap();
    let x =
        unicode_width::UnicodeWidthStr::width(&line[..line.find("Earlier").unwrap()]) as u16 + 1;
    h.send_mouse_click(0, x, y as u16 + 1);
    h.wait_for_absent("UNFINISHED-OUTPUT-END", Duration::from_secs(5));
    h.assert_screen_contains("Trash · read-only");
    h.send_keys("S-PageDown");
    h.wait_for("UNFINISHED-OUTPUT-END");
    h.send_keys("Enter");
    h.wait_for("Esc: sessions");
    let screen = h.capture_screen();
    let row = screen
        .lines()
        .position(|line| line.contains("CPU ") && line.contains("Mem "))
        .unwrap() as u16
        + 1;
    h.send_mouse_click(0, 5, row);
    h.wait_for("System Health");
    h.send_keys("Enter");
    h.wait_for("UNFINISHED-OUTPUT-END");
    h.wait_for("ERROR-OUTPUT-END");
    h.assert_screen_contains("history is incomplete");
    h.assert_screen_contains("Trash · read-only");
    h.wait_for("UNFINISHED-OUTPUT-END");
    h.wait_for("♥️x");
    h.assert_screen_contains("completion unavailable");
    h.assert_screen_not_contains("superseded unfinished output");
    for marker in ["♥️x", "MEDIA-RETAINED-TEXT"] {
        h.wait_for(marker);
        let screen = h.capture_screen();
        let (y, line) = screen
            .lines()
            .enumerate()
            .find(|(_, line)| line.contains(marker))
            .unwrap();
        let x =
            unicode_width::UnicodeWidthStr::width(&line[..line.find(marker).unwrap()]) as u16 + 1;
        let width = unicode_width::UnicodeWidthStr::width(marker) as u16;
        h.send_mouse_drag((x, y as u16 + 1), (x + width, y as u16 + 1));
        wait_until(Duration::from_secs(5), Duration::from_millis(10), || {
            let copied = std::fs::read_to_string(&clipboard).unwrap_or_default();
            (copied == marker)
                .then_some(())
                .ok_or_else(|| format!("expected {marker:?} in clipboard, got {copied:?}"))
        });
    }
    if std::env::var_os("RECORD_E2E").is_some() {
        let directory = std::path::Path::new("target/e2e-recordings");
        std::fs::create_dir_all(directory).unwrap();
        std::fs::write(
            directory.join("trash-unfinished.ansi"),
            h.capture_screen_styled(),
        )
        .unwrap();
        std::fs::write(directory.join("trash-unfinished.txt"), h.capture_screen()).unwrap();
    }
    let before = h.read_sessions();
    let events = EventStore::open(&app_dir.join("acp_events.db"), 0).unwrap();
    events
        .record(
            &id,
            1022,
            &Event::AgentMessageChunk {
                text: "LATE-RETAINED-OUTPUT\n\n".into(),
                message_id: Some("late".into()),
            },
        )
        .unwrap();
    drop(events);
    h.wait_for("LATE-RETAINED-OUTPUT");
    h.assert_screen_contains("Esc: sessions");
    let screen = h.capture_screen();
    let (y, line) = screen
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("LATE-RETAINED-OUTPUT"))
        .unwrap();
    let x =
        unicode_width::UnicodeWidthStr::width(&line[..line.find("LATE-RETAINED-OUTPUT").unwrap()])
            as u16
            + 1;
    h.send_mouse_drag((x, y as u16 + 1), (x + 19, y as u16 + 1));
    wait_until(Duration::from_secs(5), Duration::from_millis(10), || {
        let copied = std::fs::read_to_string(&clipboard).unwrap_or_default();
        (copied == "LATE-RETAINED-OUTPUT")
            .then_some(())
            .ok_or_else(|| format!("expected fresh retained output, got {copied:?}"))
    });
    if std::env::var_os("RECORD_E2E").is_some() {
        let directory = std::path::Path::new("target/e2e-recordings");
        std::fs::write(
            directory.join("trash-late-output.ansi"),
            h.capture_screen_styled(),
        )
        .unwrap();
        std::fs::write(directory.join("trash-late-output.txt"), h.capture_screen()).unwrap();
    }
    let events = EventStore::open(&app_dir.join("acp_events.db"), 0).unwrap();
    events
        .record(
            &id,
            1024,
            &Event::ConversationSummary {
                text: "CONCURRENT-TEARDOWN-SUMMARY".into(),
                summarized_until_seq: 1022,
            },
        )
        .unwrap();
    h.wait_for("CONCURRENT-TEARDOWN-SUMMARY");
    h.assert_screen_not_contains("◼");
    events
        .record(
            &id,
            1023,
            &Event::Stopped {
                reason: "user_stopped".into(),
            },
        )
        .unwrap();
    drop(events);
    h.wait_for_absent("completion unavailable", Duration::from_secs(45));
    h.assert_screen_contains("CONCURRENT-TEARDOWN-SUMMARY");
    h.assert_screen_contains("Esc: sessions");
    h.send_keys("Home");
    h.wait_for("◼");
    let marker = "UNFINISHED-OUTPUT-END";
    let screen = h.capture_screen();
    let (y, line) = screen
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains(marker))
        .unwrap();
    let x = unicode_width::UnicodeWidthStr::width(&line[..line.find(marker).unwrap()]) as u16 + 1;
    h.send_mouse_drag(
        (x, y as u16 + 1),
        (x + marker.len() as u16 - 1, y as u16 + 1),
    );
    wait_until(Duration::from_secs(5), Duration::from_millis(10), || {
        let copied = std::fs::read_to_string(&clipboard).unwrap_or_default();
        (copied == marker)
            .then_some(())
            .ok_or_else(|| format!("expected stopped retained output, got {copied:?}"))
    });
    if std::env::var_os("RECORD_E2E").is_some() {
        let directory = std::path::Path::new("target/e2e-recordings");
        std::fs::write(
            directory.join("trash-late-stop.ansi"),
            h.capture_screen_styled(),
        )
        .unwrap();
        std::fs::write(directory.join("trash-late-stop.txt"), h.capture_screen()).unwrap();
    }
    h.send_keys("End");
    h.wait_for("CONCURRENT-TEARDOWN-SUMMARY");
    for _ in 0..10 {
        let previous = h.capture_screen();
        if previous.contains("SUCCESS-OUTPUT-END") {
            break;
        }
        h.send_keys("PageUp");
        wait_until(Duration::from_secs(5), Duration::from_millis(10), || {
            (h.capture_screen() != previous)
                .then_some(())
                .ok_or_else(|| "waiting for retained scroll".to_string())
        });
    }
    h.wait_for("SUCCESS-OUTPUT-END");
    let screen = wait_until(Duration::from_secs(5), Duration::from_millis(10), || {
        let screen = h.capture_screen();
        let middle = screen.lines().count() / 2;
        let row = screen
            .lines()
            .position(|line| line.contains("SUCCESS-OUTPUT-END"))
            .ok_or_else(|| "waiting for retained tool tail".to_string())?;
        if row.abs_diff(middle) <= 1 {
            return Ok(screen);
        }
        h.send_keys(if row < middle { "Up" } else { "Down" });
        Err("moving retained tool tail away from autoscroll edges".to_string())
    });
    let (y, line) = screen
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("SUCCESS-OUTPUT-END"))
        .unwrap();
    let byte = line.find("SUCCESS-OUTPUT-END").unwrap();
    let x = unicode_width::UnicodeWidthStr::width(&line[..byte]) as u16 + 1;
    h.send_mouse_drag((x, y as u16 + 1), (x + 17, y as u16 + 1));
    wait_until(Duration::from_secs(5), Duration::from_millis(10), || {
        let copied = std::fs::read_to_string(&clipboard).unwrap_or_default();
        (copied == "SUCCESS-OUTPUT-END")
            .then_some(())
            .ok_or_else(|| format!("expected full retained tool tail, got {copied:?}"))
    });
    h.send_keys("C-q");
    h.wait_for("Enter: read");
    h.send_keys("Enter");
    h.send_keys("q");
    h.send_keys("Home");
    h.wait_for("retained-0010");
    h.type_text("must_not_send");
    h.send_paste("must_not_send");
    h.send_keys("Tab");
    h.assert_screen_not_contains("Message the agent");
    h.assert_screen_not_contains("Live send failed");
    let top = h.capture_screen();
    let (y, line) = top
        .lines()
        .enumerate()
        .find(|(_, line)| line.contains("retained-0010"))
        .unwrap();
    let byte = line.find("retained-0010").unwrap();
    let x = unicode_width::UnicodeWidthStr::width(&line[..byte]) as u16 + 1;
    h.send_mouse_drag((x, y as u16 + 1), (x + 12, y as u16 + 1));
    wait_until(Duration::from_secs(5), Duration::from_millis(10), || {
        let copied = std::fs::read_to_string(&clipboard).unwrap_or_default();
        (copied == "retained-0010")
            .then_some(())
            .ok_or_else(|| format!("expected retained text in isolated clipboard, got {copied:?}"))
    });
    if std::env::var_os("RECORD_E2E").is_some() {
        let directory = std::path::Path::new("target/e2e-recordings");
        std::fs::create_dir_all(directory).unwrap();
        std::fs::write(
            directory.join("trash-readonly.ansi"),
            h.capture_screen_styled(),
        )
        .unwrap();
        std::fs::write(directory.join("trash-readonly.txt"), h.capture_screen()).unwrap();
    }
    h.send_keys("PageDown");
    h.wait_for_absent("retained-0010", Duration::from_secs(5));
    h.send_keys("Escape");
    for key in ["Tab", "t", "m", "r"] {
        h.send_keys(key);
    }
    h.assert_screen_contains("Trash · read-only");
    h.assert_screen_not_contains("Reviving");
    h.assert_screen_not_contains("LIVE");
    let after = h.read_sessions();
    assert_eq!(
        before, after,
        "inspection must not mutate retained sessions"
    );
    assert!(!launch_marker.exists(), "inspection launched an agent");
    let workers = h.run_cli_ok(&["ps", "--acp", "--dead", "--json"]);
    let workers: serde_json::Value = serde_json::from_str(&workers).unwrap();
    assert_eq!(workers, serde_json::json!([]));
    let sessions = h
        .tmux()
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
        .unwrap();
    let sessions = String::from_utf8_lossy(&sessions.stdout);
    assert!(
        !sessions.lines().any(|name| name.starts_with("aoe_dev_")),
        "{sessions}"
    );
    let screen = h.capture_screen();
    let row = screen
        .lines()
        .position(|line| line.contains("TrashEmpty"))
        .unwrap() as u16
        + 1;
    h.send_mouse_click(0, 7, row);
    h.wait_for("No readable retained ACP content");
    h.assert_screen_contains("Trash · read-only");
    h.assert_screen_contains("history is incomplete");
    h.send_keys("d");
    h.wait_for("Yes");
    h.run_cli_ok(&["session", "restore", &empty_id]);
    h.wait_for("Session Changed");
    h.send_keys("y");
    let restored = h.read_sessions();
    let restored = restored
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["id"] == empty_id)
        .expect("peer-restored session must survive old confirmation");
    assert!(restored["trashed_at"].is_null());
    if std::env::var_os("RECORD_E2E").is_some() {
        let directory = std::path::Path::new("target/e2e-recordings");
        std::fs::write(
            directory.join("trash-purge-cancelled.ansi"),
            h.capture_screen_styled(),
        )
        .unwrap();
        std::fs::write(
            directory.join("trash-purge-cancelled.txt"),
            h.capture_screen(),
        )
        .unwrap();
    }
    h.send_keys("Enter");
    h.wait_for_absent("No readable retained ACP content", Duration::from_secs(5));
    h.wait_for("Archived (");
    let screen = h.capture_screen();
    let row = screen
        .lines()
        .position(|line| line.contains("TrashRetained"))
        .unwrap() as u16
        + 1;
    h.send_mouse_click(0, 7, row);
    h.wait_for("CONCURRENT-TEARDOWN-SUMMARY");
    h.run_cli_ok(&["rm", &id, "--purge"]);
    h.wait_for_absent("TrashRetained", Duration::from_secs(5));
    h.wait_for_absent("CONCURRENT-TEARDOWN-SUMMARY", Duration::from_secs(5));
    assert!(
        !launch_marker.exists(),
        "restore or purge restarted an agent"
    );
}

#[test]
#[parallel]
fn trash_reader_ctrl_c_exits() {
    use agent_of_empires::session::{Instance, Status};
    require_tmux!();
    let mut h = TuiTestHarness::new("trash_ctrl_c");
    h.stop_daemon_on_drop();
    let mut row = Instance::new("TrashQuit", h.project_path().to_str().unwrap());
    row.trashed_at = Some(chrono::Utc::now());
    row.status = Status::Stopped;
    std::fs::write(h.sessions_path(), serde_json::to_vec(&[row]).unwrap()).unwrap();
    h.start_daemon();
    h.spawn_tui();
    h.wait_for_runtime_ready();
    h.wait_for("Trash (");
    h.send_keys("End");
    h.send_keys("Right");
    h.wait_for("TrashQuit");
    let screen = h.capture_screen();
    let row = screen
        .lines()
        .position(|line| line.contains("TrashQuit"))
        .unwrap() as u16
        + 1;
    h.send_mouse_click(0, 7, row);
    h.wait_for("Trash · read-only");
    h.send_keys("Enter");
    h.wait_for("Esc: sessions");
    assert!(h
        .tmux()
        .args(["send-keys", "C-c"])
        .status()
        .unwrap()
        .success());
    h.wait_for_exit(Duration::from_secs(5));
}
