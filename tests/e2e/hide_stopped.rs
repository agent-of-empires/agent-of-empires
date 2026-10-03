//! `y` in the home view hides stopped sessions inside groups.

use std::time::Duration;

use serial_test::parallel;

use crate::harness::{require_tmux, TuiTestHarness};

/// A stopped session drops out of its group and the header counts it; a second `y` brings it back.
#[test]
#[parallel]
fn test_y_hides_a_stopped_session_in_its_group() {
    require_tmux!();
    let mut h = TuiTestHarness::new("hide_stopped");
    let bin = h.install_path_command("claude");
    std::fs::write(bin.join("claude"), "#!/bin/sh\nexec sleep 600\n").expect("write claude stub");
    let project = h.project_path();
    let project = project.to_str().unwrap();
    h.add_session(&[project, "-t", "kept-session", "-g", "util"]);
    h.add_session(&[project, "-t", "stopped-session", "-g", "util"]);
    h.run_cli_ok(&["session", "start", "stopped-session"]);
    h.run_cli_ok(&["session", "stop", "stopped-session"]);

    h.spawn_tui();
    h.wait_for("stopped-session");
    h.assert_screen_contains("util (2)");

    h.send_keys("y");
    h.wait_for("util (1/2)");
    h.wait_for_absent("stopped-session", Duration::from_secs(5));
    h.assert_screen_contains("kept-session");

    h.send_keys("y");
    h.wait_for("stopped-session");
    h.assert_screen_contains("util (2)");
}
