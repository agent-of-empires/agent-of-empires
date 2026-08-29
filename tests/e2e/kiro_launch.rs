//! End-to-end coverage for how a Kiro session is launched.
//!
//! Kiro's interactive flags (`--trust-all-tools`, `--agent`)
//! live on the `kiro-cli chat` subcommand, not the top-level binary. AoE used
//! to launch bare `kiro-cli` plus the yolo flag, so YOLO mode produced
//! `kiro-cli --trust-all-tools`, which the real CLI rejects with
//! `error: unexpected argument '--trust-all-tools' found`.
//!
//! These tests drive the full `aoe add --launch` path and assert on the command
//! the launch actually executed, so a regression in launch-command construction
//! is caught at the session-launch layer, not just in the `build_host_command`
//! unit tests. Launches run the pane command through an ephemeral env-file
//! wrapper (`exec <shell> <file>`) that keeps the command out of tmux's
//! `pane_start_command` argv, so we install a recording `kiro-cli` stub and read
//! the argv it was actually invoked with: a more faithful check than the old
//! `pane_start_command` read, since it observes the command as executed.
//!
//! A separate test covers the other half of `--agent` support: that AoE's
//! status hooks are installed into the agent config Kiro actually loads. Kiro
//! resolves `--agent NAME` by the `name` field inside `~/.kiro/agents/*.json`,
//! not the filename, so a generator-managed agent stored as
//! `<prefix>-NAME.json` must still receive the hooks. This drives the full
//! launch path against a seeded agents dir and asserts the on-disk result.

use crate::harness::{require_tmux, TuiTestHarness};
use serde_json::Value;
use serial_test::parallel;
use std::process::Command;

/// Kills its tmux session when dropped, so a panicking assertion in the test
/// body still tears the real session down. Holds the socket aoe used
/// (`AOE_TMUX_SOCKET`, #2608) so the kill targets the right server.
struct TmuxSessionGuard {
    socket: std::path::PathBuf,
    name: String,
}

impl Drop for TmuxSessionGuard {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .arg("-S")
            .arg(&self.socket)
            .args(["kill-session", "-t", &self.name])
            .output();
    }
}

/// The tmux session name aoe derives for the session titled `title`
/// (`<SESSION_PREFIX><title>_<id[..8]>`). Looks the session up by title rather
/// than assuming a position, and panics with a clear message if it is absent,
/// so a launch that never persisted a session fails here rather than as a
/// downstream tmux lookup miss.
fn launched_tmux_name(h: &TuiTestHarness, title: &str) -> String {
    let path = crate::harness::app_dir_in(h.home_path())
        .join("profiles")
        .join("default")
        .join("sessions.json");
    let sessions: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| panic!("no sessions.json at {} after launch", path.display()));
    let id = sessions
        .as_array()
        .and_then(|arr| arr.iter().find(|s| s["title"].as_str() == Some(title)))
        .and_then(|s| s["id"].as_str())
        .unwrap_or_else(|| panic!("no session titled '{title}' in {}", path.display()));
    let truncated = &id[..8.min(id.len())];
    format!(
        "{}{}_{}",
        agent_of_empires::tmux::SESSION_PREFIX,
        title,
        truncated
    )
}

/// Run `aoe add --launch ...` for a kiro session and return the command tmux
/// was told to run. `--launch` starts the tmux session and, since the test
/// harness's `run_cli` has no controlling terminal, skips the interactive
/// attach step instead of failing because of it; the exit status IS asserted
/// here to cover that behavior. The session (and its recorded pane command)
/// is created regardless, and `launched_tmux_name` fails loudly if it
/// wasn't. The returned guard kills the session when the caller's scope
/// ends, including on assertion panic.
fn launch_kiro_and_read_command(
    h: &mut TuiTestHarness,
    title: &str,
    extra: &[&str],
) -> (String, TmuxSessionGuard) {
    // `aoe add --tool kiro` verifies `kiro-cli` is on PATH before persisting the
    // session, so without a stub it bails (and never writes sessions.json) in
    // CI / any machine without kiro-cli installed. A recording stub both lets
    // `add` proceed AND captures the exact argv the launch actually executed:
    // launches now run through an ephemeral env-file wrapper (`exec zsh <file>`)
    // that keeps the command out of tmux's `pane_start_command`, so the stub's
    // recorded argv is the only faithful observation of the launch command.
    let argv_file = h.install_recording_path_command("kiro-cli");

    let project = h.project_path();
    let mut args = vec![
        "add",
        project.to_str().unwrap(),
        "-t",
        title,
        "--tool",
        "kiro",
        "--launch",
    ];
    args.extend_from_slice(extra);
    let output = h.run_cli(&args);
    assert!(
        output.status.success(),
        "aoe add --launch should succeed without a controlling terminal: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let session = launched_tmux_name(h, title);
    let socket = h.home_path().join("tmux.sock");
    let guard = TmuxSessionGuard {
        socket,
        name: session,
    };
    // `wait_until_consumed` returns once the pane `rm`s the env file, which is
    // BEFORE it execs kiro-cli, so poll for the stub to record its argv.
    let cmd = wait_for_recorded_argv(&argv_file, "kiro-cli chat");
    (cmd, guard)
}

/// Poll (up to 10s) for the recording stub to write the argv the launch ran it
/// with. Returns the recorded command line.
fn wait_for_recorded_argv(path: &std::path::Path, expected: &str) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Ok(content) = std::fs::read_to_string(path) {
            // Hook installation invokes the same stub with "agent set-default"
            // before the pane launches. Wait for the interactive invocation.
            if content.contains(expected) {
                return content.trim().to_string();
            }
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "agent stub never recorded {expected:?} at {} (launch did not exec it)",
                path.display()
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[test]
#[parallel]
fn test_qwen_and_kiro_stored_ids_stay_inert_on_cli_start() {
    require_tmux!();

    const STORED_SID: &str = "019342ab-1234-7def-8901-111111111111";
    const PINNED_SID: &str = "019342ab-1234-7def-8901-222222222222";
    let cases = [
        ("qwen_inert_ids", "QwenInertIds", "qwen", "qwen", "qwen"),
        (
            "kiro_inert_ids",
            "KiroInertIds",
            "kiro",
            "kiro-cli",
            "kiro-cli chat",
        ),
    ];

    for (harness_name, title, tool, binary, expected_argv) in cases {
        let mut h = TuiTestHarness::new(harness_name);
        let argv_file = h.install_recording_path_command(binary);
        let project = h.project_path();
        let add = h.run_cli(&[
            "add",
            project.to_str().unwrap(),
            "-t",
            title,
            "--tool",
            tool,
        ]);
        assert!(
            add.status.success(),
            "{tool} add failed: {}",
            String::from_utf8_lossy(&add.stderr)
        );

        let sessions_path =
            crate::harness::app_dir_in(h.home_path()).join("profiles/default/sessions.json");
        let mut sessions: Value = serde_json::from_str(
            &std::fs::read_to_string(&sessions_path).expect("read sessions before launch"),
        )
        .expect("parse sessions before launch");
        let row = sessions
            .as_array_mut()
            .and_then(|rows| rows.iter_mut().find(|row| row["title"] == title))
            .expect("find unsupported session row");
        let instance_id = row["id"].as_str().expect("session id").to_string();
        row["agent_session_id"] = Value::String(STORED_SID.to_string());
        row["resume_intent"] = serde_json::json!({ "kind": "Use", "value": PINNED_SID });
        std::fs::write(
            &sessions_path,
            serde_json::to_vec_pretty(&sessions).expect("serialize seeded sessions"),
        )
        .expect("seed stored session IDs");

        let socket = h.home_path().join("tmux.sock");
        let tmux_name = launched_tmux_name(&h, title);
        let _guard = TmuxSessionGuard {
            socket: socket.clone(),
            name: tmux_name.clone(),
        };
        for attempt in ["initial start", "stopped relaunch"] {
            if attempt != "initial start" {
                let stop = h.run_cli(&["session", "stop", title]);
                assert!(stop.status.success(), "{tool} stop failed");
            }
            let _ = std::fs::remove_file(&argv_file);
            let start = h.run_cli(&["session", "start", title]);
            assert!(
                start.status.success(),
                "{tool} {attempt} failed: {}",
                String::from_utf8_lossy(&start.stderr)
            );
            let owner = Command::new("tmux")
                .arg("-S")
                .arg(&socket)
                .args([
                    "show-environment",
                    "-h",
                    "-t",
                    &tmux_name,
                    "AOE_INSTANCE_ID",
                ])
                .output()
                .expect("read hidden owner");
            assert!(owner.status.success(), "{tool} owner read failed");
            assert_eq!(
                String::from_utf8_lossy(&owner.stdout).trim(),
                format!("AOE_INSTANCE_ID={instance_id}"),
                "{tool} agent pane must carry its full owner ID"
            );
            let argv = wait_for_recorded_argv(&argv_file, expected_argv);
            for forbidden in [
                STORED_SID,
                PINNED_SID,
                "--resume",
                "--resume-id",
                "--session-id",
            ] {
                assert!(
                    !argv.contains(forbidden),
                    "{tool} {attempt} operationalized stored state via {forbidden:?}: {argv:?}"
                );
            }
        }

        let sessions: Value = serde_json::from_str(
            &std::fs::read_to_string(&sessions_path).expect("read sessions after launch"),
        )
        .expect("parse sessions after launch");
        let row = sessions
            .as_array()
            .and_then(|rows| rows.iter().find(|row| row["title"] == title))
            .expect("find relaunched unsupported session row");
        assert_eq!(row["agent_session_id"].as_str(), Some(STORED_SID), "{tool}");
        assert_eq!(
            row["resume_intent"]["value"].as_str(),
            Some(PINNED_SID),
            "{tool}"
        );
    }
}
#[test]
#[parallel]
fn test_kiro_launches_via_chat_subcommand() {
    require_tmux!();

    let mut h = TuiTestHarness::new("kiro_launch_chat");
    let (cmd, _guard) = launch_kiro_and_read_command(&mut h, "KiroChat", &[]);

    assert!(
        cmd.contains("kiro-cli chat"),
        "kiro must launch via `kiro-cli chat`, got: {cmd:?}"
    );
}

#[test]
#[parallel]
fn test_kiro_yolo_passes_trust_all_tools_after_chat() {
    require_tmux!();

    let mut h = TuiTestHarness::new("kiro_launch_yolo");
    let (cmd, _guard) = launch_kiro_and_read_command(&mut h, "KiroYolo", &["--yolo"]);

    // The fix: YOLO mode must produce a parseable command. `kiro-cli chat` must
    // appear and `--trust-all-tools` must follow it; bare
    // `kiro-cli --trust-all-tools` is what the CLI rejected.
    let chat = cmd
        .find("kiro-cli chat")
        .unwrap_or_else(|| panic!("`kiro-cli chat` not in launch command: {cmd:?}"));
    let yolo = cmd
        .find("--trust-all-tools")
        .unwrap_or_else(|| panic!("`--trust-all-tools` not in launch command: {cmd:?}"));
    assert!(
        yolo > chat,
        "--trust-all-tools must come after `kiro-cli chat`, got: {cmd:?}"
    );
}

/// `--agent NAME` must install AoE's status hooks into the config file Kiro
/// actually loads. Kiro resolves the agent by the `name` field inside each
/// `~/.kiro/agents/*.json`, not the filename, and generator-managed agents are
/// stored as `<prefix>-NAME.json`. This seeds such a file under the harness's
/// isolated `$HOME`, launches a kiro session selecting it, and asserts the hooks
/// merged into that prefixed file (preserving its own hook) rather than a
/// `NAME.json` clone the CLI never reads.
#[test]
#[parallel]
fn test_kiro_agent_hooks_install_into_name_matched_file() {
    require_tmux!();

    let mut h = TuiTestHarness::new("kiro_agent_hooks");

    // Seed a generator-managed agent whose filename stem differs from its
    // logical `name`. Its only hook is the generator's own agentSpawn: AoE's
    // three events are absent, so finding them post-launch proves the install
    // ran against this file (not stale state) and that agentSpawn is preserved.
    let agents_dir = h.home_path().join(".kiro").join("agents");
    std::fs::create_dir_all(&agents_dir).expect("create .kiro/agents");
    let managed = agents_dir.join("TeamAgents-custom-agent.json");
    std::fs::write(
        &managed,
        r#"{"name":"custom-agent","hooks":{"agentSpawn":[{"command":"team-tool emit"}]}}"#,
    )
    .expect("seed managed agent file");

    // Guard kills the tmux session on scope exit; the launch command itself is
    // covered by the sibling tests, so only the on-disk result matters here.
    let _guard = launch_kiro_and_read_command(
        &mut h,
        "KiroAgentHooks",
        &["--extra-args", "--agent custom-agent"],
    )
    .1;

    let installed: Value = serde_json::from_str(
        &std::fs::read_to_string(&managed).expect("managed agent file still present"),
    )
    .expect("managed agent file is valid JSON");
    let hooks = installed["hooks"]
        .as_object()
        .expect("hooks object present after install");
    for event in ["preToolUse", "userPromptSubmit", "stop"] {
        assert!(
            hooks.contains_key(event),
            "AoE status hook '{event}' must be installed into the name-matched file, got: {:?}",
            hooks.keys().collect::<Vec<_>>()
        );
    }
    assert!(
        hooks.contains_key("agentSpawn"),
        "the agent's own agentSpawn hook must be preserved"
    );
    assert_eq!(
        installed["name"].as_str(),
        Some("custom-agent"),
        "the agent's name field must be left intact"
    );

    // And NOT into a filename-stem clone the CLI would never load.
    assert!(
        !agents_dir.join("custom-agent.json").exists(),
        "must not create a `custom-agent.json` clone derived from the filename stem"
    );
}
