use serial_test::parallel;

use crate::harness::TuiTestHarness;

/// A fatal error from a one-shot CLI command exits non-zero and reaches the
/// user either way. Without a tracing subscriber the sink swallows it, so
/// `main`'s `eprintln!` fallback must carry the reason on stderr (#2896). With
/// file logging enabled (`AOE_LOG_LEVEL`) it also reaches the configured log
/// file through the tracing sink (#3150).
#[test]
#[parallel]
fn test_fatal_error_reaches_stderr_and_the_debug_log() {
    let mut h = TuiTestHarness::new("cli_fatal");

    let output = h.run_cli(&["remove", "nonexistent-session-id-12345"]);
    assert!(!output.status.success(), "a fatal error must exit non-zero");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Error:"),
        "stderr must carry the fatal reason for interactive users; stderr was: {stderr}"
    );

    h.set_env("AOE_LOG_LEVEL", "info");
    let output = h.run_cli(&["remove", "nonexistent-session-id-12345"]);
    assert!(!output.status.success(), "a fatal error must exit non-zero");
    let debug_log = crate::harness::app_dir_in(h.home_path()).join("debug.log");
    let contents = std::fs::read_to_string(&debug_log)
        .unwrap_or_else(|e| panic!("debug.log unreadable at {}: {}", debug_log.display(), e));
    assert!(
        contents.contains("fatal:"),
        "a one-shot CLI fatal must reach the tracing sink; debug.log was:\n{contents}"
    );
}
