//! `aoe session wait` exits non-zero on timeout and when the session is not running.

use serial_test::parallel;

use crate::harness::{require_tmux, TuiTestHarness};

#[test]
#[parallel]
fn test_wait_times_out_then_reports_stopped_session() {
    require_tmux!();
    let mut h = TuiTestHarness::new("session_wait");
    let bin = h.install_path_command("claude");
    std::fs::write(bin.join("claude"), "#!/bin/sh\nexec sleep 600\n").expect("write claude stub");
    let project = h.project_path();
    h.add_session(&[project.to_str().unwrap(), "-t", "waiter"]);
    h.run_cli_ok(&["session", "start", "waiter"]);

    let err = h.run_cli_err(&[
        "session",
        "wait",
        "waiter",
        "--until",
        "waiting",
        "--timeout",
        "1500",
    ]);
    assert!(err.contains("timed out after 1500ms"), "stderr: {err}");

    h.run_cli_ok(&["session", "stop", "waiter"]);
    let err = h.run_cli_err(&[
        "session",
        "wait",
        "waiter",
        "--until",
        "idle",
        "--timeout",
        "1500",
    ]);
    assert!(err.contains("is not running"), "stderr: {err}");
}
