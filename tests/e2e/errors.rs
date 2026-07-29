use serial_test::serial;

use crate::harness::TuiTestHarness;

#[test]
#[serial]
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
#[serial]
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
