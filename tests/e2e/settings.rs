//! E2E coverage for the Settings TUI.

use serial_test::serial;

use crate::harness::{require_tmux, TuiTestHarness};

/// The mouse-capture toggle (issue #1346) must be reachable from the Settings
/// search and editable, so the env-only `AOE_MOUSE_CAPTURE` escape hatch is no
/// longer the only knob. Search jumps to the field in the Interaction tab;
/// Space flips it from the default-on state to Disabled.
#[test]
#[serial]
fn settings_exposes_editable_mouse_capture_toggle() {
    require_tmux!();

    let mut h = TuiTestHarness::new("settings_mouse_capture");
    h.spawn_tui();

    h.wait_for("No sessions yet");
    h.send_keys("s");
    h.wait_for("Settings");

    // Settings-wide search jumps straight to the field regardless of which
    // category it lives in.
    h.send_keys("/");
    h.type_text("mouse capture");
    h.wait_for("Mouse Capture");
    h.send_keys("Enter");
    h.assert_screen_contains("Mouse Capture");

    // Default is on; toggling lands on the Disabled state.
    h.send_keys("Space");
    h.wait_for("Disabled");
}
