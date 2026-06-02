//! Full-stack e2e: TUI cockpit focus isolation against a live daemon.
//!
//! The cockpit focus model (composer swallows approval letters; approval
//! letters only resolve under approval focus) is unit-tested in isolation
//! at `src/tui/cockpit_view/input.rs`. This test proves the same guarantee
//! end-to-end: a real `aoe serve --daemon`, a real cockpit worker driving
//! a real ACP `session/request_permission`, the native TUI attached over
//! tmux, and the keystrokes routed through the production input loop.
//!
//! Determinism comes from the shared Node fake-ACP agent
//! (`web/tests/helpers/fakeAcpAgent.mjs`): its `permission_request` turn
//! entry emits a real ACP permission request and GATES the turn awaiting
//! the client's decision, so the approval stays pending until the TUI
//! resolves it.
//!
//! Compiled only with `--features serve` (cockpit + `aoe add --cockpit`
//! don't exist otherwise). Run via:
//!
//! ```sh
//! cargo test --test e2e --features serve -- cockpit_focus_isolation
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

/// Retry `aoe cockpit prompt` until it is accepted. The prompt POST 404s
/// while the worker is still spawning / handshaking, so a successful call
/// is the readiness oracle for "worker live + ACP handshake done". The
/// prompt enqueues a turn and returns immediately (it does not block on the
/// gated approval), so this loop terminates on the first accepted prompt.
fn prompt_until_accepted(h: &TuiTestHarness, session_id: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let out = h.run_cli(&["cockpit", "prompt", session_id, "please edit a file"]);
        if out.status.success() {
            return;
        }
        if Instant::now() >= deadline {
            let ps = h.run_cli(&["cockpit", "ps", "--json"]);
            panic!(
                "cockpit worker never accepted a prompt within {:?}.\n\
                 last prompt stdout: {}\n last prompt stderr: {}\n cockpit ps: {}",
                timeout,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
                String::from_utf8_lossy(&ps.stdout),
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Stand up a live daemon, attach the native TUI cockpit view to a session
/// with a real pending approval, and prove focus isolation:
///   1. With the composer focused, typing `a`/`A`/`d` does NOT resolve the
///      approval (the letters land in the composer textarea instead).
///   2. Moving focus to the approval card and pressing `a` DOES resolve it.
#[test]
#[serial]
fn tui_cockpit_focus_isolation_with_live_daemon() {
    require_tmux!();
    require_node!();

    // HOME under /tmp: cockpit workers bind a unix socket under the app
    // dir, and a deep tempdir overflows the macOS sun_path limit.
    let mut h = TuiTestHarness::new_in_tmp("cockpit_focus_isolation");

    // Cockpit master flag on at rest so the daemon's reconciler will spawn
    // the worker for the cockpit-mode session.
    h.enable_cockpit_master();

    // Shared Node fake-ACP agent, scripted to request one approval.
    let script_path = h.home_path().join("approval-script.json");
    std::fs::write(&script_path, APPROVAL_SCRIPT).expect("write fake-acp script");
    h.install_acp_shim(&script_path);

    // Tear down the worker + daemon on Drop so a panicking assertion can't
    // leak a daemon onto the test port between serial tests.
    h.stop_daemon_on_drop();

    // A cockpit session needs a git repo as its workspace; create one.
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

    // Create the cockpit session (daemon picks it up off disk; the
    // reconciler auto-spawns the worker since the master flag is on).
    let add = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "focus-isolation",
        "-c",
        "claude",
        "--cockpit",
    ]);
    assert!(
        add.status.success(),
        "aoe add --cockpit failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&add.stdout),
        String::from_utf8_lossy(&add.stderr),
    );
    let session_id = parse_session_id(&String::from_utf8_lossy(&add.stdout));

    // Drive a real pending approval. This also gates worker readiness:
    // the prompt 404s until the worker is live and handshaked.
    prompt_until_accepted(&h, &session_id, Duration::from_secs(30));

    // Attach the native TUI cockpit view over tmux. Same HOME, so it
    // discovers the local daemon via serve.url / serve.pid.
    h.spawn(&["cockpit", "attach", &session_id]);

    // The approval must surface through the full stack (replay + WS).
    h.wait_for(" pending approval");
    h.assert_screen_contains("press a / A / d to resolve");

    // --- Negative path: composer swallows approval letters ---
    // Default focus on attach is Transcript; `i` focuses the composer.
    h.send_keys("i");
    h.type_text("aAd");
    // If the letters render in the composer textarea, the input dispatcher
    // routed them to Intent::Compose, which mathematically proves none of
    // them resolved the approval (a resolve would have consumed the key as
    // ResolveApproval, so it would never reach the textarea). This is the
    // race-free oracle for "composer swallowed the approval letters".
    h.wait_for("aAd");
    h.assert_screen_not_contains("→ allowed");
    h.assert_screen_contains(" pending approval");

    // --- Positive path: resolve only under approval focus ---
    h.send_keys("Escape"); // composer -> transcript
    h.send_keys("Tab"); // transcript -> approval card (pending)
    h.send_keys("a"); // resolve: allow
    h.wait_for("→ allowed");
}
