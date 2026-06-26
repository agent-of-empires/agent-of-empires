//! Regression tests for #1921: an abandoned `aoe __acp-runner` must
//! self-terminate instead of leaking forever, and a superseded runner must
//! exit WITHOUT deleting the replacement runner's files.
//!
//! Before the fix, the runner's main loop only exited on agent-child exit
//! or SIGTERM/SIGINT, so a runner whose daemon vanished (crash, SIGKILL, or
//! a deleted `$HOME`) stayed alive indefinitely, holding its agent
//! subprocess open. The watchdog added in #1921 polls the runner's own
//! registry record and self-destructs when it disappears or is taken over.
//!
//! These spawn a real runner with `cat` as a trivial long-lived fake agent
//! (it blocks reading stdin, which the runner keeps open). The runner is
//! spawned WITHOUT `setsid` here (only the daemon sets that up in
//! production), so it takes the non-group-leader fallback teardown path,
//! which is safe under the test's own process group, and is exactly the
//! path where the superseded-delete bug lived.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// App data dir for the debug binary under this test's env. The runner sees
/// `XDG_CONFIG_HOME`, so macOS follows the same XDG path as Linux.
fn app_dir(home: &Path, xdg: &Path) -> PathBuf {
    if cfg!(any(target_os = "linux", target_os = "macos")) {
        xdg.join("agent-of-empires-dev")
    } else {
        home.join(".agent-of-empires-dev")
    }
}

/// Unique scratch dir; removed on drop. Rooted under `/tmp` (not the
/// system temp dir, which is a long `/var/folders/...` path on macOS) and
/// kept short so the worker unix socket path stays under the macOS
/// `SUN_LEN` (~104 byte) limit. The `label` keeps concurrently-running
/// tests in the same binary from sharing (and racing to delete) a dir.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let base = if cfg!(unix) {
            PathBuf::from("/tmp")
        } else {
            std::env::temp_dir()
        };
        let dir = base.join(format!("ao{}{label}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Spawn a runner with `cat` as the fake agent and wait until it has
/// written its registry record. Returns the child and the record path.
fn spawn_runner_and_wait_for_record(home: &Path, xdg: &Path, session_id: &str) -> (Child, PathBuf) {
    let workers = app_dir(home, xdg).join("acp-workers");
    let socket = workers.join(format!("{session_id}.sock"));
    let record = workers.join(format!("{session_id}.json"));

    let bin = env!("CARGO_BIN_EXE_aoe");
    let mut child = Command::new(bin)
        .args([
            "__acp-runner",
            "--socket",
            socket.to_str().unwrap(),
            "--session-id",
            session_id,
            "--agent-name",
            "fake-agent",
            "--cwd",
            home.to_str().unwrap(),
            "--",
            "cat",
        ])
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", xdg)
        // Shrink the watchdog poll so an orphan dies in well under a second.
        .env("AOE_ACP_WATCHDOG_POLL_MS", "150")
        .spawn()
        .expect("spawn acp runner");

    let deadline = Instant::now() + Duration::from_secs(10);
    while !record.exists() {
        if let Ok(Some(status)) = child.try_wait() {
            panic!("runner exited before writing its registry record: {status}");
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!(
                "runner never wrote its registry record at {}",
                record.display()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    (child, record)
}

/// Wait for `child` to exit within `secs`, killing + panicking otherwise.
fn assert_exits_within(child: &mut Child, secs: u64, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if child.try_wait().unwrap().is_some() {
            return;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("{what}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// PID of the runner's direct child (the fake `cat` agent), via `pgrep -P`.
fn agent_pid_of(runner_pid: u32) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let out = Command::new("pgrep")
            .args(["-P", &runner_pid.to_string()])
            .output()
            .expect("run pgrep");
        if let Some(pid) = String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .and_then(|l| l.trim().parse::<u32>().ok())
        {
            return pid;
        }
        if Instant::now() > deadline {
            panic!("fake agent (cat) child of runner {runner_pid} never appeared");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `kill -0`: success means the pid is still alive.
fn pid_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Assert the agent pid is gone within `secs`. The leak this PR fixes is
/// specifically the agent surviving its runner, so the test must prove the
/// agent dies, not just the runner.
fn assert_pid_gone_within(pid: u32, secs: u64, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while pid_alive(pid) {
        if Instant::now() > deadline {
            panic!("{what}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn orphaned_runner_self_terminates_when_record_deleted() {
    if cfg!(not(unix)) {
        return;
    }

    let scratch = Scratch::new("a");
    let (home, xdg) = (scratch.0.clone(), scratch.0.clone());
    let (mut child, record) = spawn_runner_and_wait_for_record(&home, &xdg, "s1921");
    let agent_pid = agent_pid_of(child.id());

    // It must stay alive while the record exists (the whole point of the
    // shim outliving a detached daemon).
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        child.try_wait().unwrap().is_none(),
        "runner exited while its registry record still existed"
    );

    // Abandon it: delete the record, as a deleted `$HOME` or a daemon-side
    // `delete` would.
    std::fs::remove_file(&record).unwrap();

    assert_exits_within(
        &mut child,
        15,
        "orphaned runner did not self-terminate within 15s of its record being deleted",
    );
    // The agent subprocess, not just the runner, must die: the agent
    // surviving its runner is the exact leak this PR fixes.
    assert_pid_gone_within(
        agent_pid,
        5,
        "fake agent survived the runner's self-termination (the leak this PR fixes)",
    );
}

#[test]
fn superseded_runner_exits_without_deleting_replacement_record() {
    if cfg!(not(unix)) {
        return;
    }

    let scratch = Scratch::new("b");
    let (home, xdg) = (scratch.0.clone(), scratch.0.clone());
    let (mut child, record) = spawn_runner_and_wait_for_record(&home, &xdg, "s1922");
    let agent_pid = agent_pid_of(child.id());

    // Simulate a fresh runner taking over: rewrite the record so its `pid`
    // is no longer ours. Parse + mutate structurally so the test doesn't
    // break on a harmless serializer format change. The watchdog must read
    // this as "superseded" and exit, but MUST leave the (replacement's)
    // record file in place.
    let mut rec: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&record).unwrap()).unwrap();
    let runner_pid = child.id();
    rec["pid"] = serde_json::json!(runner_pid.wrapping_add(1_000_000));
    std::fs::write(&record, serde_json::to_vec(&rec).unwrap()).unwrap();

    assert_exits_within(
        &mut child,
        15,
        "superseded runner did not self-terminate within 15s of its record being taken over",
    );
    assert_pid_gone_within(
        agent_pid,
        5,
        "fake agent survived the superseded runner's self-termination",
    );

    // The replacement runner's record must survive the superseded runner's
    // exit; deleting it here would cascade (the new runner would then see
    // its own record vanish and self-destruct too).
    assert!(
        record.exists(),
        "superseded runner deleted the replacement runner's registry record"
    );
}
