use serial_test::parallel;
use std::time::Duration;

use crate::harness::{require_tmux, TuiTestHarness};

/// The TUI starts on the empty home screen with the palette hint, and the
/// palette opens, closes, and runs a fuzzy-matched command. Quitting is covered
/// by the harness's own `tui_input_barrier_handles_shell_metacharacters_in_home`.
#[test]
#[parallel]
fn test_tui_launches_and_drives_the_command_palette() {
    require_tmux!();

    let mut h = TuiTestHarness::new("launch");
    h.spawn_tui();

    h.wait_for(" aoe ");
    h.assert_screen_contains("No sessions yet");
    // ^K Cmds is priority-1, kept even on narrow footers.
    h.assert_screen_contains("^K Cmds");

    h.send_keys("C-k");
    h.wait_for("Commands");
    // Settings/Quit may scroll off a 30-row terminal; the top group is visible.
    h.assert_screen_contains("Actions");
    h.assert_screen_contains("Rename");
    h.send_keys("Escape");
    h.wait_for_absent("Commands", Duration::from_secs(5));

    h.send_keys("C-k");
    h.wait_for("Commands");
    h.type_text("set");
    h.wait_for("Open settings");
    h.send_keys("Enter");
    h.wait_for_absent("Commands", Duration::from_secs(5));
    h.wait_for("Settings");
}
