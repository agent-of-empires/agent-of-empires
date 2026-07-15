use serial_test::serial;
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

/// Best-effort cleanup so failures don't leak into the next `#[serial]` test.
fn kill_tmux(sock: &std::path::Path, name: &str) {
    let _ = Command::new("tmux")
        .arg("-S")
        .arg(sock)
        .args(["kill-session", "-t", name])
        .output();
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
                r#"{{"id":"{id}","title":"{title}","project_path":"{project}","group_path":"","command":"","tool":"claude","yolo_mode":false,"status":"idle","created_at":"2026-01-01T00:00:00Z"}}"#,
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
#[serial]
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
    h.wait_for(" aoe ");
    h.wait_for("Archivo");
    h.wait_for("Neighbor");
    // Cursor starts on the top row (Archivo); give startup recovery a beat.
    std::thread::sleep(Duration::from_millis(1200));

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

    // Navigate into the Archived section: down to the header, expand it,
    // down onto the parked row. Its preview shows the calm placeholder.
    h.send_keys("j");
    h.send_keys("l");
    h.send_keys("j");
    h.wait_for("is parked");
    let parked = h.capture_screen();
    assert!(
        parked.contains("to unarchive"),
        "archived preview should point at z to unarchive\n{parked}"
    );

    // Unarchive it; the row returns to the active list, still selected.
    h.send_keys("z");
    h.wait_for_absent("is parked", Duration::from_secs(5));
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

/// Locks #1868: archive kills all four tmux session kinds. Pre-creates real
/// sessions, runs the CLI, asserts each kind is gone.
#[test]
#[serial]
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
#[serial]
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
#[serial]
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

    // Pre-create long-lived agent sessions on the harness socket so they are
    // demonstrably alive right up until archive; because the tmux session
    // already exists under the name the instance computes, TUI startup detects
    // it as running and does not relaunch it. These run before `spawn_tui` and
    // so start the tmux server, which is why they must go through the harness
    // helper: it pins the same env `spawn_tui` uses onto the server.
    for name in &names {
        h.tmux_new_detached(name, "sleep 600");
    }

    h.spawn_tui();
    h.wait_for(" aoe ");
    // The group header renders as "name (count)"; its presence proves the group
    // loaded with all three members.
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

    // Groups render before ungrouped rows, so the group header is the top row;
    // pressing up past the top clamps the cursor onto it. `z` on a selected
    // group opens the archive-confirm dialog; `y` submits it.
    for _ in 0..sessions.len() + 2 {
        h.send_keys("k");
    }
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
