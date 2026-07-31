//! E2E coverage for `aoe logs`.
//!
//! Drives the binary via `run_cli` against a fixture log seeded inside the
//! harness's isolated `$HOME` so the real user's logs are never read or
//! touched. We use `--no-pager` everywhere to keep the test deterministic
//! (no interactive viewer launch).

use serial_test::parallel;

use crate::harness::{app_dir_in, TuiTestHarness};

/// Write a fake debug.log under the harness's isolated app dir.
fn seed_debug_log(h: &TuiTestHarness, content: &str) -> std::path::PathBuf {
    let app_dir = app_dir_in(h.home_path());
    std::fs::create_dir_all(&app_dir).expect("create app dir");
    let path = app_dir.join("debug.log");
    std::fs::write(&path, content).expect("write debug.log");
    path
}

#[test]
#[parallel]
fn logs_no_pager_prints_debug_log_to_stdout() {
    let h = TuiTestHarness::new("logs_no_pager");
    seed_debug_log(
        &h,
        "2024-01-01T00:00:01Z  INFO line one\n\
         2024-01-01T00:00:02Z  INFO line two\n",
    );

    let out = h.run_cli(&["logs", "--no-pager"]);
    assert!(
        out.status.success(),
        "aoe logs --no-pager failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("line one"),
        "stdout missing 'line one': {stdout}"
    );
    assert!(
        stdout.contains("line two"),
        "stdout missing 'line two': {stdout}"
    );
}

#[test]
#[parallel]
fn logs_lines_returns_only_tail() {
    let h = TuiTestHarness::new("logs_lines");
    seed_debug_log(&h, "a\nb\nc\nd\ne\n");

    let out = h.run_cli(&["logs", "--no-pager", "--lines", "2"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout, "d\ne\n");
}

#[test]
#[parallel]
fn logs_path_prints_configured_log_path() {
    let h = TuiTestHarness::new("logs_path");
    let path = seed_debug_log(&h, "");

    let out = h.run_cli(&["logs", "--path"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(stdout.trim(), path.to_string_lossy());
}

#[test]
#[parallel]
fn logs_serve_flag_rejected() {
    // `--serve` and `--all` were removed when serve.log was dropped; the
    // configured log file (debug.log by default) carries everything now.
    let h = TuiTestHarness::new("logs_serve_removed");
    let out = h.run_cli(&["logs", "--serve", "--no-pager"]);
    assert!(
        !out.status.success(),
        "removed --serve flag should exit non-zero"
    );
}

#[test]
#[parallel]
fn logs_all_flag_rejected() {
    let h = TuiTestHarness::new("logs_all_removed");
    let out = h.run_cli(&["logs", "--all", "--no-pager"]);
    assert!(
        !out.status.success(),
        "removed --all flag should exit non-zero"
    );
}

#[test]
#[parallel]
fn logs_missing_file_exits_zero_with_hint() {
    let h = TuiTestHarness::new("logs_missing");
    // Don't seed: app dir + debug.log absent.
    let out = h.run_cli(&["logs", "--no-pager"]);
    assert!(out.status.success(), "should exit 0 when missing");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("does not exist"),
        "stderr should explain missing file: {stderr}"
    );
}
