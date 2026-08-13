use crate::harness::{require_tmux, TuiTestHarness};
use serial_test::parallel;

/// Both system-health surfaces are reachable independently from the command
/// palette, without reserving a global function key.
#[test]
#[parallel]
fn test_system_health_palette_actions() {
    require_tmux!();

    let mut h = TuiTestHarness::new("diagnostics_toggle");
    h.spawn_tui();

    h.wait_for(" aoe ");
    // Off by default.
    h.assert_screen_not_contains("CPU ");

    h.send_keys("C-k");
    h.type_text("toggle system health strip");
    h.send_keys("Enter");
    h.wait_for("CPU ");
    h.wait_for("Mem ");

    h.send_keys("C-k");
    h.type_text("open system health");
    h.send_keys("Enter");
    h.wait_for("System Health");
    h.wait_for("No running AoE agents");
}
