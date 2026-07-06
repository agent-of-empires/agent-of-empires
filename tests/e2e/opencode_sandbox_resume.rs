//! Regression test for #2605: sandboxed opencode's SQLite database (which
//! holds `ses_*` session identity) must survive `prepare_sandbox_dir` cycles.
//!
//! Before the fix, the opencode `AgentConfigMount` listed `opencode.db*` in
//! `clean_files`; `prepare_sandbox_dir` walked that list unconditionally on
//! every agent-config refresh and per-tool sandbox setup, meaning every
//! kill/restart destroyed the DB. `aoe resume` then invoked
//! `opencode --session <id>` against an empty DB and opencode exited 1 with
//! "Session not found". `skip_entries` already blocks host-to-sandbox copy,
//! so removing the entries from `clean_files` is safe.
//!
//! Requires a running Docker daemon; marked `#[ignore]` for CI per the
//! sandbox e2e convention (see `tests/e2e/sandbox.rs`).

use serial_test::serial;

use crate::harness::TuiTestHarness;

#[test]
#[serial]
#[ignore = "requires Docker daemon"]
fn sandboxed_opencode_db_survives_prepare_sandbox_cycle() {
    let mut h = TuiTestHarness::new("opencode_sandbox_resume");
    // Install an `opencode` PATH shim so `aoe add --cmd opencode` resolves
    // during tool discovery. The container has its own opencode binary; the
    // shim only needs to make `which opencode` succeed on the host.
    h.install_path_command("opencode");
    let project = h.project_path();

    // First `aoe add --sandbox --cmd opencode`: with `config_tool == "opencode"`
    // the filter at container_config.rs:1432 selects the opencode mounts, so
    // `prepare_sandbox_dir` runs against the data-dir mount. This is the exact
    // code path #2605 lives in.
    let add1 = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--sandbox",
        "--cmd",
        "opencode",
        "-t",
        "OpencodeResumeFirst",
    ]);
    assert!(
        add1.status.success(),
        "first aoe add --sandbox --cmd opencode failed: stdout={} stderr={}",
        String::from_utf8_lossy(&add1.stdout),
        String::from_utf8_lossy(&add1.stderr),
    );

    // Simulate what an opencode-in-container run leaves behind: a SQLite DB
    // holding a `ses_*` session row. The regression in #2605 fired inside
    // `prepare_sandbox_dir`, which wiped these files on every subsequent
    // invocation.
    let sandbox_dir = h.home_path().join(".local/share/opencode/sandbox");
    std::fs::create_dir_all(&sandbox_dir).expect("create opencode sandbox dir");
    let db_bytes = b"SQLite format 3\0-ses_regression_2605";
    std::fs::write(sandbox_dir.join("opencode.db"), db_bytes).unwrap();
    std::fs::write(sandbox_dir.join("opencode.db-wal"), b"wal-frames").unwrap();
    std::fs::write(sandbox_dir.join("opencode.db-shm"), b"shm-index").unwrap();

    // A second `aoe add --sandbox --cmd opencode` re-runs the same filtered
    // walk and re-invokes `prepare_sandbox_dir` for the opencode mounts.
    // Before the fix, this cycle destroyed the DB planted above; after the
    // fix, `clean_files` is empty for the opencode data-dir mount and the DB
    // must survive intact.
    let add2 = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--sandbox",
        "--cmd",
        "opencode",
        "-t",
        "OpencodeResumeSecond",
    ]);
    assert!(
        add2.status.success(),
        "second aoe add --sandbox --cmd opencode failed: stdout={} stderr={}",
        String::from_utf8_lossy(&add2.stdout),
        String::from_utf8_lossy(&add2.stderr),
    );

    let db_path = sandbox_dir.join("opencode.db");
    assert!(
        db_path.exists(),
        "opencode.db must survive prepare_sandbox_dir cycles (regression of #2605); \
         wiping this file is what breaks `aoe resume` for sandboxed opencode",
    );
    assert_eq!(
        std::fs::read(&db_path).unwrap(),
        db_bytes,
        "opencode.db content must be untouched across CLI cycles (regression of #2605)",
    );
    assert!(
        sandbox_dir.join("opencode.db-wal").exists(),
        "opencode.db-wal must survive (regression of #2605)",
    );
    assert!(
        sandbox_dir.join("opencode.db-shm").exists(),
        "opencode.db-shm must survive (regression of #2605)",
    );
}
