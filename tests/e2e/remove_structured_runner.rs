//! The CLI trashes, purges and archives a structured session without the
//! daemon's help, so it must stop the session's out-of-process runner itself
//! and the daemon must not respawn it.

use std::time::{Duration, Instant};

use serial_test::parallel;

use crate::harness::{require_node, require_tmux, wait_until, TuiTestHarness};

/// The pid of the session's registered runner, if it has one.
fn runner_pid(h: &TuiTestHarness, id: &str) -> Option<u32> {
    let workers: serde_json::Value =
        serde_json::from_str(&h.run_cli_ok(&["ps", "--acp", "--json"])).expect("ps --acp --json");
    workers
        .as_array()
        .expect("ps --acp --json array")
        .iter()
        .find(|w| w["session_id"] == id)
        .map(|w| w["pid"].as_u64().expect("worker pid") as u32)
}

fn runner_is_gone_after(name: &str, cli_args: impl Fn(&str) -> Vec<String>) {
    let h = TuiTestHarness::new_acp(
        name,
        r#"{ "turns": [ { "updates": [], "stopReason": "end_turn" } ] }"#,
    );
    let (_, id) = h.start_structured_session(name);
    h.prompt_until_accepted(&id, "hello", Duration::from_secs(30));

    let pid = wait_until(Duration::from_secs(30), Duration::from_millis(100), || {
        runner_pid(&h, &id).ok_or_else(|| "runner not registered yet".to_string())
    });
    assert!(
        std::path::Path::new(&format!("/proc/{pid}")).exists(),
        "runner {pid} is not running before the CLI call"
    );

    let args = cli_args(&id);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    h.run_cli_ok(&args);

    wait_until(Duration::from_secs(10), Duration::from_millis(50), || {
        if runner_pid(&h, &id).is_some() {
            return Err("worker still registered".to_string());
        }
        let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default();
        if comm.trim() == "aoe" {
            return Err(format!("runner {pid} still running"));
        }
        Ok(())
    });

    // A crash-style stop is respawned by the daemon, so hold the observation
    // window open after the runner is gone.
    let window = Instant::now() + Duration::from_secs(3);
    while Instant::now() < window {
        assert_eq!(
            runner_pid(&h, &id),
            None,
            "the daemon respawned the runner after `aoe {}`",
            args.join(" ")
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
#[parallel]
fn remove_stops_the_structured_runner() {
    require_tmux!();
    require_node!();
    runner_is_gone_after("remove_runner", |id| {
        ["remove", id].map(str::to_string).to_vec()
    });
}

#[test]
#[parallel]
fn purge_stops_the_structured_runner() {
    require_tmux!();
    require_node!();
    runner_is_gone_after("purge_runner", |id| {
        ["remove", "--purge", id].map(str::to_string).to_vec()
    });
}

#[test]
#[parallel]
fn archive_stops_the_structured_runner() {
    require_tmux!();
    require_node!();
    runner_is_gone_after("archive_runner", |id| {
        ["session", "archive", id].map(str::to_string).to_vec()
    });
}

/// `--no-kill` skips only the tmux teardown.
#[test]
#[parallel]
fn archive_no_kill_still_stops_the_structured_runner() {
    require_tmux!();
    require_node!();
    runner_is_gone_after("archive_no_kill_runner", |id| {
        ["session", "archive", "--no-kill", id]
            .map(str::to_string)
            .to_vec()
    });
}
