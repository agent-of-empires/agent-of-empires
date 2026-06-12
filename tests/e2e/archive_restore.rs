use serial_test::serial;
use std::time::Duration;

use crate::harness::{require_tmux, TuiTestHarness};

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
