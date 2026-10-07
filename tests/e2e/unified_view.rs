use serial_test::parallel;
use std::time::Duration;

use crate::harness::{require_tmux, TuiTestHarness};

/// Helper: create a profile with a session in the harness's isolated home.
fn create_profile_with_session(h: &TuiTestHarness, profile: &str, title: &str) {
    let config_dir = crate::harness::app_dir_in(h.home_path());
    let profile_dir = config_dir.join("profiles").join(profile);
    std::fs::create_dir_all(&profile_dir).expect("create profile dir");

    let session = format!(
        r#"[{{"id":"test_{profile}","title":"{title}","project_path":"/tmp/{profile}","group_path":"","command":"","tool":"claude","yolo_mode":false,"status":"idle","created_at":"2026-01-01T00:00:00Z"}}]"#,
    );
    std::fs::write(profile_dir.join("sessions.json"), session).expect("write sessions.json");
}

/// The default view lists every profile's sessions flat with no `[profile]`
/// tag; the picker filters to one profile and its "all" entry returns.
#[test]
#[parallel]
fn test_profile_filter_round_trip_via_picker() {
    require_tmux!();

    let mut h = TuiTestHarness::new("unified_filter");
    create_profile_with_session(&h, "alpha", "Alpha Session");
    create_profile_with_session(&h, "beta", "Beta Session");
    h.spawn_tui();

    h.wait_for(" aoe ");
    h.assert_screen_not_contains("[alpha]");
    h.assert_screen_not_contains("[beta]");
    h.assert_screen_contains("Alpha Session");
    h.assert_screen_contains("Beta Session");

    // In all-mode the picker lists profiles directly; "alpha" sorts first.
    h.send_keys("P");
    h.wait_for("Profiles");
    h.send_keys("Enter");
    h.wait_for("[alpha]");
    h.assert_screen_contains("Alpha Session");
    h.assert_screen_not_contains("Beta Session");

    // In filtered mode "all" sits at the top of the picker.
    h.send_keys("P");
    h.wait_for("Profiles");
    for _ in 0..3 {
        h.send_keys("k");
    }
    h.send_keys("Enter");
    h.wait_for_absent("[alpha]", Duration::from_secs(5));
    h.assert_screen_contains("Alpha Session");
    h.assert_screen_contains("Beta Session");
}
