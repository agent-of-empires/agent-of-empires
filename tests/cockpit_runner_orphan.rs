//! Regression test for #1921: an abandoned `aoe __cockpit-runner` must
//! self-terminate instead of leaking forever.
//!
//! Before the fix, the runner's main loop only exited on agent-child exit
//! or SIGTERM/SIGINT, so a runner whose daemon vanished (crash, SIGKILL, or
//! a deleted `$HOME`) stayed alive indefinitely, holding its agent
//! subprocess open. The watchdog added in #1921 polls the runner's own
//! registry record and self-destructs when it disappears.
//!
//! This spawns a real runner with `cat` as a trivial long-lived fake agent
//! (it blocks reading stdin, which the runner keeps open), waits for the
//! registry record, deletes it, and asserts the runner exits. The runner is
//! spawned WITHOUT `setsid` here (only the daemon sets that up in
//! production), so it takes the non-group-leader fallback teardown path,
//! which is safe under the test's own process group.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// App data dir for the debug binary, which uses the `-dev` namespace.
fn app_dir(home: &Path, xdg: &Path) -> PathBuf {
    if cfg!(target_os = "linux") {
        xdg.join("agent-of-empires-dev")
    } else {
        home.join(".agent-of-empires-dev")
    }
}

/// Unique scratch dir; removed on drop. Rooted under `/tmp` (not the
/// system temp dir, which is a long `/var/folders/...` path on macOS) and
/// kept short so the worker unix socket path stays under the macOS
/// `SUN_LEN` (~104 byte) limit. Same constraint the live/e2e harnesses hit.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let base = if cfg!(unix) {
            PathBuf::from("/tmp")
        } else {
            std::env::temp_dir()
        };
        let dir = base.join(format!("ao{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn orphaned_runner_self_terminates_when_record_deleted() {
    // The watchdog teardown is unix process-group based.
    if cfg!(not(unix)) {
        return;
    }

    let scratch = Scratch::new();
    // HOME == XDG == the scratch root, kept short for the socket-path limit.
    let home = scratch.0.clone();
    let xdg = scratch.0.clone();

    let workers = app_dir(&home, &xdg).join("cockpit-workers");
    let session_id = "s1921";
    let socket = workers.join(format!("{session_id}.sock"));
    let record = workers.join(format!("{session_id}.json"));

    let bin = env!("CARGO_BIN_EXE_aoe");
    let mut child = Command::new(bin)
        .args([
            "__cockpit-runner",
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
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg)
        // Shrink the watchdog poll so an orphan dies in well under a second.
        .env("AOE_COCKPIT_WATCHDOG_POLL_MS", "150")
        .spawn()
        .expect("spawn cockpit runner");

    // Wait for the runner to write its registry record.
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

    // The watchdog (150ms poll, 2-miss debounce) should fire and the runner
    // should exit well within this margin.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if child.try_wait().unwrap().is_some() {
            return; // self-terminated: pass
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("orphaned runner did not self-terminate within 15s of its record being deleted");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
