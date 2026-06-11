//! Full-stack regression for #1890: the daemon must recover a structured-view
//! session whose first fresh-spawn handshake fails while the detached runner
//! stays alive and registered on disk.
//!
//! The original symptom only reproduced on slow/contended macOS hardware: the
//! runner subprocess bound its socket and wrote its registry entry before the
//! daemon-side ACP handshake completed, the handshake then lost the race, and
//! the reconciler pinned the session in its `attempted` set forever. `aoe acp
//! ps` showed the worker alive, but every prompt 404'd ("session not found on
//! the daemon"). Stock CI runners win the handshake race, so this drives the
//! failure deterministically instead: `AOE_ACP_TEST_FAIL_FIRST_HANDSHAKES=1`
//! makes the daemon fail exactly the first fresh-spawn handshake (debug-only
//! hook in `acp_client::connect_via_socket`), leaving the orphaned-but-live
//! runner. The reconciler's readopt pass must then clear `attempted` and bring
//! the session back, so the prompt is eventually accepted.
//!
//! Without the fix this test hangs until the 45s prompt deadline and panics;
//! with it, the prompt is accepted once the worker is recovered.
//!
//! Compiled only with `--features serve`. Run via:
//!
//! ```sh
//! cargo test --test e2e --features serve -- acp_orphan_runner_recovery
//! ```
#![cfg(feature = "serve")]

use std::time::{Duration, Instant};

use serial_test::serial;

use crate::harness::{pick_free_port, require_node, require_tmux, wait_for_port, TuiTestHarness};

/// Minimal one-turn fake-ACP script: accept the prompt and end the turn
/// immediately. The test only cares that the prompt POST is accepted (the
/// readiness oracle for "worker live + handshake done"), not what the agent
/// renders.
const NOOP_SCRIPT: &str = r#"{
  "turns": [
    { "updates": [], "stopReason": "end_turn" }
  ]
}"#;

/// Parse the `  ID:      <id>` line that `aoe add` prints on success.
fn parse_session_id(add_stdout: &str) -> String {
    add_stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("ID:"))
        .map(|rest| rest.trim().to_string())
        .unwrap_or_else(|| panic!("could not find session ID in `aoe add` output:\n{add_stdout}"))
}

/// Retry `aoe acp prompt` until accepted. The prompt POST 404s while no worker
/// is live; a successful call proves the reconciler recovered the orphaned
/// runner. With the bug present this never succeeds and the loop panics at the
/// deadline, which is the regression oracle.
fn prompt_until_accepted(h: &TuiTestHarness, session_id: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let out = h.run_cli(&["acp", "prompt", session_id, "please proceed"]);
        if out.status.success() {
            return;
        }
        // Only the "no live worker yet" 404 is an expected transient while the
        // reconciler recovers the orphaned runner; the client renders every
        // such 404 as "... not found on the daemon". Any other failure (a 500,
        // a transport error, a real regression) should fail fast instead of
        // being masked as a 45s timeout.
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !stderr.contains("not found on the daemon") {
            panic!(
                "acp prompt failed with an unexpected error before recovery.\n\
                 stdout: {}\n stderr: {}",
                String::from_utf8_lossy(&out.stdout),
                stderr,
            );
        }
        if Instant::now() >= deadline {
            let ps = h.run_cli(&["acp", "ps", "--json"]);
            panic!(
                "structured view worker never recovered after an injected fresh-handshake \
                 failure within {:?}.\n last prompt stdout: {}\n last prompt stderr: {}\n \
                 acp ps: {}",
                timeout,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
                String::from_utf8_lossy(&ps.stdout),
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Stand up a live daemon with the first fresh-spawn handshake rigged to fail,
/// create a structured-view session, and assert the daemon recovers the
/// orphaned runner so a prompt is eventually accepted.
#[test]
#[serial]
fn acp_recovers_orphaned_runner_after_failed_first_handshake() {
    require_tmux!();
    require_node!();

    // HOME under /tmp: structured view workers bind a unix socket under the app
    // dir, and a deep tempdir overflows the macOS sun_path limit.
    let mut h = TuiTestHarness::new_in_tmp("acp_orphan_recovery");

    // Shared Node fake-ACP agent, scripted to accept one no-op turn.
    let script_path = h.home_path().join("noop-script.json");
    std::fs::write(&script_path, NOOP_SCRIPT).expect("write fake-acp script");
    h.install_acp_shim(&script_path);

    // The fault hook: fail exactly the first fresh-spawn ACP handshake in the
    // daemon, reproducing the #1890 orphaned-but-live-runner state. Set before
    // the daemon starts so it inherits the var.
    h.set_env("AOE_ACP_TEST_FAIL_FIRST_HANDSHAKES", "1");

    // Tear down the worker + daemon on Drop so a panicking assertion can't
    // leak a daemon onto the test port between serial tests.
    h.stop_daemon_on_drop();

    // A structured view session needs a git repo as its workspace; create one.
    let project = h.project_path();
    for args in [
        vec!["init", "-q"],
        vec!["commit", "--allow-empty", "-q", "-m", "init"],
    ] {
        let out = std::process::Command::new("git")
            .args(&args)
            .current_dir(&project)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // Start the daemon.
    let port = pick_free_port();
    let port_s = port.to_string();
    let start = h.run_cli(&["serve", "--daemon", "--port", &port_s, "--no-auth"]);
    assert!(
        start.status.success(),
        "aoe serve --daemon failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr),
    );
    assert!(
        wait_for_port(port, Duration::from_secs(10)),
        "daemon never bound port {}",
        port
    );

    // Create the structured view session. The reconciler auto-spawns the
    // worker; its first handshake is rigged to fail, orphaning the runner.
    let add = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "orphan-recovery",
        "-c",
        "claude",
        "--structured-view",
    ]);
    assert!(
        add.status.success(),
        "aoe add --structured-view failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&add.stdout),
        String::from_utf8_lossy(&add.stderr),
    );
    let session_id = parse_session_id(&String::from_utf8_lossy(&add.stdout));

    // The readopt pass must clear `attempted` and respawn/reattach so the
    // worker comes back. Allow extra slack over the happy path for the failed
    // first handshake plus the reconciler tick that recovers it.
    prompt_until_accepted(&h, &session_id, Duration::from_secs(45));
}
