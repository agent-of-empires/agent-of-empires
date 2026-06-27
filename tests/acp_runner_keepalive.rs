//! Regression test for #2455: the runner's stdout-silence keepalive must
//! unstick an agent whose stdout stalled mid-turn.
//!
//! The upstream `claude-agent-acp` adapter buffers its stdout with no
//! backpressure handling, so a mid-turn write can stall until a stdin
//! `data` event wakes its event loop. The runner now writes a harmless
//! `\n` to the agent's stdin after stdout goes silent, which flushes the
//! stall. Without that keepalive the session freezes until a human types
//! something.
//!
//! This spawns a real runner with a fake agent that writes one line, then
//! BLOCKS reading a line from stdin, then writes a second line. With the
//! keepalive the runner supplies the stdin byte automatically and the
//! second line is forwarded to the attached daemon; without it the fake
//! agent blocks forever and the second line never arrives.
//!
//! Unix-only: the harness drives the runner's unix socket directly, so the
//! whole file is gated rather than guarded at runtime (the `UnixStream`
//! import alone would otherwise break non-Unix compilation).
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// App data dir for the debug binary, which uses the `-dev` namespace.
fn app_dir(home: &Path, xdg: &Path) -> PathBuf {
    if cfg!(any(target_os = "linux", target_os = "macos")) {
        xdg.join("agent-of-empires-dev")
    } else {
        home.join(".agent-of-empires-dev")
    }
}

/// Unique scratch dir under `/tmp`, kept short so the worker unix socket
/// path stays under the macOS `SUN_LEN` limit. Removed on drop.
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

struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn keepalive_unsticks_agent_blocked_on_stdin() {
    let scratch = Scratch::new("k2455");
    let (home, xdg) = (scratch.0.clone(), scratch.0.clone());
    let workers = app_dir(&home, &xdg).join("acp-workers");
    let session_id = "s2455";
    let socket = workers.join(format!("{session_id}.sock"));

    // Fake agent: emit one ndjson-ish line, block on a stdin read (this is
    // the stall the keepalive must break), emit a second line, then keep
    // stdin open so the runner doesn't tear down before we read it.
    let fake = r#"printf 'LINE1\n'; IFS= read -r _; printf 'LINE2\n'; cat >/dev/null"#;

    let bin = env!("CARGO_BIN_EXE_aoe");
    let child = Command::new(bin)
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
            "sh",
            "-c",
            fake,
        ])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg)
        // Force the keepalive on for this non-claude fake agent, and shrink
        // the cadence so the test runs in well under a second.
        .env("AOE_ACP_STDOUT_NUDGE", "1")
        .env("AOE_ACP_STDOUT_NUDGE_MS", "150")
        .env("AOE_ACP_STDOUT_NUDGE_SLOW_MS", "150")
        .spawn()
        .expect("spawn acp runner");
    let mut child = KillOnDrop(child);

    // Connect to the runner's socket as the daemon; it forwards agent
    // stdout to us (replaying the ring for anything buffered pre-attach).
    let stream = connect_with_retry(&socket, Duration::from_secs(10), &mut child.0);
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());

    let line1 = read_line(&mut reader);
    assert!(
        line1.contains("LINE1"),
        "expected the agent's first line, got {line1:?}"
    );

    // The agent is now blocked on its stdin read. Only the runner's
    // keepalive can unblock it; if it does not, this read times out.
    let line2 = read_line(&mut reader);
    assert!(
        line2.contains("LINE2"),
        "second line never arrived; the stalled agent was not unstuck by the keepalive (got {line2:?})"
    );
}

fn connect_with_retry(socket: &Path, within: Duration, child: &mut Child) -> UnixStream {
    let deadline = Instant::now() + within;
    loop {
        if let Ok(s) = UnixStream::connect(socket) {
            return s;
        }
        if let Ok(Some(status)) = child.try_wait() {
            panic!("runner exited before its socket was connectable: {status}");
        }
        if Instant::now() > deadline {
            panic!(
                "runner socket {} never became connectable",
                socket.display()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Read one `\n`-terminated line; panic on timeout/EOF so a stall surfaces
/// as a clear failure rather than a hang.
fn read_line(reader: &mut BufReader<UnixStream>) -> String {
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => panic!("socket closed before a line arrived"),
        Ok(_) => line,
        Err(e) => {
            // Drain anything buffered for a better failure message.
            let mut rest = Vec::new();
            let _ = reader.get_mut().read_to_end(&mut rest);
            panic!("read failed (likely a timeout from the stalled agent): {e}");
        }
    }
}
