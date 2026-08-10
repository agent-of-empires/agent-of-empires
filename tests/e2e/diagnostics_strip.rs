use serial_test::parallel;
use std::time::Duration;

use crate::harness::{require_tmux, TuiTestHarness};

/// F9 toggles the memory diagnostics strip on and off. The strip always shows
/// the "N agents · N procs" counts (even before the first memory sample), so
/// "procs" is a stable anchor for its presence.
#[test]
#[parallel]
fn test_diagnostics_strip_toggles_with_f9() {
    require_tmux!();

    let mut h = TuiTestHarness::new("diagnostics_toggle");
    h.spawn_tui();

    h.wait_for(" aoe ");
    // Off by default.
    h.assert_screen_not_contains("procs");

    h.send_keys("F9");
    h.wait_for("procs");

    h.send_keys("F9");
    h.wait_for_absent("procs", Duration::from_secs(5));
}
