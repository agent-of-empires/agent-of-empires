use serial_test::parallel;
use std::time::Duration;

use crate::harness::{require_tmux, TuiTestHarness};

/// Helper: create extra profile directories in the harness's isolated home.
fn create_profile(h: &TuiTestHarness, name: &str) {
    let config_dir = crate::harness::app_dir_in(h.home_path());
    std::fs::create_dir_all(config_dir.join("profiles").join(name)).expect("create profile dir");
}

/// Open and close the picker, cancel then confirm a delete, cancel then
/// complete a create, which switches to the new profile and closes the picker.
#[test]
#[parallel]
fn test_profile_picker_lists_deletes_and_creates() {
    require_tmux!();

    let mut h = TuiTestHarness::new("picker_flow");
    create_profile(&h, "deleteme");
    create_profile(&h, "work");
    h.spawn_tui();

    h.wait_for(" aoe ");
    h.send_keys("P");
    h.wait_for("Profiles");
    for name in ["default", "deleteme", "work"] {
        h.assert_screen_contains(name);
    }
    h.send_keys("Escape");
    h.wait_for_absent("Profiles", Duration::from_secs(5));
    h.assert_screen_contains("No sessions yet");

    // "deleteme" is the first row: the picker sinks "default" last.
    h.send_keys("P");
    h.wait_for("Profiles");
    h.send_keys("d");
    h.wait_for("Delete Profile");
    h.assert_screen_contains("[Yes]");
    h.assert_screen_contains("[No]");
    h.send_keys("Escape");
    h.wait_for_absent("Delete Profile", Duration::from_secs(5));
    h.assert_screen_contains("deleteme");
    h.send_keys("d");
    h.wait_for("Delete Profile");
    h.send_keys("y");
    h.wait_for_absent("Delete Profile", Duration::from_secs(5));
    h.assert_screen_contains("Profiles");
    h.assert_screen_not_contains("deleteme");

    h.send_keys("n");
    h.wait_for("New Profile");
    h.assert_screen_contains("Name:");
    h.send_keys("Escape");
    h.wait_for_absent("New Profile", Duration::from_secs(5));
    h.assert_screen_contains("Profiles");
    h.send_keys("n");
    h.wait_for("New Profile");
    h.type_text("testprof");
    h.send_keys("Enter");
    h.wait_for_absent("Profiles", Duration::from_secs(5));
}
