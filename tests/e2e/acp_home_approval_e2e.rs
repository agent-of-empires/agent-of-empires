//! Two real Home TUIs share approvals through the owner-verified Unix API.
//! The fake ACP agent gates its turn until a client resolves the approval.
//! Requires tmux and Node; no dashboard build is needed.

use std::time::{Duration, Instant};

use serial_test::parallel;

use crate::harness::{require_node, require_tmux, TuiTestHarness};

/// One-turn fake-ACP script: emit a single permission request (which the fake
/// gates on) so the worker surfaces exactly one pending approval and holds it
/// until the client resolves it.
const APPROVAL_SCRIPT: &str = r#"{
  "turns": [
    {
      "updates": [
        {
          "sessionUpdate": "permission_request",
          "toolCall": {
            "toolCallId": "home-approval-tool",
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

/// Retry `aoe acp prompt` until it is accepted. The prompt POST 404s while the
/// worker is still spawning / handshaking, so a successful call is the
/// readiness oracle for "worker live + ACP handshake done". The prompt enqueues
/// a turn and returns immediately (it does not block on the gated approval).
fn prompt_until_accepted(h: &TuiTestHarness, session_id: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let out = h.run_cli(&["acp", "prompt", session_id, "please edit a file"]);
        if out.status.success() {
            return;
        }
        if Instant::now() >= deadline {
            let ps = h.run_cli(&["ps", "--acp", "--dead", "--json"]);
            panic!(
                "structured view worker never accepted a prompt within {:?}.\n\
                 last prompt stdout: {}\n last prompt stderr: {}\n ps --acp: {}",
                timeout,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
                String::from_utf8_lossy(&ps.stdout),
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Press `a` on the home list until the permission dialog opens, dismissing the
/// "No Pending Approval" info dialog between tries. The approval only lands in
/// `structured_pending_approvals` once the 1 Hz daemon-status poll has fetched
/// `/api/sessions` and seen the running worker's pending approval, so an early
/// press can miss it; this bounds that race without a fixed sleep.
fn open_approval_dialog(h: &TuiTestHarness, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        h.send_keys("a");
        std::thread::sleep(Duration::from_millis(300));
        let screen = h.capture_screen();
        if screen.contains("Respond to Permission Prompt") {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "home permission dialog never opened within {timeout:?}.\nlast screen:\n{screen}"
            );
        }
        // Clear a "No Pending Approval" info dialog (the map was not populated
        // yet) before the next attempt.
        h.send_keys("Escape");
        std::thread::sleep(Duration::from_millis(400));
    }
}

/// Poll `aoe acp history --json` until the durable log records an
/// `ApprovalResolved`. That event is only written after the supervisor resolves
/// the permission on the live worker, which unblocks the gated fake turn, so it
/// proves the resolve round-tripped the whole seam rather than only clearing
/// the TUI's optimistic local state.
fn wait_for_resolved(h: &TuiTestHarness, session_id: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let out = h.run_cli(&["acp", "history", session_id, "--json"]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        if stdout.contains("ApprovalResolved") {
            return;
        }
        if Instant::now() >= deadline {
            panic!(
                "session log never recorded an ApprovalResolved within {:?}.\nhistory stdout:\n{}\nstderr:\n{}",
                timeout,
                stdout,
                String::from_utf8_lossy(&out.stderr),
            );
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Stand up a live daemon with a structured session holding one pending
/// approval, attach the native home TUI over tmux, and resolve the approval
/// from the home list:
///   1. `a` opens the shared permission dialog, showing what is being approved
///      (the projected tool title), NOT the terminal keystroke guidance.
///   2. `a` in the dialog submits Allow, which resolves through ACP and lets
///      the gated fake turn complete (an `ApprovalResolved` lands in the log).
#[test]
#[parallel]
fn tui_home_resolves_structured_approval_with_live_daemon() {
    require_tmux!();
    require_node!();

    // HOME under /tmp: structured view workers bind a unix socket under the app
    // dir, and a deep tempdir overflows the macOS sun_path limit.
    let mut h = TuiTestHarness::new_in_tmp("acp_home_approval");

    // Shared Node fake-ACP agent, scripted to request one approval.
    let script_path = h.home_path().join("approval-script.json");
    std::fs::write(&script_path, APPROVAL_SCRIPT).expect("write fake-acp script");
    h.install_acp_shim(&script_path);

    // Tear down the worker + daemon on Drop so a panicking assertion can't leak
    // a daemon onto the test port between serial tests.
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

    let start = h.run_cli(&["serve", "--core-only", "--daemon"]);
    assert!(
        start.status.success(),
        "aoe serve --daemon failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr),
    );

    // Create the structured view session (daemon picks it up off disk; the
    // reconciler auto-spawns the worker since the master flag is on).
    let add = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "home-approval",
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

    // Drive a real pending approval. This also gates worker readiness: the
    // prompt 404s until the worker is live and handshaked.
    prompt_until_accepted(&h, &session_id, Duration::from_secs(30));

    // Both TUIs discover the same core daemon over its private Unix socket.
    h.spawn_tui();
    h.wait_for(" aoe ");
    h.wait_for("home-approval");
    h.spawn_peer_tui("approval-peer");
    let peer_deadline = Instant::now() + Duration::from_secs(20);
    loop {
        h.send_session_keys("approval-peer", "a");
        std::thread::sleep(Duration::from_millis(300));
        let screen = h.capture_session_screen("approval-peer");
        if screen.contains("Edit a file") && screen.contains("Respond to Permission Prompt") {
            break;
        }
        assert!(
            Instant::now() < peer_deadline,
            "peer did not receive approval:\n{screen}"
        );
        h.send_session_keys("approval-peer", "Escape");
    }
    h.send_session_keys("approval-peer", "Escape");

    // `a` on the home row opens the shared permission dialog once the poll has
    // surfaced the pending approval.
    open_approval_dialog(&h, Duration::from_secs(20));

    // The dialog names what is being approved (the projected tool title) and
    // drops the terminal-only "raw keystrokes" guidance, which is wrong on the
    // ACP path (there is no pane the user has looked at).
    h.assert_screen_contains("Edit a file");
    h.assert_screen_not_contains("raw keystrokes");

    // `a` submits Allow. The resolve round-trips daemon -> supervisor ->
    // worker, unblocking the gated fake turn.
    h.send_keys("a");

    // Proof the turn completed end to end: the durable log records the
    // resolution (only written after the supervisor resolves on the worker).
    wait_for_resolved(&h, &session_id, Duration::from_secs(20));
    let peer_deadline = Instant::now() + Duration::from_secs(20);
    loop {
        h.send_session_keys("approval-peer", "a");
        std::thread::sleep(Duration::from_millis(300));
        let screen = h.capture_session_screen("approval-peer");
        if screen.contains("No Pending Approval") {
            assert!(
                screen.contains("home-approval"),
                "peer lost the session:\n{screen}"
            );
            break;
        }
        assert!(
            Instant::now() < peer_deadline,
            "peer retained resolved approval:\n{screen}"
        );
        h.send_session_keys("approval-peer", "Escape");
    }
}
