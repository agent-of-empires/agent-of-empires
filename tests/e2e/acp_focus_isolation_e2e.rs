//! Full-stack e2e: TUI structured view modal approval against a live daemon.
//!
//! The structured view presents a pending approval like a native agent's
//! permission prompt: it grabs focus on its own (no Tab into the chat), only
//! the decision keys act, and `a`/`A`/`d` resolve it in place. That model is
//! unit-tested at `src/tui/structured_view/input.rs`; this test proves it
//! end-to-end: a real `aoe serve --daemon`, a real structured view worker
//! driving a real ACP `session/request_permission`, the native TUI attached
//! over tmux, and the keystrokes routed through the production input loop.
//!
//! Determinism comes from the shared Node fake-ACP agent
//! (`web/tests/helpers/fakeAcpAgent.mjs`): its `permission_request` turn
//! entry emits a real ACP permission request and GATES the turn awaiting
//! the client's decision, so the approval stays pending until the TUI
//! resolves it.
//!
//! Compiled only with the default `serve` feature (excluded by `--no-default-features`) (structured view + `aoe add --structured-view`
//! don't exist otherwise). Run via:
//!
//! ```sh
//! cargo test --features e2e-tests --test e2e -- acp_focus_isolation
//! ```
#![cfg(feature = "serve")]

use std::time::{Duration, Instant};

use serial_test::serial;

use crate::harness::{pick_free_port, require_node, require_tmux, wait_for_port, TuiTestHarness};

/// One-turn fake-ACP script: emit a single permission request (which the
/// fake gates on) so the worker surfaces exactly one pending approval and
/// holds it until the TUI resolves it.
const APPROVAL_SCRIPT: &str = r#"{
  "turns": [
    {
      "updates": [
        {
          "sessionUpdate": "permission_request",
          "toolCall": {
            "toolCallId": "focus-isolation-tool",
            "title": "Edit a file",
            "kind": "edit"
          }
        }
      ],
      "stopReason": "end_turn"
    }
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

/// Retry `aoe acp prompt` until it is accepted. The prompt POST 404s
/// while the worker is still spawning / handshaking, so a successful call
/// is the readiness oracle for "worker live + ACP handshake done". The
/// prompt enqueues a turn and returns immediately (it does not block on the
/// gated approval), so this loop terminates on the first accepted prompt.
fn prompt_until_accepted(h: &TuiTestHarness, session_id: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let out = h.run_cli(&["acp", "prompt", session_id, "please edit a file"]);
        if out.status.success() {
            return;
        }
        if Instant::now() >= deadline {
            let ps = h.run_cli(&["acp", "ps", "--json"]);
            panic!(
                "structured view worker never accepted a prompt within {:?}.\n\
                 last prompt stdout: {}\n last prompt stderr: {}\n acp ps: {}",
                timeout,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
                String::from_utf8_lossy(&ps.stdout),
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Stand up a live daemon, attach the native TUI structured view to a session
/// with a real pending approval, and prove the modal-approval model:
///   1. A pending approval grabs focus on its own (no Tab): the card shows
///      the decision hint immediately.
///   2. A non-decision key does NOT resolve it (only a/A/d act).
///   3. Pressing `a` resolves it, with no focus switch anywhere.
#[test]
#[serial]
fn tui_acp_modal_approval_with_live_daemon() {
    require_tmux!();
    require_node!();

    // HOME under /tmp: structured view workers bind a unix socket under the app
    // dir, and a deep tempdir overflows the macOS sun_path limit.
    let mut h = TuiTestHarness::new_in_tmp("acp_focus_isolation");

    // Shared Node fake-ACP agent, scripted to request one approval.
    let script_path = h.home_path().join("approval-script.json");
    std::fs::write(&script_path, APPROVAL_SCRIPT).expect("write fake-acp script");
    h.install_acp_shim(&script_path);

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

    // Create the structured view session (daemon picks it up off disk; the
    // reconciler auto-spawns the worker since the master flag is on).
    let add = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "focus-isolation",
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

    // Drive a real pending approval. This also gates worker readiness:
    // the prompt 404s until the worker is live and handshaked.
    prompt_until_accepted(&h, &session_id, Duration::from_secs(30));

    // Attach the native TUI structured view over tmux. Same HOME, so it
    // discovers the local daemon via serve.url / serve.pid.
    h.spawn(&["acp", "attach", &session_id]);

    // The approval must surface through the full stack (replay + WS).
    // It is modal: it grabs focus on its own, so the decision hint shows
    // with no Tab or other gesture.
    h.wait_for(" pending approval");
    h.wait_for("press a / A / d to resolve");

    // --- Negative path: a non-decision key does not resolve ---
    // Only a/A/d act while the prompt is up; a stray letter is ignored
    // (the modal approval owns the keyboard, so it can't leak into a
    // composer draft either).
    h.send_keys("z");
    h.assert_screen_not_contains("→ allowed");
    h.assert_screen_contains(" pending approval");

    // --- Positive path: `a` resolves it directly, no focus switch ---
    h.send_keys("a"); // resolve: allow
    h.wait_for("→ allowed");
    // Neither the ignored `z` nor the resolving `a` may leak into the
    // composer: the empty-composer placeholder proves the modal owned
    // both keys end to end.
    h.assert_screen_contains("Message the agent");
}
