use serial_test::parallel;
use std::time::Duration;

use crate::harness::{require_tmux, TuiTestHarness};

/// Helper: create extra profile directories in the harness's isolated home.
fn create_profile(h: &TuiTestHarness, name: &str) {
    let config_dir = crate::harness::app_dir_in(h.home_path());
    std::fs::create_dir_all(config_dir.join("profiles").join(name)).expect("create profile dir");
}

#[test]
#[parallel]
fn test_profile_picker_opens_and_closes() {
    require_tmux!();

    let mut h = TuiTestHarness::new("picker_open");
    h.spawn_tui();

    // Default launch is now all-profiles mode
    h.wait_for(" aoe ");
    h.send_keys("P");
    h.wait_for("Profiles");
    h.assert_screen_contains("default");

    // Esc closes
    h.send_keys("Escape");
    h.wait_for_absent("Profiles", Duration::from_secs(5));
    h.assert_screen_contains("No sessions yet");
}

#[test]
#[parallel]
fn test_profile_picker_shows_multiple_profiles() {
    require_tmux!();

    let mut h = TuiTestHarness::new("picker_multi");
    create_profile(&h, "work");
    create_profile(&h, "personal");
    h.spawn_tui();

    h.wait_for(" aoe ");
    h.send_keys("P");
    h.wait_for("Profiles");
    h.assert_screen_contains("default");
    h.assert_screen_contains("work");
    h.assert_screen_contains("personal");
}

#[test]
#[parallel]
fn test_profile_picker_create_new_profile() {
    require_tmux!();

    let mut h = TuiTestHarness::new("picker_create");
    h.spawn_tui();

    h.wait_for(" aoe ");
    h.send_keys("P");
    h.wait_for("Profiles");

    // Press 'n' to enter create mode
    h.send_keys("n");
    h.wait_for("New Profile");
    h.assert_screen_contains("Name:");

    // Type a name and confirm
    h.type_text("testprof");
    h.send_keys("Enter");

    // Should switch to the new profile and close the picker
    h.wait_for_absent("New Profile", Duration::from_secs(5));
}

#[test]
#[parallel]
fn test_profile_picker_create_esc_returns_to_list() {
    require_tmux!();

    let mut h = TuiTestHarness::new("picker_create_esc");
    h.spawn_tui();

    h.wait_for(" aoe ");
    h.send_keys("P");
    h.wait_for("Profiles");

    h.send_keys("n");
    h.wait_for("New Profile");

    h.send_keys("Escape");
    h.wait_for_absent("New Profile", Duration::from_secs(5));
    // Should be back in list mode, not fully closed
    h.assert_screen_contains("Profiles");
}

#[test]
#[parallel]
fn test_profile_picker_delete_flow() {
    require_tmux!();

    let mut h = TuiTestHarness::new("picker_delete");
    create_profile(&h, "deleteme");
    h.spawn_tui();

    h.wait_for(" aoe ");
    h.send_keys("P");
    h.wait_for("Profiles");
    h.assert_screen_contains("deleteme");

    // Navigate to "deleteme" (after "default" alphabetically)
    h.send_keys("j");
    std::thread::sleep(Duration::from_millis(50));

    // Press 'd' to delete
    h.send_keys("d");
    h.wait_for("Delete Profile");
    h.assert_screen_contains("[Yes]");
    h.assert_screen_contains("[No]");

    // Confirm with 'y'
    h.send_keys("y");

    // Picker should stay open with refreshed list
    h.wait_for_absent("Delete Profile", Duration::from_secs(5));
    h.assert_screen_contains("Profiles");
    h.assert_screen_not_contains("deleteme");
}

#[test]
#[parallel]
fn test_profile_picker_delete_cancel() {
    require_tmux!();

    let mut h = TuiTestHarness::new("picker_del_cancel");
    create_profile(&h, "keepme");
    h.spawn_tui();

    h.wait_for(" aoe ");
    h.send_keys("P");
    h.wait_for("Profiles");

    // Navigate to "keepme"
    h.send_keys("j");
    std::thread::sleep(Duration::from_millis(50));

    h.send_keys("d");
    h.wait_for("Delete Profile");

    // Cancel with Esc
    h.send_keys("Escape");
    h.wait_for_absent("Delete Profile", Duration::from_secs(5));
    // Back to list, profile still there
    h.assert_screen_contains("Profiles");
    h.assert_screen_contains("keepme");
}

#[test]
#[parallel]
fn test_profile_picker_switch_profile() {
    require_tmux!();

    let mut h = TuiTestHarness::new("picker_switch");
    create_profile(&h, "other");
    h.spawn_tui();

    h.wait_for(" aoe ");
    h.send_keys("P");
    h.wait_for("Profiles");

    // Navigate to "other" and select
    h.send_keys("j");
    std::thread::sleep(Duration::from_millis(50));
    h.send_keys("Enter");

    // Picker should close
    h.wait_for_absent("Profiles", Duration::from_secs(5));
}
