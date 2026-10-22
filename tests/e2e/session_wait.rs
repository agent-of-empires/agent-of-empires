//! `aoe session wait` blocks on an activity transition and fails when the session goes away.

use std::time::Duration;

use serde_json::Value;
use serial_test::parallel;

use crate::harness::{app_dir_in, require_tmux, wait_until, write_executable, TuiTestHarness};

/// Statuses the waiter has reported, from its per-poll debug line. Only `session wait` logs it, so
/// a line proves the waiter itself completed a poll.
fn wait_poll_statuses(h: &TuiTestHarness) -> Vec<String> {
    let log =
        std::fs::read_to_string(app_dir_in(h.home_path()).join("debug.log")).unwrap_or_default();
    log.lines()
        .filter_map(|l| l.split("wait poll status=").nth(1))
        .map(|s| s.trim().to_string())
        .collect()
}

/// Block until the waiter has completed a poll whose status satisfies `accept`.
fn wait_for_poll(h: &TuiTestHarness, accept: impl Fn(&str) -> bool) {
    wait_until(
        Duration::from_secs(10),
        Duration::from_millis(100),
        || match wait_poll_statuses(h).iter().find(|s| accept(s)) {
            Some(_) => Ok(()),
            None => Err("the waiter has not completed a matching poll yet".to_string()),
        },
    );
}

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
    h.set_env("AOE_LOG_LEVEL", "debug");
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
    wait_for_poll(&h, |s| s != "stopped" && s != "error");

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
    h.set_env("AOE_LOG_LEVEL", "debug");
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

    h.run_cli_ok(&["profile", "create", "other"]);
    let mut waiter = h.spawn_cli(&[
        "session",
        "wait",
        &id,
        "--until",
        "waiting",
        "--timeout",
        "10000",
    ]);
    wait_for_poll(&h, |s| s != "waiting");
    assert!(
        waiter.try_wait().expect("poll waiter").is_none(),
        "wait returned before the agent reached Waiting"
    );
    h.run_cli_ok(&["profile", "default", "other"]);
    std::fs::write(&gate, "").expect("open the gate");

    let out = waiter.wait_with_output().expect("wait for `session wait`");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "waiting");
}

/// A launch holds `Launch`/`Starting` before its pane exists; the missing pane is not termination.
#[test]
#[parallel]
fn test_wait_survives_a_launch_held_by_a_pre_launch_hook() {
    require_tmux!();
    let mut h = TuiTestHarness::new("session_wait_launch");
    h.set_env("AOE_LOG_LEVEL", "debug");
    let bin = h.install_path_command("claude");
    std::fs::write(bin.join("claude"), "#!/bin/sh\nexec sleep 600\n").expect("write claude stub");
    let gate = h.home_path().join("gate");
    h.append_config(&format!(
        "[hooks]\non_launch = [\"while [ ! -f {} ]; do sleep 0.1; done\"]",
        gate.display()
    ));
    // Another session keeps the tmux server reachable, so the missing pane reads as Absent.
    let keepalive = h
        .tmux()
        .args(["new-session", "-d", "-s", "keepalive", "sleep 600"])
        .output()
        .expect("start keepalive session");
    assert!(keepalive.status.success(), "{keepalive:?}");
    let project = h.project_path();
    h.add_session(&[project.to_str().unwrap(), "-t", "launcher"]);

    let starter = h.spawn_cli(&["session", "start", "launcher"]);
    wait_until(Duration::from_secs(10), Duration::from_millis(100), || {
        let out = h.run_cli(&["session", "show", "launcher", "--json"]);
        let parsed: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
        match parsed["status"].as_str() {
            Some("starting") => Ok(()),
            other => Err(format!("launch not reserved yet: {other:?}")),
        }
    });

    let mut waiter = h.spawn_cli(&[
        "session",
        "wait",
        "launcher",
        "--until",
        "running",
        "--until",
        "waiting",
        "--until",
        "idle",
        "--until",
        "error",
        "--timeout",
        "30000",
    ]);
    wait_for_poll(&h, |s| s == "starting");
    assert!(
        waiter.try_wait().expect("poll waiter").is_none(),
        "wait gave up while the launch was still held"
    );
    std::fs::write(&gate, "").expect("release the hook");

    let started = starter
        .wait_with_output()
        .expect("wait for `session start`");
    assert!(
        started.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let out = waiter.wait_with_output().expect("wait for `session wait`");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        ["running", "waiting", "idle", "error"].contains(&stdout.trim()),
        "stdout: {stdout}"
    );
}
