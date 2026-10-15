//! E2E coverage for `aoe logs`.
//!
//! Drives the binary via `run_cli` against a fixture log seeded inside the
//! harness's isolated `$HOME` so the real user's logs are never read or
//! touched. `--no-pager` keeps the output deterministic.

use serial_test::parallel;

use crate::harness::{app_dir_in, TuiTestHarness};

#[test]
#[parallel]
fn logs_prints_tails_and_locates_the_debug_log() {
    let h = TuiTestHarness::new("logs");

    // Missing file: exit 0 with a hint rather than an error.
    let out = h.run_cli(&["logs", "--no-pager"]);
    assert!(out.status.success(), "should exit 0 when missing");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("does not exist"), "{stderr}");

    let app_dir = app_dir_in(h.home_path());
    std::fs::create_dir_all(&app_dir).expect("create app dir");
    let path = app_dir.join("debug.log");
    std::fs::write(&path, "a\nb\nc\nd\ne\n").expect("write debug.log");

    assert_eq!(h.run_cli_ok(&["logs", "--no-pager"]), "a\nb\nc\nd\ne\n");
    assert_eq!(
        h.run_cli_ok(&["logs", "--no-pager", "--lines", "2"]),
        "d\ne\n"
    );
    assert_eq!(
        h.run_cli_ok(&["logs", "--path"]).trim(),
        path.to_string_lossy()
    );
}
