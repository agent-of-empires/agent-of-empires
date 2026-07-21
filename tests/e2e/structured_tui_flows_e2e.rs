//! Full-stack e2e for the TUI structured-view entry flows (#1965):
//!
//! 1. Creating a session with the wizard's Structured toggle drops the
//!    user straight into the native structured view (#2926) instead of
//!    stranding them on the home list.
//! 2. Keyboard-confirming the context menu's "Switch to structured"
//!    (`y` on the confirm dialog) performs the switch immediately
//!    (#2925); before the fix the stashed switch only fired on the
//!    NEXT keypress, which read as "switching does nothing".
//!
//! Both stand up a real `aoe serve --daemon` and the shared Node
//! fake-ACP agent so the daemon can actually spawn a worker for the
//! structured session. Compiled only with the `serve` feature. Run:
//!
//! ```sh
//! cargo test --features e2e-tests --test e2e -- structured_tui_flows
//! ```
#![cfg(feature = "serve")]

use std::time::Duration;

use serial_test::serial;

use crate::harness::{pick_free_port, require_node, require_tmux, wait_for_port, TuiTestHarness};

/// Minimal fake-ACP script: one message turn, so a prompt (if any test
/// ever sends one) completes cleanly. Session creation itself only needs
/// the handshake, which the fake always answers.
const MESSAGE_SCRIPT: &str = r#"{
  "turns": [
    {
      "updates": [
        {
          "sessionUpdate": "agent_message_chunk",
          "content": { "type": "text", "text": "hello from the fake agent" }
        }
      ],
      "stopReason": "end_turn"
    }
  ]
}"#;

/// Seed the app config so no first-run dialog (welcome, telemetry
/// consent, agent-hooks install) overlays the wizard mid-test.
fn write_seeded_config(h: &TuiTestHarness) {
    let config_dir = crate::harness::app_dir_in(h.home_path());
    let config_content = format!(
        r#"[updates]
update_check_mode = "off"

[acp]
offer_structured_in_new_session = true

[app_state]
has_seen_welcome = true
has_responded_to_telemetry = true
last_seen_version = "{version}"
has_acknowledged_agent_hooks = true
"#,
        version = env!("CARGO_PKG_VERSION"),
    );
    std::fs::write(config_dir.join("config.toml"), config_content)
        .expect("write seeded config.toml");
}

/// Init the harness project dir as a git repo (structured sessions use
/// it as their workspace root).
fn init_git_repo(path: &std::path::Path) {
    for args in [
        vec!["init", "-q"],
        vec!["commit", "--allow-empty", "-q", "-m", "init"],
    ] {
        let out = std::process::Command::new("git")
            .args(&args)
            .current_dir(path)
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
}

/// Start a daemon on a free port and wait for it to bind.
fn start_daemon(h: &TuiTestHarness) {
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
}

/// Wizard-created structured session must open the native structured
/// view without any further input (#2926). The load-bearing tell is the
/// structured view chrome (composer hint) replacing the home screen.
#[test]
#[serial]
fn wizard_created_structured_session_opens_structured_view() {
    require_tmux!();
    require_node!();

    let mut h = TuiTestHarness::new_in_tmp("structured_wizard");
    write_seeded_config(&h);
    let script_path = h.home_path().join("fake-acp-script.json");
    std::fs::write(&script_path, MESSAGE_SCRIPT).expect("write fake-acp script");
    h.install_acp_shim(&script_path);
    h.stop_daemon_on_drop();

    let project = h.project_path();
    init_git_repo(&project);
    start_daemon(&h);

    h.spawn_tui();
    h.wait_for(" aoe ");

    h.send_keys("n");
    h.wait_for(" New Session ");
    // Path is the default focused field; replace its prefill.
    h.send_keys("C-u");
    h.type_text(project.to_str().unwrap());
    h.send_keys("Tab"); // -> Title
    h.type_text("wizstruct");
    // The Tool row is only in the Tab order when more than one tool is
    // available (`available_tools.len() > 1`); a lone stubbed `claude`
    // renders it read-only and Tab skips it. Detect which layout this
    // environment produced from the cycler chrome (`[1/N]` only renders
    // in the multi-tool form) instead of hard-coding the Tab count,
    // which is exactly what broke on CI's single-tool runner.
    h.send_keys("Tab");
    if h.capture_screen().contains("[1/") {
        h.send_keys("Tab"); // multi-tool: hop over the Tool row
    }
    h.send_keys("Space"); // -> Structured toggle
    h.assert_screen_contains("[x] Structured view");
    h.send_keys("Enter");

    // The structured view must open embedded in the preview pane:
    // composer chrome on screen, wizard gone, and the sidebar still
    // visible around it (live-mode style, not a full-screen takeover).
    // Generous timeout; creation runs hooks + storage and the view
    // only opens after the create result lands.
    h.wait_for_timeout("Message the agent", Duration::from_secs(15));
    h.assert_screen_not_contains(" New Session ");
    h.assert_screen_contains("wizstruct");
    h.assert_screen_contains(" aoe ");
}

/// Keyboard-confirmed view switch ('y' on the confirm) must fire
/// without waiting for another keypress (#2925). The test sends `y`
/// and then ONLY waits: the "switched to the structured view" toast
/// can only appear if the drain runs as part of handling that same
/// key event.
#[test]
#[serial]
fn keyboard_confirmed_view_switch_fires_immediately() {
    require_tmux!();
    require_node!();

    let mut h = TuiTestHarness::new_in_tmp("structured_switch");
    write_seeded_config(&h);
    let script_path = h.home_path().join("fake-acp-script.json");
    std::fs::write(&script_path, MESSAGE_SCRIPT).expect("write fake-acp script");
    h.install_acp_shim(&script_path);
    h.stop_daemon_on_drop();

    let project = h.project_path();
    init_git_repo(&project);
    start_daemon(&h);

    // A plain terminal claude session; the switch flips it to structured.
    let add = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "switchme",
        "-c",
        "claude",
    ]);
    assert!(
        add.status.success(),
        "aoe add failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&add.stdout),
        String::from_utf8_lossy(&add.stderr),
    );

    h.spawn_tui();
    h.wait_for("switchme");

    // Right-click the session row to open the context menu. The row's
    // 1-indexed terminal position is derived from the captured screen so
    // grouping/header layout changes don't silently break the click.
    // The preview panel title also carries the session name, so anchor
    // on sidebar rows only (they start with the box-drawing `│`).
    let screen = h.capture_screen();
    let row_idx = screen
        .lines()
        .position(|l| l.starts_with('│') && l.contains("switchme"))
        .expect("session row on screen") as u16
        + 1;
    h.send_mouse_click(2, 5, row_idx);
    h.wait_for("Switch to structured");

    // Highlight the last menu item ("Switch to structured") and submit.
    h.send_keys("Up");
    h.send_keys("Enter");
    h.wait_for("Switch to structured view");

    // Confirm with 'y' and then ONLY wait. Before the #2925 fix the
    // stashed switch sat until the next keypress, so this wait_for
    // timed out.
    h.send_keys("y");
    h.wait_for_timeout("switched to the structured view", Duration::from_secs(15));
    h.wait_for("[structured]");
}
