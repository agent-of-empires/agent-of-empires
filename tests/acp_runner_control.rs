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

/// Kill+reap the spawned runner on drop so an assertion failure mid-test
/// doesn't leave a runner (and its agent tree) behind. Pairs with
/// `Scratch`, which removes the scratch dir on drop.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
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
    assert_eq!(completed["outcome"]["status"], "completed");
    assert_eq!(completed["outcome"]["stop_reason"], "end_turn");

    let _ = child.kill();
    let _ = child.wait();
}

/// Write a length-prefixed control frame (4-byte big-endian length, then
/// the JSON body).
fn write_frame(stream: &mut UnixStream, body: &serde_json::Value) {
    let json = serde_json::to_vec(body).expect("serialize frame");
    let len = (json.len() as u32).to_be_bytes();
    stream.write_all(&len).expect("write frame length");
    stream.write_all(&json).expect("write frame body");
    stream.flush().expect("flush frame");
}

/// Resolve a python3 interpreter for the fake ACP agent, or None to skip.
fn find_python3() -> Option<PathBuf> {
    for cand in [
        "/usr/bin/python3",
        "/opt/homebrew/bin/python3",
        "/usr/local/bin/python3",
    ] {
        let p = PathBuf::from(cand);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

/// #2976 Phase B: the runner owns the ACP handshake. Drive it as a v2
/// daemon over the control channel across two attaches and assert the
/// agent is handshaken (initialize + session/new) exactly once, that the
/// second attach replays the cache without touching the agent, and that a
/// prompt completes natively.
#[test]
fn runner_owns_handshake_and_caches_across_attaches() {
    if cfg!(not(unix)) {
        return;
    }
    let Some(python3) = find_python3() else {
        eprintln!("skipping: python3 not found for fake ACP agent");
        return;
    };

    let scratch = Scratch::new("hs");
    let home = scratch.0.join("home");
    let xdg = scratch.0.join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();

    // A minimal ACP agent: responds to the runner-issued handshake and
    // prompt requests and appends each received method to a log so the test
    // can assert the agent saw each exactly once.
    let agent_log = scratch.0.join("agent-methods.log");
    let agent_py = scratch.0.join("fake_agent.py");
    std::fs::write(
        &agent_py,
        r#"
import sys, json, os
log = os.environ["AOE_FAKE_AGENT_LOG"]
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except Exception:
        continue
    method = msg.get("method")
    mid = msg.get("id")
    if method is None or mid is None:
        continue
    with open(log, "a") as f:
        f.write(method + "\n")
    if method == "initialize":
        result = {"protocolVersion": 1, "agentCapabilities": {"loadSession": False, "promptCapabilities": {}}}
    elif method == "session/new":
        result = {"sessionId": "sess-fake-1"}
    elif method == "session/prompt":
        result = {"stopReason": "end_turn"}
    else:
        result = {}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}) + "\n")
    sys.stdout.flush()
"#,
    )
    .unwrap();

    let session_id = "shs00001";
    let workers = app_dir(&home, &xdg).join("acp-workers");
    let socket = workers.join(format!("{session_id}.sock"));
    let control = workers.join(format!("{session_id}.control.sock"));
    let record = workers.join(format!("{session_id}.json"));

    let bin = env!("CARGO_BIN_EXE_aoe");
    let _child = KillOnDrop(
        Command::new(bin)
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
                python3.to_str().unwrap(),
                agent_py.to_str().unwrap(),
            ])
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &xdg)
            .env("AOE_FAKE_AGENT_LOG", &agent_log)
            .env("AOE_ACP_WATCHDOG_POLL_MS", "150")
            .spawn()
            .expect("spawn acp runner"),
    );

    wait_for(&record, "registry record");
    wait_for(&control, "control socket");

    let v2 = serde_json::json!(2);

    // --- First attach: the runner runs the handshake against the agent. ---
    {
        let mut ctl = UnixStream::connect(&control).expect("connect control socket");
        ctl.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let hello = read_frame(&mut ctl);
        assert_eq!(hello["kind"], "hello");
        assert_eq!(hello["control_protocol_version"], v2);

        write_frame(
            &mut ctl,
            &serde_json::json!({"kind": "attach", "control_protocol_version": 2}),
        );
        write_frame(
            &mut ctl,
            &serde_json::json!({"kind": "initialize", "request": {"protocolVersion": 1}}),
        );
        let initialized = read_frame(&mut ctl);
        assert_eq!(initialized["kind"], "initialized");
        assert!(initialized["result"].is_object());

        write_frame(
            &mut ctl,
            &serde_json::json!({"kind": "establish_session", "method": "session/new", "request": {"cwd": home.to_str().unwrap()}}),
        );
        let ready = read_frame(&mut ctl);
        assert_eq!(ready["kind"], "session_ready");
        assert_eq!(ready["acp_session_id"], "sess-fake-1");

        write_frame(
            &mut ctl,
            &serde_json::json!({"kind": "prompt", "request": {"sessionId": "sess-fake-1", "prompt": []}}),
        );
        let completed = read_frame(&mut ctl);
        assert_eq!(completed["kind"], "prompt_completed");
        assert_eq!(completed["outcome"]["status"], "completed");
        assert_eq!(completed["outcome"]["stop_reason"], "end_turn");
    }

    // --- Second attach: the runner replays the cache, no agent contact. ---
    {
        let mut ctl = UnixStream::connect(&control).expect("reconnect control socket");
        ctl.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let hello = read_frame(&mut ctl);
        assert_eq!(hello["kind"], "hello");

        write_frame(
            &mut ctl,
            &serde_json::json!({"kind": "attach", "control_protocol_version": 2}),
        );
        write_frame(
            &mut ctl,
            &serde_json::json!({"kind": "initialize", "request": {"protocolVersion": 1}}),
        );
        let initialized = read_frame(&mut ctl);
        assert_eq!(initialized["kind"], "initialized");

        write_frame(
            &mut ctl,
            &serde_json::json!({"kind": "establish_session", "method": "session/new", "request": {}}),
        );
        let ready = read_frame(&mut ctl);
        assert_eq!(ready["kind"], "session_ready");
        assert_eq!(ready["acp_session_id"], "sess-fake-1");
    }

    // The agent saw the handshake exactly once despite two attaches; the
    // second attach was served entirely from the runner's cache.
    let methods = std::fs::read_to_string(&agent_log).unwrap_or_default();
    let count = |m: &str| methods.lines().filter(|l| *l == m).count();
    assert_eq!(
        count("initialize"),
        1,
        "initialize sent to agent once: {methods:?}"
    );
    assert_eq!(
        count("session/new"),
        1,
        "session/new sent to agent once: {methods:?}"
    );
    assert_eq!(
        count("session/prompt"),
        1,
        "session/prompt sent to agent once: {methods:?}"
    );
}

/// #2976 Phase B regression: when the agent answers `session/new` with a
/// JSON-RPC error, the runner forwards the FULL error object (including
/// `data`) in `HandshakeFailed`, so the daemon can reconstruct the crate
/// error and surface the same `data.details` remediation banner the
/// byte-relay path did. Guards the startup-error-banner live test at the
/// runner layer.
#[test]
fn runner_forwards_session_error_data_in_handshake_failed() {
    if cfg!(not(unix)) {
        return;
    }
    let Some(python3) = find_python3() else {
        eprintln!("skipping: python3 not found for fake ACP agent");
        return;
    };

    let scratch = Scratch::new("hserr");
    let home = scratch.0.join("home");
    let xdg = scratch.0.join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();

    let agent_py = scratch.0.join("fail_agent.py");
    std::fs::write(
        &agent_py,
        r#"
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except Exception:
        continue
    method = msg.get("method")
    mid = msg.get("id")
    if method is None or mid is None:
        continue
    if method == "initialize":
        resp = {"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": 1, "agentCapabilities": {"loadSession": False, "promptCapabilities": {}}}}
    elif method == "session/new":
        resp = {"jsonrpc": "2.0", "id": mid, "error": {"code": -32603, "message": "Internal error", "data": {"details": "native binary failed to launch"}}}
    else:
        resp = {"jsonrpc": "2.0", "id": mid, "result": {}}
    sys.stdout.write(json.dumps(resp) + "\n")
    sys.stdout.flush()
"#,
    )
    .unwrap();

    let session_id = "shserr01";
    let workers = app_dir(&home, &xdg).join("acp-workers");
    let socket = workers.join(format!("{session_id}.sock"));
    let control = workers.join(format!("{session_id}.control.sock"));
    let record = workers.join(format!("{session_id}.json"));

    let bin = env!("CARGO_BIN_EXE_aoe");
    let _child = KillOnDrop(
        Command::new(bin)
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
                python3.to_str().unwrap(),
                agent_py.to_str().unwrap(),
            ])
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &xdg)
            .env("AOE_ACP_WATCHDOG_POLL_MS", "150")
            .spawn()
            .expect("spawn acp runner"),
    );

    wait_for(&record, "registry record");
    wait_for(&control, "control socket");

    let mut ctl = UnixStream::connect(&control).expect("connect control socket");
    ctl.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let hello = read_frame(&mut ctl);
    assert_eq!(hello["kind"], "hello");

    write_frame(
        &mut ctl,
        &serde_json::json!({"kind": "attach", "control_protocol_version": 2}),
    );
    write_frame(
        &mut ctl,
        &serde_json::json!({"kind": "initialize", "request": {"protocolVersion": 1}}),
    );
    let initialized = read_frame(&mut ctl);
    assert_eq!(initialized["kind"], "initialized");

    write_frame(
        &mut ctl,
        &serde_json::json!({"kind": "establish_session", "method": "session/new", "request": {}}),
    );
    let failed = read_frame(&mut ctl);
    assert_eq!(failed["kind"], "handshake_failed");
    // The remediation detail survives the control channel intact.
    assert_eq!(
        failed["error"]["data"]["details"],
        "native binary failed to launch"
    );
    assert_eq!(failed["error"]["code"], -32603);
}
