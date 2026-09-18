//! Native structured rendering consumes replay and then a live Unix WebSocket event.
//! The daemon is core-only, so no TCP or dashboard fallback can satisfy the test.

use std::time::{Duration, Instant};

use serial_test::parallel;

use crate::harness::{require_node, require_tmux, TuiTestHarness};

/// Two edits distinguish the initial replay from a subsequent live event.
const EDIT_SCRIPT: &str = r#"{
  "turns": [
    {
      "updates": [
        {
          "sessionUpdate": "tool_call",
          "toolCallId": "tc-edit-1",
          "title": "edit greeting.txt",
          "kind": "edit",
          "status": "pending",
          "rawInput": {
            "file_path": "greeting.txt",
            "old_string": "hello from before",
            "new_string": "hello from after"
          }
        },
        {
          "sessionUpdate": "tool_call_update",
          "toolCallId": "tc-edit-1",
          "status": "completed",
          "rawOutput": { "content": "updated greeting.txt" }
        }
      ],
      "stopReason": "end_turn"
    },
    {
      "updates": [
        {
          "sessionUpdate": "tool_call",
          "toolCallId": "tc-edit-live",
          "title": "edit live-followup.txt",
          "kind": "edit",
          "status": "pending",
          "rawInput": {
            "file_path": "live-followup.txt",
            "old_string": "before",
            "new_string": "after"
          }
        },
        {
          "sessionUpdate": "tool_call_update",
          "toolCallId": "tc-edit-live",
          "status": "completed",
          "rawOutput": { "content": "updated live-followup.txt" }
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

/// Stand up a live daemon, drive one scripted `edit` tool call, attach
/// the native TUI structured view, and assert the transcript shows the compact
/// target and change counts rather than the expanded diff.
#[test]
#[parallel]
fn tui_acp_renders_compact_edit_summary_with_live_daemon() {
    require_tmux!();
    require_node!();

    // HOME under /tmp: structured view workers bind a unix socket under the app
    // dir, and a deep tempdir overflows the macOS sun_path limit.
    let mut h = TuiTestHarness::new_in_tmp("acp_tool_cards");

    // Shared Node fake-ACP agent, scripted to emit one completed edit.
    let script_path = h.home_path().join("edit-script.json");
    std::fs::write(&script_path, EDIT_SCRIPT).expect("write fake-acp script");
    h.install_acp_shim(&script_path);

    h.stop_daemon_on_drop();

    // A structured view session needs a git repo as its workspace; create one.
    let project = h.project_path();
    for args in [
        vec!["init", "-q"],
        vec!["commit", "--allow-empty", "-q", "-m", "init"],
    ] {
        let mut git = std::process::Command::new("git");
        git.current_dir(&project)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t");
        if args[0] == "commit" {
            git.arg("-c").arg("commit.gpgsign=false");
        }
        git.args(&args);
        let out = git.output().expect("run git");
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

    // Native attach discovers the owner-verified Unix API.
    h.spawn(&["acp", "attach", &session_id]);

    // The edit summary must surface through the full stack (replay + WS):
    // tool label, target path, and added/removed counts.
    h.wait_for("greeting.txt");
    h.assert_screen_contains("+1 -1");
    let screen = h.capture_screen();
    assert!(
        !screen.contains("hello from before"),
        "diff should be collapsed"
    );
    let followup = h.run_cli(&["acp", "prompt", &session_id, "live followup"]);
    assert!(
        followup.status.success(),
        "live prompt failed: {}",
        String::from_utf8_lossy(&followup.stderr)
    );
    h.wait_for("live-followup.txt");
}
