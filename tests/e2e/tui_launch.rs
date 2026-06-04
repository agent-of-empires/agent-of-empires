use serial_test::serial;
use std::time::Duration;

use crate::harness::{require_tmux, TuiTestHarness};

#[test]
#[serial]
fn test_tui_launches_and_shows_home_screen() {
    require_tmux!();

    let mut h = TuiTestHarness::new("launch");
    h.spawn_tui();

    h.wait_for(" aoe ");
    h.assert_screen_contains("No sessions yet");
    // Status bar should be visible. ^K Cmds is priority-1 (kept even on
    // narrow footers) and the caret glyph is distinctive enough to survive
    // any future reshuffling.
    h.assert_screen_contains("^K Cmds");
}

#[test]
#[serial]
fn test_tui_quit_with_q() {
    require_tmux!();

    let mut h = TuiTestHarness::new("quit");
    h.spawn_tui();

    h.wait_for(" aoe ");
    // `q` opens the quit confirmation (on by default, #1569); it does not
    // exit on its own anymore.
    h.send_keys("q");
    h.wait_for("Quit Agent of Empires");
    // Confirm to actually exit.
    h.send_keys("y");
    h.wait_for_exit(Duration::from_secs(5));
    assert!(
        !h.session_alive(),
        "session should have exited after confirming quit"
    );
}
