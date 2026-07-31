use serial_test::parallel;

use crate::harness::TuiTestHarness;

#[test]
#[parallel]
fn test_cli_remove_nonexistent() {
    let h = TuiTestHarness::new("cli_rm_noexist");

    let output = h.run_cli(&["remove", "nonexistent-session-id-12345"]);
    assert!(
        !output.status.success(),
        "aoe remove should fail for nonexistent session"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let combined = format!("{}{}", stdout, stderr);
    assert!(
        combined.contains("not found")
            || combined.contains("No session")
            || combined.contains("error")
            || combined.contains("Error"),
        "expected error message about missing session.\nstdout: {}\nstderr: {}",
        stdout,
        stderr
    );
}

/// Regression test for #2896: routing fatal errors through the tracing sink
/// must not regress the interactive path. A one-shot CLI command runs without a
/// tracing subscriber, so the sink swallows the error; `main`'s `eprintln!`
/// fallback is the only thing the user sees. Assert stderr specifically (not
/// combined stdout+stderr) carries the reason and the exit stays non-zero.
#[test]
#[parallel]
fn test_fatal_error_prints_to_stderr_and_exits_nonzero() {
    let h = TuiTestHarness::new("cli_fatal_stderr");

    let output = h.run_cli(&["remove", "nonexistent-session-id-12345"]);
    assert!(
        !output.status.success(),
        "a fatal error from main must exit non-zero"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Error:"),
        "stderr must carry the fatal reason for interactive users; stderr was: {stderr}"
    );
}

/// A CLI subcommand with file logging enabled (`AOE_LOG_LEVEL` set) must route
/// a fatal error through the tracing sink, so the failure reason is written to
/// the configured log file and not only to stderr.
#[test]
#[parallel]
fn test_fatal_error_routes_to_debug_log_when_subscriber_enabled() {
    let mut h = TuiTestHarness::new("cli_fatal_logged");
    h.set_env("AOE_LOG_LEVEL", "info");

    let output = h.run_cli(&["remove", "nonexistent-session-id-12345"]);
    assert!(
        !output.status.success(),
        "a fatal error from main must exit non-zero"
    );

    let debug_log = crate::harness::app_dir_in(h.home_path()).join("debug.log");
    let contents = std::fs::read_to_string(&debug_log)
        .unwrap_or_else(|e| panic!("debug.log unreadable at {}: {}", debug_log.display(), e));
    assert!(
        contents.contains("fatal:"),
        "a one-shot CLI fatal must reach the tracing sink; debug.log was:\n{contents}"
    );
}
