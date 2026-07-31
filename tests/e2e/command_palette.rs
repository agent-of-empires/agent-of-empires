use serial_test::parallel;
use std::time::Duration;

use crate::harness::{require_tmux, TuiTestHarness};

#[test]
#[parallel]
fn test_command_palette_opens_with_ctrl_k() {
    require_tmux!();

    let mut h = TuiTestHarness::new("palette_open");
    h.spawn_tui();

    h.wait_for(" aoe ");
    h.send_keys("C-k");
    h.wait_for("Commands");
    // Both assertions target entries in the top "Actions" group so they're
    // visible regardless of how the dialog clamps in a 30-row test terminal.
    // Settings/Quit live further down the list and may scroll off-screen.
    h.assert_screen_contains("Actions");
    h.assert_screen_contains("Rename");
}

#[test]
#[parallel]
fn test_command_palette_esc_closes() {
    require_tmux!();

    let mut h = TuiTestHarness::new("palette_close");
    h.spawn_tui();

    h.wait_for(" aoe ");
    h.send_keys("C-k");
    h.wait_for("Commands");

    h.send_keys("Escape");
    h.wait_for_absent("Commands", Duration::from_secs(5));
}

#[test]
#[parallel]
fn test_command_palette_fuzzy_search_settings() {
    require_tmux!();

    let mut h = TuiTestHarness::new("palette_fuzzy");
    h.spawn_tui();

    h.wait_for(" aoe ");
    h.send_keys("C-k");
    h.wait_for("Commands");

    // Type "set" to filter to "Open settings"
    h.type_text("set");
    std::thread::sleep(Duration::from_millis(100));
    h.assert_screen_contains("Open settings");

    // Enter should run it (opens settings view).
    h.send_keys("Enter");
    h.wait_for_absent("Commands", Duration::from_secs(5));
    h.wait_for("Settings");
}

#[test]
#[parallel]
fn test_status_bar_shows_palette_hint() {
    require_tmux!();

    let mut h = TuiTestHarness::new("palette_hint");
    h.spawn_tui();

    h.wait_for(" aoe ");
    // The footer should mention the Ctrl+K shortcut so users discover it.
    h.assert_screen_contains("^K");
}

// Help dialog content is verified by a unit test in `src/tui/components/help.rs`
// (`help_lists_command_palette`); we don't e2e it here because the help dialog
// is taller than the 100x30 test terminal and the bottom section clips off
// screen, which is a separate UX concern.
