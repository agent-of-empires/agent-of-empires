//! Integration test for #1054 Phase A: the `aoe __acp-runner` shim binds a
//! sibling `<id>.control.sock` and reports a native turn-complete signal
//! over it when it observes the agent's response to the daemon-issued
//! `session/prompt` request.
//!
//! This spawns a real runner with `cat` as the fake agent. `cat` echoes
//! its stdin to stdout verbatim, which lets the test drive the full
//! round-trip through real sockets: writing a `session/prompt` request and
//! then a matching response line to the main relay socket makes the runner
//! forward each to `cat`, echo them back on the agent-to-daemon path, and
//! (for the response) fire `PromptCompleted` on the control socket. No real
//! ACP agent is needed to exercise the runner's control-channel wiring.
//!
//! Before this change the control socket did not exist, so the control
//! connect below fails outright; that is the red state the fix turns green.

#![cfg(feature = "serve")]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

/// App data dir for the debug binary under this test's env, mirroring the
/// XDG resolution the runner uses.
fn app_dir(home: &Path, xdg: &Path) -> PathBuf {
    if cfg!(any(target_os = "linux", target_os = "macos")) {
        xdg.join("agent-of-empires-dev")
    } else {
        home.join(".agent-of-empires-dev")
    }
}

/// Short-lived scratch dir under `/tmp` so the unix socket path stays
/// within the macOS `SUN_LEN` limit. Removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let base = if cfg!(unix) {
            PathBuf::from("/tmp")
        } else {
            std::env::temp_dir()
        };
        let dir = base.join(format!("aoc{}{label}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Scratch(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn wait_for(path: &Path, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !path.exists() {
        if Instant::now() > deadline {
            panic!("{what} never appeared at {}", path.display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Read one length-prefixed control frame (4-byte big-endian length, then
/// that many JSON bytes) and parse it as a generic JSON value.
fn read_frame(stream: &mut UnixStream) -> serde_json::Value {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read frame length");
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).expect("read frame body");
    serde_json::from_slice(&body).expect("parse frame json")
}

#[test]
fn runner_reports_native_prompt_complete_over_control_socket() {
    if cfg!(not(unix)) {
        return;
    }
    let scratch = Scratch::new("ctl");
    let home = scratch.0.join("home");
    let xdg = scratch.0.join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();

    let session_id = "sctl0001";
    let workers = app_dir(&home, &xdg).join("acp-workers");
    let socket = workers.join(format!("{session_id}.sock"));
    let control = workers.join(format!("{session_id}.control.sock"));
    let record = workers.join(format!("{session_id}.json"));

    let bin = env!("CARGO_BIN_EXE_aoe");
    let mut child: Child = Command::new(bin)
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
            // Absolute path: relying on the runner's inherited PATH makes a
            // non-standard PATH (e.g. nix-first) surface as a confusing
            // "registry record never appeared" instead of a clear failure.
            "/bin/cat",
        ])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg)
        .env("AOE_ACP_WATCHDOG_POLL_MS", "150")
        .spawn()
        .expect("spawn acp runner");

    // The runner binds the control socket before the main relay socket, so
    // both exist once the record is written.
    wait_for(&record, "registry record");
    wait_for(&control, "control socket");
    wait_for(&socket, "relay socket");

    // Attach the control channel and read the runner's Hello greeting.
    let mut ctl = UnixStream::connect(&control).expect("connect control socket");
    ctl.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let hello = read_frame(&mut ctl);
    assert_eq!(hello["kind"], "hello", "first control frame is Hello");
    assert_eq!(hello["session_id"], session_id);

    // Reading Hello proves the runner has started installing the control
    // outbound, but the write half is stored just after Hello is sent, so
    // this short wait lets that store land before we drive the prompt,
    // exercising the live-write path rather than the buffered path. This is
    // an ordering wait, not the closed emit/install TOCTOU race.
    std::thread::sleep(Duration::from_millis(150));

    // Act as the daemon on the relay socket: issue a session/prompt request
    // (records the id), then a matching response (cat echoes it back on the
    // agent-to-daemon path, where the runner detects turn completion).
    let mut relay = UnixStream::connect(&socket).expect("connect relay socket");
    relay
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"session/prompt\",\"params\":{}}\n")
        .unwrap();
    relay
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{\"stopReason\":\"end_turn\"}}\n")
        .unwrap();
    relay.flush().unwrap();

    // The runner surfaces a native turn-complete for prompt id 5. The relay
    // echoes the two request/response lines first, but those flow on the
    // relay socket, not the control socket, so the next control frame is the
    // PromptCompleted.
    let completed = read_frame(&mut ctl);
    assert_eq!(completed["kind"], "prompt_completed");
    assert_eq!(completed["prompt_req_id"], 5);
    assert_eq!(completed["stop_reason"], "end_turn");

    let _ = child.kill();
    let _ = child.wait();
}
