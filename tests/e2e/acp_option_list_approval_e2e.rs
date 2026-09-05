//! Full-stack e2e: the native TUI answers a permission request whose
//! options carry a question rather than an allow/deny vocabulary.
//!
//! pi's `ask_user_question` reaches aoe as a `session/request_permission`
//! with one `allow_once` option per answer, so `a` (allow once) has no
//! single meaning: it used to send whichever option came first. The TUI
//! now opens the option picker instead. Classification and picker
//! construction are unit-tested at `src/acp/approvals.rs` and
//! `src/tui/structured_view/mod.rs`; this proves it end-to-end through a
//! real daemon, a real ACP permission request, and the production input
//! loop. See #3741.
//!
//! The fake-ACP agent's `echoDecision` flag emits the option id it
//! received as a message chunk, so the assertion reads what actually
//! reached the agent rather than what the UI claimed to send.
//!
//! Run via:
//!
//! ```sh
//! cargo test --features e2e-tests --test e2e -- acp_option_list_approval
//! ```

use std::time::{Duration, Instant};

use serial_test::parallel;

use crate::harness::{pick_free_port, require_node, require_tmux, wait_for_port, TuiTestHarness};

/// One-turn fake-ACP script: a permission request offering four
/// same-kind options. The fake gates the turn on the decision and then
/// echoes the option id it was given.
const QUESTION_SCRIPT: &str = r#"{
  "turns": [
    {
      "updates": [
        {
          "sessionUpdate": "permission_request",
          "toolCall": {
            "toolCallId": "pi-ui-1",
            "title": "Pick an option",
            "kind": "other"
          },
          "options": [
            { "optionId": "choice-0", "name": "Option Alpha", "kind": "allow_once" },
            { "optionId": "choice-1", "name": "Option Bravo", "kind": "allow_once" },
            { "optionId": "choice-2", "name": "Option Charlie", "kind": "allow_once" },
            { "optionId": "choice-3", "name": "Option Delta", "kind": "allow_once" }
          ],
          "echoDecision": true
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
/// is the readiness oracle for "worker live + ACP handshake done".
fn prompt_until_accepted(h: &TuiTestHarness, session_id: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let out = h.run_cli(&["acp", "prompt", session_id, "ask me something"]);
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

/// Stand up a live daemon, drive one option-list permission request, and
/// answer it from the TUI: `a` opens the picker instead of allowing, and
/// the option the user lands on is the one the agent receives.
#[test]
#[parallel]
fn tui_acp_answers_an_option_list_approval_with_live_daemon() {
    require_tmux!();
    require_node!();

    // HOME under /tmp: structured view workers bind a unix socket under the app
    // dir, and a deep tempdir overflows the macOS sun_path limit.
    let mut h = TuiTestHarness::new_in_tmp("acp_option_list_approval");

    let script_path = h.home_path().join("question-script.json");
    std::fs::write(&script_path, QUESTION_SCRIPT).expect("write fake-acp script");
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
        "option-list",
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

    // The shelf offers answering, not allow-once: there is no single
    // option an allow-once decision could mean.
    h.wait_for("Approval 1/1 · Pick an option");
    h.wait_for("a answer");
    h.assert_screen_not_contains("A always");

    // `a` opens the picker with the agent's own labels.
    h.send_keys("a");
    h.wait_for("Option Alpha");
    h.assert_screen_contains("Option Delta");

    // Move to the third option and accept it. Picking the first would
    // reproduce the bug, so this asserts on Charlie specifically.
    h.send_keys("j");
    h.send_keys("j");
    h.send_keys("Enter");

    // What the AGENT received, echoed back into the transcript.
    h.wait_for("permission_option=choice-2");
    h.wait_for_absent("Approval 1/1 · Pick an option", Duration::from_secs(10));
}
