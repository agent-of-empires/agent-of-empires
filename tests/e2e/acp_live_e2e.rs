//! Structured sessions against a live daemon and the Node fake ACP agent
//! (`web/tests/helpers/fakeAcpAgent.mjs`), whose `permission_request` entries
//! gate the turn until the client decides.

use std::path::Path;
use std::time::{Duration, Instant};

use serial_test::parallel;

use crate::harness::{
    app_dir_in, init_git_repo, require_node, require_tmux, wait_until, TuiTestHarness,
};

const APPROVAL_SCRIPT: &str = r#"{
  "turns": [
    {
      "updates": [
        {
          "sessionUpdate": "permission_request",
          "toolCall": { "toolCallId": "approval-tool", "title": "Edit a file", "kind": "edit" }
        }
      ],
      "stopReason": "end_turn"
    }
  ]
}"#;

/// Start the fake agent with `script`, a daemon, and a structured session whose
/// worker has accepted `prompt`.
fn live_session(name: &str, script: &str, prompt: &str) -> (TuiTestHarness, String) {
    let h = TuiTestHarness::new_acp(name, script);
    let (_, session_id) = h.start_structured_session(name);
    h.prompt_until_accepted(&session_id, prompt, Duration::from_secs(30));
    (h, session_id)
}

#[test]
#[parallel]
fn tui_acp_renders_compact_edit_summary_with_live_daemon() {
    require_tmux!();
    require_node!();
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
    }
  ]
}"#;
    let (mut h, session_id) = live_session("acp_tool_cards", EDIT_SCRIPT, "please edit a file");
    h.spawn(&["acp", "attach", &session_id]);

    h.wait_for("greeting.txt");
    h.assert_screen_contains("+1 -1");
    assert!(
        !h.capture_screen().contains("hello from before"),
        "diff should be collapsed"
    );
}

/// A pending approval grabs focus on its own, ignores non-decision keys, and
/// `a` resolves it without leaking either key into the composer.
#[test]
#[parallel]
fn tui_acp_modal_approval_with_live_daemon() {
    require_tmux!();
    require_node!();
    let (mut h, session_id) =
        live_session("acp_focus_isolation", APPROVAL_SCRIPT, "please edit a file");
    h.spawn(&["acp", "attach", &session_id]);

    h.wait_for("Approval 1/1 · Edit a file");
    h.wait_for("a allow once");

    h.send_keys("z");
    h.assert_screen_not_contains("Allowed once · Edit a file");
    h.assert_screen_contains("Approval 1/1 · Edit a file");

    h.send_keys("a");
    h.wait_for_absent("Approval 1/1 · Edit a file", Duration::from_secs(10));
    h.assert_screen_contains("Message the agent");
}

/// Options that all share `allow_once` (pi's `ask_user_question`) make `a` open
/// a picker, and the agent receives the option the user lands on (#3741).
#[test]
#[parallel]
fn tui_acp_answers_an_option_list_approval_with_live_daemon() {
    require_tmux!();
    require_node!();
    // `echoDecision` echoes the received option id into the transcript.
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
    let (mut h, session_id) = live_session(
        "acp_option_list_approval",
        QUESTION_SCRIPT,
        "ask me something",
    );
    h.spawn(&["acp", "attach", &session_id]);

    h.wait_for("Approval 1/1 · Pick an option");
    h.wait_for("a answer");
    h.assert_screen_not_contains("A always");

    h.send_keys("a");
    h.wait_for("Option Alpha");
    h.assert_screen_contains("Option Delta");

    // Charlie, not the first option, which the old behavior sent.
    h.send_keys("j");
    h.send_keys("j");
    h.send_keys("Enter");

    h.wait_for("permission_option=choice-2");
    h.wait_for_absent("Approval 1/1 · Pick an option", Duration::from_secs(10));
}

/// `a` on a structured row of the home list opens the permission dialog and
/// resolves through ACP (#3544).
#[test]
#[parallel]
fn tui_home_resolves_structured_approval_with_live_daemon() {
    require_tmux!();
    require_node!();
    let (mut h, session_id) =
        live_session("acp_home_approval", APPROVAL_SCRIPT, "please edit a file");
    h.spawn_tui();
    h.wait_for(" aoe ");
    h.wait_for("acp_home_approval");

    // The approval reaches the home list via the 1 Hz daemon status poll, so an
    // early press shows "No Pending Approval"; dismiss and retry.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        h.send_keys("a");
        std::thread::sleep(Duration::from_millis(300));
        let screen = h.capture_screen();
        if screen.contains("Respond to Permission Prompt") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "home permission dialog never opened.\nlast screen:\n{screen}"
        );
        h.send_keys("Escape");
        std::thread::sleep(Duration::from_millis(400));
    }

    h.assert_screen_contains("Edit a file");
    h.assert_screen_not_contains("raw keystrokes");
    h.send_keys("a");

    // Only written after the supervisor resolves on the worker.
    wait_until(Duration::from_secs(20), Duration::from_millis(300), || {
        let out = h.run_cli(&["acp", "history", &session_id, "--json"]);
        let stdout = String::from_utf8_lossy(&out.stdout);
        if stdout.contains("ApprovalResolved") {
            Ok(())
        } else {
            Err(format!("no ApprovalResolved in history:\n{stdout}"))
        }
    });
}

/// Daemon session-scoped `acp.protocol` tracing is teed into the per-session
/// worker log alongside the runner marker (#1864).
#[test]
#[parallel]
fn daemon_breadcrumbs_reach_per_session_log() {
    require_tmux!();
    require_node!();
    let script = r#"{ "turns": [ { "updates": [], "stopReason": "end_turn" } ] }"#;
    let (h, session_id) = live_session("acp_session_log_tee", script, "hello");

    let body = wait_until(Duration::from_secs(10), Duration::from_millis(250), || {
        let out = h.run_cli(&["acp", "logs", "--session", &session_id]);
        let body = String::from_utf8_lossy(&out.stdout).to_string();
        if body.contains("initializing ACP agent") {
            Ok(body)
        } else {
            Err(format!("`aoe acp logs` lacks the handshake:\n{body}"))
        }
    });
    assert!(body.contains("runner.startup"), "runner marker:\n{body}");
    assert!(body.contains("acp.protocol"), "daemon target:\n{body}");
}

/// The daemon fails the first fresh-spawn handshake while the runner stays
/// registered; the reconciler must readopt it so prompts are accepted (#1890).
#[test]
#[parallel]
fn acp_recovers_orphaned_runner_after_failed_first_handshake() {
    require_tmux!();
    require_node!();
    let mut h = TuiTestHarness::new_acp(
        "acp_orphan_recovery",
        r#"{ "turns": [ { "updates": [], "stopReason": "end_turn" } ] }"#,
    );
    h.set_env("AOE_ACP_TEST_FAIL_FIRST_HANDSHAKES", "1");
    let (_, session_id) = h.start_structured_session("orphan-recovery");

    // 404 or 503 `worker_not_ready` are the expected transients; anything
    // else fails fast rather than timing out.
    wait_until(Duration::from_secs(45), Duration::from_millis(250), || {
        let out = h.run_cli(&["acp", "prompt", &session_id, "please proceed"]);
        if out.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("not found on the daemon") || stderr.contains("worker_not_ready"),
            "acp prompt failed with an unexpected error before recovery.\nstderr: {stderr}"
        );
        Err(format!("worker never recovered.\nstderr: {stderr}"))
    });
}

fn env_value(capture: &str, key: &str) -> Option<String> {
    capture
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{key}=")))
        .map(str::to_owned)
}

/// Wait for the adapter invocation that started with `key=expected`; other
/// shim invocations write their own captures and are skipped. Outlasts the
/// harness's 60s runner socket timeout.
fn wait_for_capture(dir: &Path, key: &str, expected: &str) -> String {
    wait_until(Duration::from_secs(75), Duration::from_millis(100), || {
        let seen = captures(dir);
        seen.iter()
            .find(|c| env_value(c, key).as_deref() == Some(expected))
            .cloned()
            .ok_or_else(|| {
                let observed: Vec<_> = seen.iter().map(|c| env_value(c, key)).collect();
                format!("no adapter started with {key}={expected}; observed {observed:?}")
            })
    })
}

fn captures(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .collect()
}

/// Configured `environment` entries and the desktop session layer reach a
/// structured adapter, read from the env the shim captured just before exec.
/// The daemon's ambient values are deliberately wrong so an allowlist
/// forward cannot pass, and `AOE_TOKEN` is always refused (#3262).
#[test]
#[parallel]
fn configured_host_environment_reaches_structured_worker() {
    require_tmux!();
    require_node!();
    let mut h = TuiTestHarness::new_in_tmp("acp_host_env");
    h.stop_daemon_on_drop();

    let home = h.home_path().to_path_buf();
    let expected_codex_home = home.join("configured-codex-home");
    let expected_git_config = home.join("configured-gitconfig");
    for (key, value) in [
        ("CODEX_HOME", home.join("wrong-ambient-codex-home")),
        ("GIT_CONFIG_GLOBAL", home.join("wrong-ambient-gitconfig")),
    ] {
        h.set_env(key, value.to_str().unwrap());
    }
    h.set_env("DISPLAY", ":42");
    h.set_env("XDG_RUNTIME_DIR", "/run/user/4242");
    // Stays out while `session.inherit_host_environment` is off.
    h.set_env("GOPATH", "/scratch/gopath");

    let config_path = app_dir_in(&home).join("config.toml");
    let seeded = std::fs::read_to_string(&config_path).expect("read seeded config");
    std::fs::write(
        &config_path,
        format!(
            "environment = [\n  \"CODEX_HOME={}\",\n  \"GIT_CONFIG_GLOBAL={}\",\n  \"AOE_TOKEN=must-not-reach-agent\",\n]\n\n{seeded}",
            expected_codex_home.display(),
            expected_git_config.display(),
        ),
    )
    .expect("write host environment config");

    let fake_script = home.join("host-env-script.json");
    std::fs::write(&fake_script, r#"{"turns":[]}"#).expect("write fake-acp script");
    let capture_dir = home.join("adapter-env");
    h.install_acp_shim_capturing_env(&fake_script, &capture_dir);
    // The reconciler spawns the adapter without a prompt.
    h.start_structured_session("HostEnv");

    let capture = wait_for_capture(
        &capture_dir,
        "CODEX_HOME",
        expected_codex_home.to_str().unwrap(),
    );

    for (key, expected) in [
        ("GIT_CONFIG_GLOBAL", expected_git_config.to_str().unwrap()),
        ("DISPLAY", ":42"),
        ("XDG_RUNTIME_DIR", "/run/user/4242"),
    ] {
        assert_eq!(env_value(&capture, key).as_deref(), Some(expected), "{key}");
    }
    assert_eq!(env_value(&capture, "GOPATH"), None);
    for capture in captures(&capture_dir) {
        assert_eq!(env_value(&capture, "AOE_TOKEN"), None);
    }
}

/// `host_hooks.before_session` output reaches a structured adapter and beats
/// both the daemon's ambient value and a same-keyed static `environment` entry,
/// the opposite precedence from `before_start`. `AOE_TOKEN` is still refused.
#[test]
#[parallel]
fn before_session_mints_environment_for_structured_worker() {
    require_tmux!();
    require_node!();
    let mut h = TuiTestHarness::new_in_tmp("before_session_env");
    h.stop_daemon_on_drop();

    let home = h.home_path().to_path_buf();
    let minted_codex_home = home.join("minted-codex-home");
    let minted_git_config = home.join("minted-gitconfig");
    let static_codex_home = home.join("static-codex-home");
    h.set_env(
        "CODEX_HOME",
        home.join("wrong-ambient-codex-home").to_str().unwrap(),
    );

    let config_path = app_dir_in(&home).join("config.toml");
    let seeded = std::fs::read_to_string(&config_path).expect("read seeded config");
    std::fs::write(
        &config_path,
        format!(
            "environment = [\n  \"CODEX_HOME={static_codex}\",\n]\n\n\
             {seeded}\n\n\
             [host_hooks]\n\
             before_session = [\n  \
               \"echo CODEX_HOME={minted_codex}\",\n  \
               \"echo GIT_CONFIG_GLOBAL={minted_git}\",\n  \
               \"echo AOE_TOKEN=must-not-reach-agent\",\n\
             ]\n",
            static_codex = static_codex_home.display(),
            minted_codex = minted_codex_home.display(),
            minted_git = minted_git_config.display(),
        ),
    )
    .expect("write before_session config");

    let fake_script = home.join("before-session-script.json");
    std::fs::write(&fake_script, r#"{"turns":[]}"#).expect("write fake-acp script");
    let capture_dir = home.join("adapter-env");
    h.install_acp_shim_capturing_env(&fake_script, &capture_dir);
    h.start_structured_session("BeforeSession");

    // Only a minting invocation carries GIT_CONFIG_GLOBAL, so waiting on it
    // keeps the CODEX_HOME comparison a real assertion.
    let capture = wait_for_capture(
        &capture_dir,
        "GIT_CONFIG_GLOBAL",
        minted_git_config.to_str().unwrap(),
    );
    assert_eq!(
        env_value(&capture, "CODEX_HOME").as_deref(),
        Some(minted_codex_home.to_str().unwrap()),
        "minted CODEX_HOME must win over static {} and ambient values",
        static_codex_home.display(),
    );
    for capture in captures(&capture_dir) {
        assert_eq!(env_value(&capture, "AOE_TOKEN"), None);
    }
}

/// Seeded config plus the wizard's Structured toggle, over a daemon.
fn structured_tui_harness(name: &str) -> TuiTestHarness {
    let script = r#"{
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
    let h = TuiTestHarness::new_acp(name, script);
    let config_path = app_dir_in(h.home_path()).join("config.toml");
    let seeded = std::fs::read_to_string(&config_path).expect("read seeded config");
    std::fs::write(
        &config_path,
        format!("{seeded}\n[acp]\noffer_structured_in_new_session = true\n"),
    )
    .expect("write config");
    init_git_repo(&h.project_path());
    h.start_daemon();
    h
}

/// A wizard-created structured session opens the embedded structured view
/// with no further input (#2926).
#[test]
#[parallel]
fn wizard_created_structured_session_opens_structured_view() {
    require_tmux!();
    require_node!();
    let mut h = structured_tui_harness("structured_wizard");
    let project = h.project_path();

    h.spawn_tui();
    h.wait_for(" aoe ");
    h.send_keys("n");
    h.wait_for(" New Session ");
    h.send_keys("C-u");
    h.type_text(project.to_str().unwrap());
    h.send_keys("Tab");
    h.type_text("wizstruct");
    // The Tool row is focusable only with several tools, shown by `[1/N]`.
    h.send_keys("Tab");
    if h.capture_screen().contains("[1/") {
        h.send_keys("Tab");
    }
    h.send_keys("Space");
    h.assert_screen_contains("[x] Structured view");
    h.send_keys("Enter");

    h.wait_for_timeout("Message the agent", Duration::from_secs(15));
    h.assert_screen_not_contains(" New Session ");
    h.assert_screen_contains("wizstruct");
    h.assert_screen_contains(" aoe ");
}

/// Confirming "Switch to structured" with `y` switches without waiting for
/// another keypress (#2925).
#[test]
#[parallel]
fn keyboard_confirmed_view_switch_fires_immediately() {
    require_tmux!();
    require_node!();
    let mut h = structured_tui_harness("structured_switch");
    let project = h.project_path();
    h.add_session(&[project.to_str().unwrap(), "-t", "switchme", "-c", "claude"]);

    h.spawn_tui();
    h.wait_for("switchme");
    // Sidebar rows start with `│`; the preview title also names the session.
    let screen = h.capture_screen();
    let row_idx = screen
        .lines()
        .position(|l| l.starts_with('│') && l.contains("switchme"))
        .expect("session row on screen") as u16
        + 1;
    h.send_mouse_click(2, 5, row_idx);
    h.wait_for("Switch to structured");
    h.send_keys("Up");
    h.send_keys("Enter");
    h.wait_for("Switch to structured view");

    h.send_keys("y");
    h.wait_for_timeout("switched to the structured view", Duration::from_secs(15));
    h.wait_for("[structured]");
}
