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

/// Drive a full archive -> restore cycle through the real TUI.
///
/// Verifies the user-visible contract end to end: archiving keeps the row
/// selected (its calm "Archived" placeholder is what the preview shows, even
/// with another session present to swap to) and reveals the Archived section;
/// restoring does NOT flash the "tmux session is gone" corpse error and brings
/// the row back to the live list.
#[test]
#[serial]
fn test_archive_then_restore_cycle() {
    require_tmux!();

    let mut h = TuiTestHarness::new("archive_restore");
    install_persistent_claude(&mut h);

    let project = h.project_path();
    // Two sessions so "selection stays on the archived row" is meaningful: the
    // pre-fix bug swapped the selection to the neighbour below.
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
    h.wait_for("Archived");
    let after_archive = h.capture_screen();

    // The calm placeholder proves the archived session is still the selected
    // preview target (it did not swap to Neighbor).
    assert!(
        after_archive.contains("Press z to restore"),
        "archived preview should show the calm placeholder for the still-selected row\n{after_archive}"
    );
    assert!(
        after_archive.contains("is parked"),
        "archived preview should explain the parked state\n{after_archive}"
    );
    assert!(
        !after_archive.contains("tmux session is gone"),
        "archived preview must not show the corpse error\n{after_archive}"
    );

    // Restore it.
    h.send_keys("z");
    std::thread::sleep(Duration::from_millis(400));
    let after_restore = h.capture_screen();
    assert!(
        !after_restore.contains("tmux session is gone"),
        "restore must not flash the 'tmux session is gone' error\n{after_restore}"
    );
    assert!(
        after_restore.contains("Archivo"),
        "restored row should be back in the live list\n{after_restore}"
    );

    // Let the background revive settle; the corpse error must not reappear.
    std::thread::sleep(Duration::from_millis(2500));
    let settled = h.capture_screen();
    assert!(
        !settled.contains("tmux session is gone"),
        "restored session must not settle on the corpse error\n{settled}"
    );
}
