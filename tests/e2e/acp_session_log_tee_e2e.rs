//! Full-stack e2e: daemon session-scoped tracing is teed into the
//! per-session worker log (#1864).
//!
//! Before the fix, `aoe acp logs --session <id>` read only the per-worker
//! file, which received just the runner startup marker plus agent stderr.
//! The daemon's `acp.protocol` breadcrumbs (handshake, watchdog, cancel)
//! went to the shared debug.log, so the command was empty of anything
//! diagnostic. This proves a daemon-emitted, session-scoped `acp.protocol`
//! breadcrumb now lands in the per-session log surfaced by `aoe acp logs`.
//!
//! Compiled only with `--features serve`. Run via:
//!
//! ```sh
//! cargo test --test e2e --features serve -- acp_session_log_tee
//! ```
#![cfg(feature = "serve")]

use std::time::{Duration, Instant};

use serial_test::serial;

use crate::harness::{pick_free_port, require_node, require_tmux, wait_for_port, TuiTestHarness};

/// Minimal one-turn fake-ACP script: just end the turn. We only need the
/// worker to spawn and complete the ACP handshake, which is what makes the
/// daemon emit the session-scoped `acp.protocol` breadcrumbs we assert on.
const SCRIPT: &str = r#"{
  "turns": [
    { "updates": [], "stopReason": "end_turn" }
  ]
}"#;

fn parse_session_id(add_stdout: &str) -> String {
    add_stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("ID:"))
        .map(|rest| rest.trim().to_string())
        .unwrap_or_else(|| panic!("could not find session ID in `aoe add` output:\n{add_stdout}"))
}

/// `aoe acp prompt` 404s until the worker is live and handshaked, so a
/// successful call is the readiness oracle: by the time it returns, the
/// daemon has emitted the handshake breadcrumbs.
fn prompt_until_accepted(h: &TuiTestHarness, session_id: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let out = h.run_cli(&["acp", "prompt", session_id, "hello"]);
        if out.status.success() {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "worker never accepted a prompt within {timeout:?}.\nstdout: {}\nstderr: {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Poll `aoe acp logs --session <id>` until its output contains `needle`.
/// Returns the final output for further assertions, or panics on timeout.
fn logs_until_contains(
    h: &TuiTestHarness,
    session_id: &str,
    needle: &str,
    timeout: Duration,
) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        let out = h.run_cli(&["acp", "logs", "--session", session_id]);
        let body = String::from_utf8_lossy(&out.stdout).to_string();
        if body.contains(needle) {
            return body;
        }
        if Instant::now() >= deadline {
            panic!(
                "`aoe acp logs` never contained {needle:?} within {timeout:?}.\nlast output:\n{body}"
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[test]
#[serial]
fn daemon_breadcrumbs_reach_per_session_log() {
    require_tmux!();
    require_node!();

    let mut h = TuiTestHarness::new_in_tmp("acp_session_log_tee");

    let script_path = h.home_path().join("tee-script.json");
    std::fs::write(&script_path, SCRIPT).expect("write fake-acp script");
    h.install_acp_shim(&script_path);
    h.stop_daemon_on_drop();

    // A structured view session needs a git repo as its workspace.
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
        assert!(out.status.success(), "git {args:?} failed");
    }

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
        "daemon never bound port {port}"
    );

    let add = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "tee-log",
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

    // Worker spawns + handshakes; the daemon emits session-scoped
    // `acp.protocol` breadcrumbs during the handshake.
    prompt_until_accepted(&h, &session_id, Duration::from_secs(30));

    // The fix: a daemon-side breadcrumb (target `acp.protocol`, carrying the
    // session field) is teed into the per-session log. Before #1864 only the
    // runner startup marker landed here.
    let body = logs_until_contains(
        &h,
        &session_id,
        "initializing ACP agent",
        Duration::from_secs(10),
    );

    // Same file still carries the runner startup marker, confirming the tee
    // is additive rather than a redirect.
    assert!(
        body.contains("runner.startup"),
        "per-session log should still contain the runner startup marker:\n{body}"
    );
    // And it carries the daemon-side target, which previously only reached
    // the shared debug.log.
    assert!(
        body.contains("acp.protocol"),
        "per-session log should contain the daemon acp.protocol breadcrumb:\n{body}"
    );
}
