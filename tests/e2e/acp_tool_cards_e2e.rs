//! Full-stack e2e: the native TUI structured view renders a per-kind Edit card
//! (a compact +/- line diff) instead of a generic one-liner. #1702.
//!
//! Per-kind dispatch is unit-tested at `src/tui/structured_view/render.rs`;
//! this proves the same end-to-end: a real `aoe serve --daemon`, a real
//! structured view worker bridging a scripted ACP `tool_call` (kind `edit`,
//! carrying `old_string`/`new_string` in `rawInput`), and the native TUI
//! attached over tmux rendering the diff through the production path.
//!
//! Mirrors the web Edit-card story `edit-card-diff-scroll.spec.ts`: the
//! fake-ACP `tool_call` shape (toolCallId / kind / status / rawInput) is
//! identical, so the TUI and the dashboard exercise the same wire data.
//!
//! Compiled only with the default `serve` feature (excluded by `--no-default-features`). Run via:
//!
//! ```sh
//! cargo test --test e2e -- acp_tool_cards
//! ```
#![cfg(feature = "serve")]

use std::time::{Duration, Instant};

use serial_test::serial;

use crate::harness::{pick_free_port, require_node, require_tmux, wait_for_port, TuiTestHarness};

/// One-turn fake-ACP script: emit a single completed `edit` tool call
/// whose `rawInput` carries the before/after text the structured view turns into
/// a diff. The turn ends immediately (no gating).
const EDIT_SCRIPT: &str = r#"{
  "turns": [
    {
      "updates": [
        {
          "sessionUpdate": "tool_call",
          "toolCallId": "tc-edit-1",
          "title": "edit greeting.txt",
          "kind": "edit",
          "status": "completed",
          "rawInput": {
            "file_path": "greeting.txt",
            "old_string": "hello from before",
            "new_string": "hello from after"
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

/// Retry `aoe acp prompt` until accepted. The prompt POST 404s while
/// the worker is still spawning / handshaking, so a successful call is
/// the readiness oracle for "worker live + ACP handshake done".
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

/// Stand up a live daemon, drive one scripted `edit` tool call, attach
/// the native TUI structured view, and assert the transcript shows the compact
/// added/removed line diff rather than the generic one-liner.
#[test]
#[serial]
fn tui_acp_renders_edit_diff_with_live_daemon() {
    require_tmux!();
    require_node!();

    // HOME under /tmp: structured view workers bind a unix socket under the app
    // dir, and a deep tempdir overflows the macOS sun_path limit.
    let mut h = TuiTestHarness::new_in_tmp("acp_tool_cards");

    // Shared Node fake-ACP agent, scripted to emit one completed edit.
    let script_path = h.home_path().join("edit-script.json");
    std::fs::write(&script_path, EDIT_SCRIPT).expect("write fake-acp script");
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
        "tool-cards",
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

    // Drive the scripted edit turn. This also gates worker readiness: the
    // prompt 404s until the worker is live and handshaked.
    prompt_until_accepted(&h, &session_id, Duration::from_secs(30));

    // Attach the native TUI structured view over tmux. Same HOME, so it
    // discovers the local daemon via serve.url / serve.pid.
    h.spawn(&["acp", "attach", &session_id]);

    // The edit card must surface through the full stack (replay + WS):
    // header + path + a removed line + an added line.
    h.wait_for("greeting.txt");
    h.assert_screen_contains("- hello from before");
    h.assert_screen_contains("+ hello from after");
}
