//! `aoe session wait` blocks on an activity transition and fails when the session goes away.

use std::time::Duration;

use serde_json::Value;
use serial_test::parallel;

use crate::harness::{require_tmux, wait_until, write_executable, TuiTestHarness};

#[test]
#[parallel]
fn test_wait_times_out_then_reports_stopped_session() {
    require_tmux!();
    let mut h = TuiTestHarness::new("session_wait");
    let bin = h.install_path_command("claude");
    std::fs::write(bin.join("claude"), "#!/bin/sh\nexec sleep 600\n").expect("write claude stub");
    let project = h.project_path();
    h.add_session(&[project.to_str().unwrap(), "-t", "waiter"]);
    h.run_cli_ok(&["session", "start", "waiter"]);

    let out = h.run_cli(&[
        "session",
        "wait",
        "waiter",
        "--until",
        "waiting",
        "--timeout",
        "1500",
    ]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(124), "stderr: {err}");
    assert!(err.contains("timed out after 1500ms"), "stderr: {err}");

    h.run_cli_ok(&["session", "stop", "waiter"]);
    let err = h.run_cli_err(&[
        "session",
        "wait",
        "waiter",
        "--until",
        "idle",
        "--timeout",
        "1500",
    ]);
    assert!(err.contains("is not running"), "stderr: {err}");
}

/// A pane that disappears mid-wait reads as `Error`; that must not satisfy `--until error`.
#[test]
#[parallel]
fn test_wait_until_error_fails_when_session_is_killed_mid_wait() {
    require_tmux!();
    let mut h = TuiTestHarness::new("session_wait_stop");
    let bin = h.install_path_command("claude");
    std::fs::write(bin.join("claude"), "#!/bin/sh\nexec sleep 600\n").expect("write claude stub");
    let project = h.project_path();
    h.add_session(&[project.to_str().unwrap(), "-t", "stopper"]);
    h.run_cli_ok(&["session", "start", "stopper"]);

    let waiter = h.spawn_cli(&[
        "session",
        "wait",
        "stopper",
        "--until",
        "error",
        "--timeout",
        "30000",
    ]);
    // Kill the pane without `aoe session stop`, which would store `Stopped` and hide the gone-pane
    // path whichever side of the waiter's first poll it lands on.
    let names = h
        .tmux()
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
        .expect("list tmux sessions");
    let names = String::from_utf8_lossy(&names.stdout);
    let pane = names
        .lines()
        .find(|n| n.contains("stopper"))
        .expect("agent tmux session");
    // Killing the last session exits the server, which reads as unreachable rather than gone.
    h.tmux()
        .args(["set-option", "-s", "exit-empty", "off"])
        .output()
        .expect("keep tmux server");
    h.tmux_kill_session(pane);

    let out = waiter.wait_with_output().expect("wait for `session wait`");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "stdout: {:?}", out.stdout);
    assert!(stderr.contains("is not running"), "stderr: {stderr}");
}

#[test]
#[parallel]
fn test_wait_returns_when_the_agent_reaches_the_target_status() {
    require_tmux!();
    let mut h = TuiTestHarness::new("session_wait_transition");
    h.install_path_command("opencode");
    let dir = h.home_path().join("fake-bin");
    std::fs::create_dir_all(&dir).expect("create fake-bin dir");
    let gate = h.home_path().join("gate");
    let agent = dir.join("fake-wait-agent");
    // Only the configured rule can read `deploy to prod?` as Waiting.
    write_executable(
        &agent,
        &format!(
            "#!/bin/sh\necho busy\nwhile [ ! -f '{}' ]; do sleep 0.1; done\n\
             echo 'deploy to prod?'\nsleep 60\n",
            gate.display()
        ),
    );
    h.append_config(
        "[[agents.opencode.status_rules]]\nstatus = \"waiting\"\ncontains = \"deploy to prod?\"",
    );
    let project = h.project_path();
    let id = h.add_session(&[
        project.to_str().unwrap(),
        "-t",
        "transition",
        "--tool",
        "opencode",
        "--cmd-override",
        agent.to_str().unwrap(),
    ]);
    h.run_cli_ok(&["session", "start", &id]);
    wait_until(Duration::from_secs(10), Duration::from_millis(200), || {
        let out = h.run_cli(&["session", "capture", &id, "--json"]);
        let parsed: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
        match parsed["content"].as_str() {
            Some(c) if c.contains("busy") => Ok(()),
            _ => Err("fake agent has not painted yet".to_string()),
        }
    });

    let mut waiter = h.spawn_cli(&["session", "wait", &id, "--until", "waiting"]);
    assert!(
        waiter.try_wait().expect("poll waiter").is_none(),
        "wait returned before the agent reached Waiting"
    );
    std::fs::write(&gate, "").expect("open the gate");

    let out = waiter.wait_with_output().expect("wait for `session wait`");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "waiting");
}
