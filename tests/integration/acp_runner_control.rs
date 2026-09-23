//! Integration tests for the runner's control-only v3 transport.
//!
//! The scenarios spawn real `aoe __acp-runner` processes with deterministic
//! stand-in agents and exercise framed handshake, forward-lane, reverse-lane,
//! reconnect, cache, and cancellation behavior over `<id>.control.sock`.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use agent_of_empires::acp::acp_client::AcpClient;
use agent_of_empires::acp::state::AcpSessionId;

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

fn wait_for_u32(path: &Path, what: &str) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(value) = std::fs::read_to_string(path) {
            if let Ok(value) = value.trim().parse() {
                return value;
            }
        }
        if Instant::now() > deadline {
            panic!("{what} never became a u32 at {}", path.display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_runner_exit(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child.try_wait().expect("inspect runner").is_some() {
            return;
        }
        assert!(Instant::now() < deadline, "timed-out runner stayed live");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn wait_for_record_pid(path: &Path, pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|record| record["pid"].as_u64())
            == Some(u64::from(pid))
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "replacement record never appeared"
        );
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

/// The v3 core loop over real sockets and framing: an
/// agent-issued request reaches the daemon as a `ServerCall`, and the
/// daemon's `ServerResult` reaches the agent as a JSON-RPC response echoing
/// the agent's own id.
///
/// The stand-in agent is `/bin/cat`, so whatever the runner writes to its
/// stdin comes back on its stdout. That is enough to drive both directions:
/// the test writes an agent-to-client request into the runner (by having the
/// runner send it to cat), and reads back the response the runner wrote.
#[test]
#[serial_test::parallel]
fn runner_proxies_agent_requests_over_the_control_channel() {
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

    wait_for(&record, "registry record");
    wait_for(&control, "control socket");
    // #2977: the relay socket is retired, so the runner must NOT create it.
    assert!(
        !socket.exists(),
        "a v3 runner must not bind the retired raw socket at {}",
        socket.display()
    );

    let mut ctl = UnixStream::connect(&control).expect("connect control socket");
    ctl.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let hello = read_frame(&mut ctl);
    assert_eq!(hello["kind"], "hello", "first control frame is Hello");
    assert_eq!(hello["session_id"], session_id);
    assert_eq!(hello["control_protocol_version"], 3);

    write_frame(
        &mut ctl,
        &serde_json::json!({"kind": "attach", "control_protocol_version": 3}),
    );

    // Drive an agent-to-client request through cat. `fs/read_text_file` goes
    // out on the forward lane, so the runner writes it to the agent; cat
    // echoes it back, and that echo is indistinguishable from the agent
    // issuing the request itself. One round trip therefore exercises both
    // lanes. (Preserving a string-shaped agent id is covered by the
    // `parse_request_value_id` table in `src/process/runner.rs`; the ids here
    // are numeric.)
    write_frame(
        &mut ctl,
        &serde_json::json!({
            "kind": "agent_call",
            "call_id": 1,
            "method": "fs/read_text_file",
            "params": {"path": "/tmp/x"},
        }),
    );

    // cat echoes the runner's own request back, so the runner sees a request
    // (id + method) on the agent-to-daemon path and forwards it as a
    // ServerCall.
    let call = read_frame(&mut ctl);
    assert_eq!(call["kind"], "server_call", "got {call}");
    assert_eq!(call["method"], "fs/read_text_file");
    assert_eq!(call["params"]["path"], "/tmp/x");
    let call_id = call["call_id"].as_u64().expect("call_id");

    // Answer it. The runner must write a JSON-RPC response to the agent
    // echoing the agent's own id, which cat echoes back to the runner, which
    // has no waiter for it and drops it. Nothing more should arrive on the
    // control channel.
    write_frame(
        &mut ctl,
        &serde_json::json!({
            "kind": "server_result",
            "call_id": call_id,
            "result": {"content": "hello"},
        }),
    );

    // A second answer for the same call must be dropped, not written again.
    write_frame(
        &mut ctl,
        &serde_json::json!({
            "kind": "server_result",
            "call_id": call_id,
            "result": {"content": "duplicate"},
        }),
    );

    // The runner answered the echoed request, so cat echoes that response
    // back; it carries an id the runner allocated for the forward call, so it
    // resolves call_id 1. That is the next (and only) control frame.
    let result = read_frame(&mut ctl);
    assert_eq!(result["kind"], "agent_result", "got {result}");
    assert_eq!(result["call_id"], 1);

    // More than the former eight-slot staging channel must be consumed without
    // treating a local queue-full condition as a daemon disconnect.
    for index in 0..16 {
        write_frame(
            &mut ctl,
            &serde_json::json!({
                "kind": "agent_call",
                "call_id": 100 + index,
                "method": "fs/read_text_file",
                "params": {"index": index},
            }),
        );
    }
    for index in 0..16 {
        let call = read_frame(&mut ctl);
        assert_eq!(call["kind"], "server_call", "burst frame {index}: {call}");
        assert_eq!(call["params"]["index"], index);
    }

    let _ = child.kill();
    let _ = child.wait();
}

/// Read control frames until one is not a `notify`.
///
/// The control channel interleaves the agent's `session/update` stream with
/// typed frames. Tests that want the typed frame skip past them.
fn read_typed_frame(stream: &mut UnixStream) -> serde_json::Value {
    loop {
        let frame = read_frame(stream);
        if frame["kind"] != "notify" {
            return frame;
        }
    }
}

/// Write a length-prefixed control frame (4-byte big-endian length, then
/// the JSON body).
fn encode_frame(body: &serde_json::Value) -> Vec<u8> {
    let json = serde_json::to_vec(body).expect("serialize frame");
    let len = (json.len() as u32).to_be_bytes();
    [len.as_slice(), &json].concat()
}

fn write_frame(stream: &mut UnixStream, body: &serde_json::Value) {
    stream.write_all(&encode_frame(body)).expect("write frame");
    stream.flush().expect("flush frame");
}

/// A valid peer that stops reading a 17 MiB agent frame must time out without
/// losing it or monopolizing the accept slot; the next daemon receives the same
/// frame. Since the identity guard holds the detached backlog until the session
/// is announced, both peers establish the session before the large
/// `session/update` is delivered.
#[test]
#[serial_test::parallel]
fn runner_requeues_large_frame_after_stalled_writer() {
    if cfg!(not(unix)) {
        return;
    }
    let Some(python3) = find_python3() else {
        eprintln!("skipping: python3 not found for large-frame agent");
        return;
    };

    let scratch = Scratch::new("attach");
    let home = scratch.0.join("home");
    let xdg = scratch.0.join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();
    let trigger = scratch.0.join("emit-large");
    let emitted = scratch.0.join("large-emitted");
    let agent = scratch.0.join("large_agent.py");
    // Establishes a session, then on trigger emits one 17 MiB session/update
    // (the detached backlog) and records that it left the agent.
    std::fs::write(
        &agent,
        r#"import json, sys, pathlib, time
trigger, emitted = map(pathlib.Path, sys.argv[1:])
def send(m):
    print(json.dumps(m), flush=True)
for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":msg["id"],"result":{"protocolVersion":1,"agentCapabilities":{}}})
    elif method == "session/new":
        send({"jsonrpc":"2.0","id":msg["id"],"result":{"sessionId":"large-sess"}})
        while not trigger.exists(): time.sleep(0.01)
        send({"jsonrpc":"2.0","method":"session/update","params":{"blob":"x" * (17 * 1024 * 1024)}})
        emitted.write_text("emitted")
    else:
        send({"jsonrpc":"2.0","id":msg["id"],"result":{}})
"#,
    )
    .unwrap();

    let session_id = "sattach1";
    let workers = app_dir(&home, &xdg).join("acp-workers");
    let socket = workers.join(format!("{session_id}.sock"));
    let control = workers.join(format!("{session_id}.control.sock"));
    let record = workers.join(format!("{session_id}.json"));
    let _child = KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_aoe"))
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
                agent.to_str().unwrap(),
                trigger.to_str().unwrap(),
                emitted.to_str().unwrap(),
            ])
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &xdg)
            .env("AOE_ACP_WATCHDOG_POLL_MS", "150")
            .spawn()
            .expect("spawn acp runner"),
    );

    wait_for(&record, "registry record");
    wait_for(&control, "control socket");

    // Helpers driving the framed handshake over a raw peer.
    let attach = |stream: &mut UnixStream| {
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        assert_eq!(read_frame(stream)["kind"], "hello");
        write_frame(
            stream,
            &serde_json::json!({"kind": "attach", "control_protocol_version": 3}),
        );
    };
    let initialize = |stream: &mut UnixStream| {
        write_frame(
            stream,
            &serde_json::json!({"kind": "initialize", "request": {"protocolVersion": 1}}),
        );
        assert_eq!(read_typed_frame(stream)["kind"], "initialized");
    };

    // The stalled peer establishes the session, then stops reading so the
    // large notify's write times out and the frame is requeued.
    let mut stalled = UnixStream::connect(&control).expect("connect stalled peer");
    attach(&mut stalled);
    initialize(&mut stalled);
    write_frame(
        &mut stalled,
        &serde_json::json!({
            "kind": "establish_session",
            "method": "session/new",
            "request": {"cwd": home.to_str().unwrap(), "mcpServers": []},
        }),
    );
    assert_eq!(read_typed_frame(&mut stalled)["kind"], "session_ready");
    // Release the backlog now that identity is announced, then stall.
    std::fs::write(&trigger, "go").unwrap();
    wait_for(&emitted, "detached backlog");

    // The replacement peer is admitted only after the stalled writer times out;
    // it resumes and receives the requeued large frame.
    let mut accepted = UnixStream::connect(&control).expect("connect replacement peer");
    attach(&mut accepted);
    initialize(&mut accepted);
    write_frame(
        &mut accepted,
        &serde_json::json!({"kind": "resume_session"}),
    );
    assert_eq!(read_frame(&mut accepted)["kind"], "session_ready");
    let buffered = read_frame(&mut accepted);
    assert_eq!(buffered["kind"], "notify", "got {buffered}");
    assert_eq!(buffered["method"], "session/update");
    assert_eq!(
        buffered["params"]["blob"].as_str().unwrap().len(),
        17 * 1024 * 1024
    );
    drop(stalled);
}

/// Regression for a control-lane deadlock: an agent
/// that issues a client request while it is answering `session/new`.
///
/// That is legal ACP and precisely what this proxy layer exists to support.
/// Before the fix, the runner's control read loop awaited the handshake
/// response inline, so the agent's `fs/read_text_file` reached the daemon as a
/// `ServerCall` but the answering `ServerResult` could never be read: the
/// agent waited on the runner and the runner waited on the agent. It was also
/// unrecoverable, because the accept loop awaits the connection handler, so no
/// later daemon could attach either.
///
/// Asserts the session completes, which it cannot do if the loop parks.
#[test]
#[serial_test::parallel]
fn agent_request_during_session_new_does_not_deadlock_the_runner() {
    if cfg!(not(unix)) {
        return;
    }
    let Some(python3) = find_python3() else {
        eprintln!("skipping: python3 not found for fake ACP agent");
        return;
    };

    let scratch = Scratch::new("dl");
    let home = scratch.0.join("home");
    let xdg = scratch.0.join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();

    // Answers `initialize` immediately, but on `session/new` first issues its
    // own `fs/read_text_file` at the client and waits for the answer before
    // replying. An agent doing setup work through the client's fs capability
    // behaves exactly like this.
    let agent_py = scratch.0.join("deadlock_agent.py");
    std::fs::write(
        &agent_py,
        r#"
import sys, json

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

pending_session = None
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"protocolVersion": 1}})
    elif method == "session/new":
        # Ask the client for a file BEFORE answering. This is the deadlock.
        pending_session = msg["id"]
        send({"jsonrpc": "2.0", "id": 9001, "method": "fs/read_text_file",
              "params": {"path": "/tmp/setup"}})
    elif method == "session/prompt":
        send({"jsonrpc": "2.0", "id": msg["id"],
              "result": {"stopReason": "end_turn"}})
    elif method is None and msg.get("id") == 9001 and pending_session is not None:
        # Our fs request was answered; now we can finish session/new.
        send({"jsonrpc": "2.0", "id": pending_session,
              "result": {"sessionId": "sess-dl"}})
        pending_session = None
"#,
    )
    .unwrap();

    let session_id = "sdl00001";
    let workers = app_dir(&home, &xdg).join("acp-workers");
    let control = workers.join(format!("{session_id}.control.sock"));
    let record = workers.join(format!("{session_id}.json"));

    let _child = KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_aoe"))
            .args([
                "__acp-runner",
                "--socket",
                workers.join(format!("{session_id}.sock")).to_str().unwrap(),
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
    assert_eq!(read_frame(&mut ctl)["kind"], "hello");
    write_frame(
        &mut ctl,
        &serde_json::json!({"kind": "attach", "control_protocol_version": 3}),
    );
    write_frame(
        &mut ctl,
        &serde_json::json!({"kind": "initialize", "request": {"protocolVersion": 1}}),
    );
    assert_eq!(read_typed_frame(&mut ctl)["kind"], "initialized");

    write_frame(
        &mut ctl,
        &serde_json::json!({
            "kind": "establish_session",
            "method": "session/new",
            "request": {"cwd": home.to_str().unwrap()},
        }),
    );

    // The agent's mid-handshake request must reach us as a ServerCall. Pre-fix
    // this arrived fine; it is the answer that could not be read.
    let call = read_typed_frame(&mut ctl);
    assert_eq!(call["kind"], "server_call", "got {call}");
    assert_eq!(call["method"], "fs/read_text_file");
    write_frame(
        &mut ctl,
        &serde_json::json!({
            "kind": "server_result",
            "call_id": call["call_id"],
            "result": {"content": "setup data"},
        }),
    );

    // Pre-fix this read times out: the runner never processed the
    // server_result, so the agent never finished session/new.
    let ready = read_typed_frame(&mut ctl);
    assert_eq!(
        ready["kind"], "session_ready",
        "the handshake must complete once the agent's own request is answered: {ready}"
    );
    assert_eq!(ready["acp_session_id"], "sess-dl");
    let persisted: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&record).expect("read worker record"))
            .expect("parse worker record");
    assert_eq!(
        persisted["stored_acp_session_id"], "sess-dl",
        "session identity must be durable before SessionReady"
    );
    write_frame(
        &mut ctl,
        &serde_json::json!({"kind": "prompt", "request": {"sessionId": "sess-dl", "prompt": []}}),
    );
    let started = read_typed_frame(&mut ctl);
    assert_eq!(started["kind"], "prompt_started");
    let completion = read_typed_frame(&mut ctl);
    assert_eq!(completion["prompt_req_id"], started["prompt_req_id"]);
    assert_eq!(completion["kind"], "prompt_completed");
    drop(ctl);

    let mut resumed = UnixStream::connect(&control).expect("reconnect control socket");
    resumed
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    assert_eq!(read_frame(&mut resumed)["kind"], "hello");
    write_frame(
        &mut resumed,
        &serde_json::json!({"kind": "attach", "control_protocol_version": 3}),
    );
    assert_eq!(
        read_typed_frame(&mut resumed),
        completion,
        "a flushed completion must replay after daemon loss"
    );
}

/// Resolve python3 through PATH first, retaining the common absolute fallbacks
/// for stripped-down test environments.
fn find_python3() -> Option<PathBuf> {
    which::which("python3").ok().or_else(|| {
        [
            "/usr/bin/python3",
            "/opt/homebrew/bin/python3",
            "/usr/local/bin/python3",
        ]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
    })
}

#[tokio::test]
#[serial_test::parallel]
async fn cancelled_attach_reaps_runner_and_replacement_survives_load_fallback() {
    if cfg!(not(unix)) {
        return;
    }
    let Some(python3) = find_python3() else {
        eprintln!("skipping: python3 not found for fake ACP agent");
        return;
    };

    let scratch = Scratch::new("latehs");
    let home = scratch.0.join("home");
    let xdg = scratch.0.join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();

    let agent_log = scratch.0.join("agent-methods.log");
    let agent_pid_file = scratch.0.join("agent.pid");
    let partial_read = scratch.0.join("partial-read");
    let agent_py = scratch.0.join("delayed_agent.py");
    std::fs::write(
        &agent_py,
        r#"
import json, os, sys, time
with open(os.environ["AOE_FAKE_AGENT_PID"], "w") as f:
    f.write(str(os.getpid()))
for line in sys.stdin:
    try:
        msg = json.loads(line)
    except Exception:
        continue
    method = msg.get("method")
    mid = msg.get("id")
    if method is None or mid is None:
        continue
    with open(os.environ["AOE_FAKE_AGENT_LOG"], "a") as f:
        f.write(method + "\n")
    if method == "initialize":
        time.sleep(int(os.environ["AOE_FAKE_INIT_DELAY_MS"]) / 1000)
        result = {"protocolVersion": 1, "agentCapabilities": {"loadSession": True, "promptCapabilities": {}}}
    elif method == "session/load" and os.environ["AOE_FAKE_LOAD_ERROR"] == "1":
        error = {"code": -32000, "message": "stored session unavailable"}
        sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "error": error}) + "\n")
        sys.stdout.flush()
        continue
    elif method == "session/new":
        result = {"sessionId": "fresh-thread"}
    else:
        result = {}
    sys.stdout.write(json.dumps({"jsonrpc": "2.0", "id": mid, "result": result}) + "\n")
    sys.stdout.flush()
"#,
    )
    .unwrap();

    let session_id = "slate001";
    let workers = app_dir(&home, &xdg).join("acp-workers");
    let socket = workers.join(format!("{session_id}.sock"));
    let control = workers.join(format!("{session_id}.control.sock"));
    let record = workers.join(format!("{session_id}.json"));
    let bin = env!("CARGO_BIN_EXE_aoe");
    let spawn_runner = |delay: &str, fail_load: bool| {
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
            .env("AOE_FAKE_AGENT_PID", &agent_pid_file)
            .env("AOE_FAKE_INIT_DELAY_MS", delay)
            .env("AOE_E2E_PARTIAL_FRAME_FILE", &partial_read)
            .env("AOE_FAKE_LOAD_ERROR", if fail_load { "1" } else { "0" })
            .env("AOE_ACP_WATCHDOG_POLL_MS", "5000")
            .spawn()
            .expect("spawn acp runner")
    };

    let mut old = KillOnDrop(spawn_runner("2000", false));
    wait_for(&record, "old registry record");
    wait_for(&control, "old control socket");
    assert!(
        !socket.exists(),
        "a v3 runner must not bind the retired raw socket at {}",
        socket.display()
    );
    let old_agent_pid = wait_for_u32(&agent_pid_file, "old agent pid");

    let attach = async {
        tokio::time::timeout(
            Duration::from_millis(500),
            AcpClient::attach(
                socket.clone(),
                home.clone(),
                vec![],
                "stored-codex-thread".into(),
                false,
                AcpSessionId(session_id.into()),
                None,
                "fake-agent".into(),
                None,
            ),
        )
        .await
    };
    let spawn_replacement = async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !std::fs::read_to_string(&agent_log)
            .unwrap_or_default()
            .lines()
            .any(|method| method == "initialize")
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "old runner never began initialize"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let replacement = KillOnDrop(spawn_runner("750", true));
        wait_for_record_pid(&record, replacement.0.id());
        replacement
    };
    let (attach, mut replacement) = tokio::join!(attach, spawn_replacement);
    assert!(
        attach.is_err(),
        "delayed initialize must exceed the attach budget"
    );
    wait_for_runner_exit(&mut old.0);
    assert!(
        !agent_of_empires::process::worker_registry::is_pid_alive(old_agent_pid),
        "timed-out agent {old_agent_pid} stayed live"
    );
    assert!(
        replacement
            .0
            .try_wait()
            .expect("inspect replacement")
            .is_none()
            && record.exists()
            && control.exists(),
        "old runner teardown removed the replacement runner's files"
    );

    let baseline = std::fs::read_to_string(&agent_log)
        .unwrap_or_default()
        .lines()
        .filter(|method| *method == "initialize")
        .count();
    let mut ctl = UnixStream::connect(&control).expect("connect replacement control");
    ctl.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    assert_eq!(read_frame(&mut ctl)["kind"], "hello");
    write_frame(
        &mut ctl,
        &serde_json::json!({"kind": "attach", "control_protocol_version": 3}),
    );
    write_frame(
        &mut ctl,
        &serde_json::json!({"kind": "initialize", "request": {"protocolVersion": 1}}),
    );
    let load = encode_frame(&serde_json::json!({
        "kind": "establish_session",
        "method": "session/load",
        "request": {"sessionId": "stored-codex-thread", "cwd": home.to_str().unwrap()}
    }));
    let _ = std::fs::remove_file(&partial_read);
    ctl.write_all(&load[..2]).expect("write partial frame");
    ctl.flush().expect("flush partial frame");
    let partial_deadline = Instant::now() + Duration::from_secs(10);
    while std::fs::read_to_string(&partial_read).ok().as_deref() != Some("2") {
        assert!(
            Instant::now() < partial_deadline,
            "runner did not consume partial header"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(read_frame(&mut ctl)["kind"], "initialized");
    ctl.write_all(&load[2..]).expect("finish partial frame");
    ctl.flush().expect("flush completed frame");
    assert_eq!(read_frame(&mut ctl)["kind"], "handshake_failed");
    assert!(
        replacement
            .0
            .try_wait()
            .expect("inspect replacement")
            .is_none()
            && record.exists()
            && control.exists(),
        "recoverable session/load error tore down the replacement runner"
    );
    write_frame(
        &mut ctl,
        &serde_json::json!({
            "kind": "establish_session",
            "method": "session/new",
            "request": {"cwd": home.to_str().unwrap()}
        }),
    );
    let ready = read_frame(&mut ctl);
    assert_eq!(ready["kind"], "session_ready");
    assert_eq!(ready["acp_session_id"], "fresh-thread");

    let methods = std::fs::read_to_string(&agent_log).unwrap_or_default();
    assert_eq!(
        methods.lines().filter(|m| *m == "initialize").count(),
        baseline + 1
    );
    assert_eq!(methods.lines().filter(|m| *m == "session/load").count(), 1);
    assert_eq!(methods.lines().filter(|m| *m == "session/new").count(), 1);

    drop(replacement);
}

/// The runner owns the ACP handshake. Drive it as a v3
/// daemon over the control channel across two attaches and assert the
/// agent is handshaken (initialize + session/new) exactly once, that the
/// second attach replays the cache without touching the agent, and that a
/// prompt completes natively.
#[test]
#[serial_test::parallel]
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

    let v3 = serde_json::json!(3);

    // --- First attach: the runner runs the handshake against the agent. ---
    {
        let mut ctl = UnixStream::connect(&control).expect("connect control socket");
        ctl.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let hello = read_frame(&mut ctl);
        assert_eq!(hello["kind"], "hello");
        assert_eq!(hello["control_protocol_version"], v3);

        write_frame(
            &mut ctl,
            &serde_json::json!({"kind": "attach", "control_protocol_version": 3}),
        );
        write_frame(
            &mut ctl,
            &serde_json::json!({"kind": "initialize", "request": {"protocolVersion": 1}}),
        );
        let initialized = read_typed_frame(&mut ctl);
        assert_eq!(initialized["kind"], "initialized");
        assert!(initialized["result"].is_object());

        write_frame(
            &mut ctl,
            &serde_json::json!({"kind": "establish_session", "method": "session/new", "request": {"cwd": home.to_str().unwrap()}}),
        );
        let ready = read_typed_frame(&mut ctl);
        assert_eq!(ready["kind"], "session_ready");
        assert_eq!(ready["acp_session_id"], "sess-fake-1");

        write_frame(
            &mut ctl,
            &serde_json::json!({"kind": "prompt", "request": {"sessionId": "sess-fake-1", "prompt": []}}),
        );
        let started = read_typed_frame(&mut ctl);
        assert_eq!(started["kind"], "prompt_started");
        let completed = read_typed_frame(&mut ctl);
        assert_eq!(completed["prompt_req_id"], started["prompt_req_id"]);
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
            &serde_json::json!({"kind": "attach", "control_protocol_version": 3}),
        );
        let replayed = read_typed_frame(&mut ctl);
        assert_eq!(replayed["kind"], "prompt_completed");
        assert_eq!(replayed["outcome"]["stop_reason"], "end_turn");

        write_frame(
            &mut ctl,
            &serde_json::json!({"kind": "initialize", "request": {"protocolVersion": 1}}),
        );
        let initialized = read_typed_frame(&mut ctl);
        assert_eq!(initialized["kind"], "initialized");

        write_frame(
            &mut ctl,
            &serde_json::json!({"kind": "establish_session", "method": "session/new", "request": {}}),
        );
        let ready = read_typed_frame(&mut ctl);
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

/// A standard `session/load` response has no `sessionId`: the requested id is
/// already the identity being reopened. Codex also streams historical updates
/// before answering the load. The runner must accept that response, retain the
/// requested id for cancel, and replay the raw response from its handshake
/// cache without issuing either a second load or a fallback session/new.
#[test]
#[serial_test::parallel]
fn runner_load_uses_requested_id_and_caches_response() {
    if cfg!(not(unix)) {
        return;
    }
    let scratch = Scratch::new("hsload");
    let home = scratch.0.join("home");
    let xdg = scratch.0.join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();

    let agent_log = scratch.0.join("agent-methods.log");
    let fake_agent =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("web/tests/helpers/fakeAcpAgent.mjs");

    let session_id = "shsload1";
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
                "fake-codex-acp",
                "--cwd",
                home.to_str().unwrap(),
                "--",
                "node",
                fake_agent.to_str().unwrap(),
            ])
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &xdg)
            .env("FAKE_ACP_DEBUG_LOG", &agent_log)
            .env("FAKE_ACP_IMPERSONATE", "codex")
            .env("FAKE_ACP_LOAD_REPLAY", "old agent answer")
            .env("FAKE_ACP_LOAD_REPLAY_USER", "old user prompt")
            .env("FAKE_ACP_LOAD_REPLAY_BEFORE_RESPONSE", "1")
            .env("AOE_ACP_WATCHDOG_POLL_MS", "150")
            .spawn()
            .expect("spawn acp runner"),
    );

    wait_for(&record, "registry record");
    wait_for(&control, "control socket");

    {
        let mut ctl = UnixStream::connect(&control).expect("connect control socket");
        ctl.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        assert_eq!(read_frame(&mut ctl)["kind"], "hello");
        write_frame(
            &mut ctl,
            &serde_json::json!({"kind": "attach", "control_protocol_version": 3}),
        );
        write_frame(
            &mut ctl,
            &serde_json::json!({
                "kind": "establish_session",
                "method": "session/load",
                "request": {"sessionId": "existing-session", "cwd": home.to_str().unwrap()}
            }),
        );

        let ready = read_typed_frame(&mut ctl);
        assert_eq!(ready["kind"], "session_ready", "load must succeed: {ready}");
        assert_eq!(ready["acp_session_id"], "existing-session");
        assert!(ready["result"].get("sessionId").is_none());

        write_frame(&mut ctl, &serde_json::json!({"kind": "cancel"}));
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let methods = std::fs::read_to_string(&agent_log).unwrap_or_default();
            if methods.contains("session/cancel sessionId=existing-session") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "cancel must address the loaded session: {methods:?}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    // Reattach and repeat the handshake inputs. Both responses must come from
    // the runner cache, preserving the original raw load result and id.
    {
        let mut ctl = UnixStream::connect(&control).expect("reconnect control socket");
        ctl.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        assert_eq!(read_frame(&mut ctl)["kind"], "hello");
        write_frame(
            &mut ctl,
            &serde_json::json!({"kind": "attach", "control_protocol_version": 3}),
        );
        write_frame(
            &mut ctl,
            &serde_json::json!({
                "kind": "establish_session",
                "method": "session/load",
                "request": {"sessionId": "existing-session", "cwd": home.to_str().unwrap()}
            }),
        );
        let ready = read_typed_frame(&mut ctl);
        assert_eq!(ready["kind"], "session_ready");
        assert_eq!(ready["acp_session_id"], "existing-session");
        assert!(ready["result"].get("sessionId").is_none());
    }

    let methods = std::fs::read_to_string(&agent_log).unwrap();
    assert_eq!(
        methods
            .lines()
            .filter(|line| line.contains("handleRequest method=session/load"))
            .count(),
        1
    );
    assert_eq!(
        methods
            .lines()
            .filter(|line| line.contains("handleRequest method=session/new"))
            .count(),
        0
    );
}

/// When the agent answers session/new with a
/// JSON-RPC error, the runner forwards the FULL error object (including
/// `data`) in `HandshakeFailed`, so the daemon can reconstruct the crate
/// error and surface the same `data.details` remediation banner the
/// direct stdio path does. Guards the startup-error-banner live test at the
/// runner layer.
#[test]
#[serial_test::parallel]
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
        &serde_json::json!({"kind": "attach", "control_protocol_version": 3}),
    );
    write_frame(
        &mut ctl,
        &serde_json::json!({"kind": "initialize", "request": {"protocolVersion": 1}}),
    );
    let initialized = read_typed_frame(&mut ctl);
    assert_eq!(initialized["kind"], "initialized");

    write_frame(
        &mut ctl,
        &serde_json::json!({"kind": "establish_session", "method": "session/new", "request": {}}),
    );
    let failed = read_typed_frame(&mut ctl);
    assert_eq!(failed["kind"], "handshake_failed");
    // The remediation detail survives the control channel intact.
    assert_eq!(
        failed["error"]["data"]["details"],
        "native binary failed to launch"
    );
    assert_eq!(failed["error"]["code"], -32603);
}

/// The replacement daemon must wait for an already-sent reset and use its
/// committed identity, without loading or creating another agent session.
#[tokio::test]
#[serial_test::parallel]
async fn resumed_client_uses_reset_committed_after_reattach() {
    use agent_of_empires::acp::control_protocol::{self, ControlBody};
    use agent_of_empires::acp::state::Event;

    let Some(python3) = find_python3() else {
        return;
    };
    let scratch = Scratch::new("reset-resume");
    let home = scratch.0.join("home");
    let xdg = scratch.0.join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();
    let received = scratch.0.join("reset-received");
    let release = scratch.0.join("release-reset");
    let agent = scratch.0.join("agent.py");
    std::fs::write(&agent, r#"import json, sys, pathlib, time
received, release = map(pathlib.Path, sys.argv[1:])
count = 0
sid = None
def send(msg):
    print(json.dumps(msg), flush=True)
for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialize":
        result = {"protocolVersion":1,"agentCapabilities":{}}
    elif method == "session/new":
        count += 1
        if count == 2:
            received.write_text("received")
            while not release.exists(): time.sleep(0.01)
        sid = "session-" + str(count)
        result = {"sessionId":sid}
    elif method == "session/prompt":
        actual = msg["params"]["sessionId"]
        send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":sid,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":actual + ":new-count=" + str(count)}}}})
        result = {"stopReason":"end_turn"}
    else:
        raise RuntimeError("unexpected agent method: " + str(method))
    send({"jsonrpc":"2.0","id":msg["id"],"result":result})
"#).unwrap();
    let session = "reset-resume";
    let socket = scratch.0.join(format!("{session}.sock"));
    let control = agent_of_empires::process::worker::control_socket_sibling(&socket);
    let _runner = KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_aoe"))
            .args([
                "__acp-runner",
                "--socket",
                socket.to_str().unwrap(),
                "--session-id",
                session,
                "--agent-name",
                "review-agent",
                "--cwd",
                home.to_str().unwrap(),
                "--",
                python3.to_str().unwrap(),
                agent.to_str().unwrap(),
                received.to_str().unwrap(),
                release.to_str().unwrap(),
            ])
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &xdg)
            .spawn()
            .unwrap(),
    );
    wait_for(&control, "control socket");
    let mut first = tokio::net::UnixStream::connect(&control).await.unwrap();
    assert!(matches!(
        control_protocol::read_frame(&mut first).await.unwrap(),
        Some(ControlBody::Hello { .. })
    ));
    control_protocol::write_frame(
        &mut first,
        &ControlBody::Attach {
            control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
        },
    )
    .await
    .unwrap();
    control_protocol::write_frame(
        &mut first,
        &ControlBody::Initialize {
            request: serde_json::json!({"protocolVersion":1}),
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        control_protocol::read_frame(&mut first).await.unwrap(),
        Some(ControlBody::Initialized { .. })
    ));
    control_protocol::write_frame(
        &mut first,
        &ControlBody::EstablishSession {
            method: "session/new".into(),
            request: serde_json::json!({"cwd":home,"mcpServers":[]}),
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        control_protocol::read_frame(&mut first).await.unwrap(),
        Some(ControlBody::SessionReady { .. })
    ));
    control_protocol::write_frame(
        &mut first,
        &ControlBody::AgentCall {
            call_id: 77,
            method: "session/new".into(),
            params: serde_json::json!({"cwd":home,"mcpServers":[]}),
        },
    )
    .await
    .unwrap();
    wait_for(&received, "agent received reset");
    drop(first);

    let mut resumed = AcpClient::attach(
        socket,
        home,
        vec![],
        "session-1".into(),
        false,
        AcpSessionId(session.into()),
        None,
        "review-agent".into(),
        None,
    )
    .await
    .unwrap();
    // Initialization has replayed on the replacement connection. Release the
    // old daemon's reset only now, then verify what the consumer addresses.
    std::fs::write(&release, "release").unwrap();
    resumed.send_prompt("after reset", &[]).await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut assigned = None;
    let mut addressed = None;
    while assigned.is_none() || addressed.is_none() {
        match tokio::time::timeout_at(deadline, resumed.next_event())
            .await
            .unwrap()
            .unwrap()
        {
            Event::AcpSessionAssigned { acp_session_id } => assigned = Some(acp_session_id),
            Event::AgentMessageChunk { text } => addressed = Some(text),
            _ => {}
        }
    }
    resumed.shutdown().await.unwrap();
    assert_eq!(assigned.as_deref(), Some("session-2"));
    assert_eq!(addressed.as_deref(), Some("session-2:new-count=2"));
}

/// A daemon reattaching while a turn is still streaming must not be killed.
/// The runner announces the cached identity ahead of the detached backlog, so
/// the daemon commits identity and routes the flushed backlog plus continuing
/// live traffic directly instead of buffering unbounded pre-identity traffic.
/// Before the fix a full backlog plus live streaming overflowed the pending
/// replay bound ("pending native session updates exceed replay capacity") and
/// the reconciler terminated the worker. See #3937.
#[tokio::test]
#[serial_test::parallel]
async fn streaming_during_reattach_does_not_overflow_pending_replay() {
    use agent_of_empires::acp::control_protocol::{self, ControlBody};
    use agent_of_empires::acp::state::Event;

    let Some(python3) = find_python3() else {
        return;
    };
    let scratch = Scratch::new("stream-reattach");
    let home = scratch.0.join("home");
    let xdg = scratch.0.join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();
    let trigger = scratch.0.join("flood-trigger");
    let flooded = scratch.0.join("flooded");
    let agent = scratch.0.join("agent.py");
    // On session/new the agent mints its id and blocks until the test has
    // detached, then floods a full detached backlog and keeps streaming one
    // update every ~2ms across the reattach handshake. The detached reverse
    // call is answered only after the runner has read the whole backlog.
    std::fs::write(
        &agent,
        r#"import json, sys, pathlib, time
trigger, flooded = map(pathlib.Path, sys.argv[1:])
sid = None
def send(msg):
    print(json.dumps(msg), flush=True)
def chunk(text):
    send({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":sid,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text}}}})
for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":msg["id"],"result":{"protocolVersion":1,"agentCapabilities":{}}})
    elif method == "session/new":
        sid = "stream-session"
        send({"jsonrpc":"2.0","id":msg["id"],"result":{"sessionId":sid}})
        while not trigger.exists(): time.sleep(0.01)
        for _ in range(5000): chunk("backlog")
        send({"jsonrpc":"2.0","id":"barrier","method":"x/barrier","params":{}})
        sys.stdin.readline()
        flooded.write_text("flooded")
        while True:
            chunk("live")
            time.sleep(0.002)
    else:
        send({"jsonrpc":"2.0","id":msg["id"],"result":{}})
"#,
    )
    .unwrap();
    let session = "stream-reattach";
    let socket = scratch.0.join(format!("{session}.sock"));
    let control = agent_of_empires::process::worker::control_socket_sibling(&socket);
    let _runner = KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_aoe"))
            .args([
                "__acp-runner",
                "--socket",
                socket.to_str().unwrap(),
                "--session-id",
                session,
                "--agent-name",
                "stream-agent",
                "--cwd",
                home.to_str().unwrap(),
                "--",
                python3.to_str().unwrap(),
                agent.to_str().unwrap(),
                trigger.to_str().unwrap(),
                flooded.to_str().unwrap(),
            ])
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &xdg)
            .spawn()
            .unwrap(),
    );
    wait_for(&control, "control socket");

    // First daemon: establish the native session, then detach.
    let mut first = tokio::net::UnixStream::connect(&control).await.unwrap();
    assert!(matches!(
        control_protocol::read_frame(&mut first).await.unwrap(),
        Some(ControlBody::Hello { .. })
    ));
    control_protocol::write_frame(
        &mut first,
        &ControlBody::Attach {
            control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
        },
    )
    .await
    .unwrap();
    control_protocol::write_frame(
        &mut first,
        &ControlBody::Initialize {
            request: serde_json::json!({"protocolVersion":1}),
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        control_protocol::read_frame(&mut first).await.unwrap(),
        Some(ControlBody::Initialized { .. })
    ));
    control_protocol::write_frame(
        &mut first,
        &ControlBody::EstablishSession {
            method: "session/new".into(),
            request: serde_json::json!({"cwd":home,"mcpServers":[]}),
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        control_protocol::read_frame(&mut first).await.unwrap(),
        Some(ControlBody::SessionReady { .. })
    ));
    drop(first);

    // With no daemon attached, flood a full detached backlog and keep
    // streaming, so the reattach faces backlog plus live traffic.
    std::fs::write(&trigger, "go").unwrap();
    wait_for(&flooded, "runner consumed detached backlog");

    let mut resumed = AcpClient::attach(
        socket,
        home,
        vec![],
        "stream-session".into(),
        false,
        AcpSessionId(session.into()),
        None,
        "stream-agent".into(),
        None,
    )
    .await
    .unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut assigned = None;
    let mut chunks = 0usize;
    while assigned.is_none() || chunks == 0 {
        match tokio::time::timeout_at(deadline, resumed.next_event())
            .await
            .unwrap()
            .unwrap()
        {
            Event::AcpSessionAssigned { acp_session_id } => assigned = Some(acp_session_id),
            Event::AgentMessageChunk { .. } => chunks += 1,
            Event::AgentStartupError { .. } => {
                panic!("reattach overflowed pending replay and the worker was killed")
            }
            _ => {}
        }
    }
    resumed.shutdown().await.unwrap();
    assert_eq!(assigned.as_deref(), Some("stream-session"));
}

#[tokio::test]
#[serial_test::parallel]
async fn resumed_prompt_completes_only_for_its_own_runner_request() {
    use agent_of_empires::acp::state::Event;

    let Some(python3) = find_python3() else {
        return;
    };
    for old_first in [true, false] {
        let scratch = Scratch::new(if old_first {
            "prompt-old-first"
        } else {
            "prompt-new-first"
        });
        let home = scratch.0.join("home");
        let xdg = scratch.0.join("xdg");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&xdg).unwrap();
        let received = scratch.0.join("old-received");
        let agent = scratch.0.join("agent.py");
        std::fs::write(
            &agent,
            r#"import json, sys, pathlib
received = pathlib.Path(sys.argv[1])
old_first = sys.argv[2] == "true"
old = None
count = 0
def reply(req, reason):
    print(json.dumps({"jsonrpc":"2.0","id":req["id"],"result":{"stopReason":reason}}), flush=True)
for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialize":
        result = {"protocolVersion":1,"agentCapabilities":{}}
    elif method == "session/new":
        result = {"sessionId":"prompt-session"}
    elif method == "session/prompt":
        count += 1
        if count == 1:
            old = msg
            received.write_text("received")
        elif count == 2:
            responses = [(old, "cancelled"), (msg, "max_tokens")]
            for req, reason in responses if old_first else reversed(responses):
                reply(req, reason)
        else:
            reply(msg, "end_turn")
        continue
    elif method == "session/cancel":
        continue
    else:
        raise RuntimeError("unexpected method: " + str(method))
    print(json.dumps({"jsonrpc":"2.0","id":msg["id"],"result":result}), flush=True)
"#,
        )
        .unwrap();
        let session = "prompt-correlation";
        let socket = scratch.0.join(format!("{session}.sock"));
        let control = agent_of_empires::process::worker::control_socket_sibling(&socket);
        let _runner = KillOnDrop(
            Command::new(env!("CARGO_BIN_EXE_aoe"))
                .args([
                    "__acp-runner",
                    "--socket",
                    socket.to_str().unwrap(),
                    "--session-id",
                    session,
                    "--agent-name",
                    "review-agent",
                    "--cwd",
                    home.to_str().unwrap(),
                    "--",
                    python3.to_str().unwrap(),
                    agent.to_str().unwrap(),
                    received.to_str().unwrap(),
                    if old_first { "true" } else { "false" },
                ])
                .env("HOME", &home)
                .env("XDG_CONFIG_HOME", &xdg)
                .spawn()
                .unwrap(),
        );
        wait_for(&control, "control socket");
        let mut first = UnixStream::connect(&control).unwrap();
        first
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        assert_eq!(read_frame(&mut first)["kind"], "hello");
        write_frame(
            &mut first,
            &serde_json::json!({"kind":"attach","control_protocol_version":3}),
        );
        write_frame(
            &mut first,
            &serde_json::json!({"kind":"initialize","request":{"protocolVersion":1}}),
        );
        assert_eq!(read_typed_frame(&mut first)["kind"], "initialized");
        write_frame(
            &mut first,
            &serde_json::json!({"kind":"establish_session","method":"session/new","request":{"cwd":home,"mcpServers":[]}}),
        );
        assert_eq!(read_typed_frame(&mut first)["kind"], "session_ready");
        write_frame(
            &mut first,
            &serde_json::json!({"kind":"prompt","request":{"sessionId":"prompt-session","prompt":[]}}),
        );
        assert_eq!(read_typed_frame(&mut first)["kind"], "prompt_started");
        wait_for(&received, "old prompt received by agent");
        drop(first);

        let mut resumed = AcpClient::attach(
            socket,
            home,
            vec![],
            "prompt-session".into(),
            true,
            AcpSessionId(session.into()),
            None,
            "review-agent".into(),
            None,
        )
        .await
        .unwrap();
        for prompt in ["new prompt", "following prompt"] {
            resumed.send_prompt(prompt, &[]).await.unwrap();
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            loop {
                if let Event::Stopped { reason } =
                    tokio::time::timeout_at(deadline, resumed.next_event())
                        .await
                        .expect("prompt completion deadline")
                        .expect("event channel closed")
                {
                    assert_eq!(reason, "prompt_complete", "old_first={old_first}, {prompt}");
                    break;
                }
            }
        }
        resumed.shutdown().await.unwrap();
    }
}

#[test]
fn runner_native_identity_is_announced_before_callbacks_and_never_reassigned() {
    let Some(python3) = find_python3() else {
        eprintln!("skipping: python3 not found for native identity protocol agent");
        return;
    };
    let scratch = Scratch::new("nativeidentity");
    let home = scratch.0.join("home");
    let xdg = scratch.0.join("xdg");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&xdg).unwrap();
    let agent = scratch.0.join("agent.py");
    let answers = scratch.0.join("answers.jsonl");
    std::fs::write(&agent, r#"import json, sys
count = 0
for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    if method is None:
        with open(sys.argv[1], "a") as output:
            output.write(json.dumps(message) + "\n")
        continue
    if method == "initialize":
        result = {"protocolVersion":1,"agentCapabilities":{}}
    elif method == "session/new":
        result = {"sessionId":"native-owner"}
    else:
        result = {}
    frames = [json.dumps({"jsonrpc":"2.0","id":message["id"],"result":result})]
    if method in ("session/new", "test/callback"):
        count += 1
        positional = method == "test/callback" and message["params"].get("positional")
        if positional:
            frames.append(json.dumps({"jsonrpc":"2.0","id":"connection-elicitation","method":"elicitation/create","params":{"mode":"form","message":"Connection authentication","requestId":message["id"],"requestedSchema":{"type":"object","properties":{}}}}))
        params = ["native-owner", "unused"] if positional else {"sessionId":"native-owner","path":"unused"}
        frames.append(json.dumps({"jsonrpc":"2.0","id":f"callback-{count}","method":"fs/read_text_file","params":params}))
    sys.stdout.write("\n".join(frames) + "\n")
    sys.stdout.flush()
"#).unwrap();
    let session = "nativeguard";
    let workers = app_dir(&home, &xdg).join("acp-workers");
    let socket = workers.join(format!("{session}.sock"));
    let control = workers.join(format!("{session}.control.sock"));
    let _child = KillOnDrop(
        Command::new(env!("CARGO_BIN_EXE_aoe"))
            .args([
                "__acp-runner",
                "--socket",
                socket.to_str().unwrap(),
                "--session-id",
                session,
                "--agent-name",
                "fake-agent",
                "--cwd",
                home.to_str().unwrap(),
                "--",
                python3.to_str().unwrap(),
                agent.to_str().unwrap(),
                answers.to_str().unwrap(),
            ])
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &xdg)
            .env("AOE_ACP_WATCHDOG_POLL_MS", "150")
            .spawn()
            .expect("spawn native-identity runner"),
    );
    wait_for(&control, "native-identity control socket");
    let attach = || {
        let mut stream = UnixStream::connect(&control).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        assert_eq!(read_frame(&mut stream)["kind"], "hello");
        write_frame(
            &mut stream,
            &serde_json::json!({"kind":"attach","control_protocol_version":3}),
        );
        stream
    };
    let initialize = |stream: &mut UnixStream| {
        write_frame(
            stream,
            &serde_json::json!({"kind":"initialize","request":{"protocolVersion":1}}),
        );
        assert_eq!(read_frame(stream)["kind"], "initialized");
    };
    let resume = |stream: &mut UnixStream| {
        write_frame(stream, &serde_json::json!({"kind":"resume_session"}));
        let ready = read_frame(stream);
        assert_eq!(ready["kind"], "session_ready");
        assert_eq!(ready["acp_session_id"], "native-owner");
    };
    let answer = |stream: &mut UnixStream, positional: bool| {
        let call = read_frame(stream);
        assert_eq!(call["kind"], "server_call");
        assert_eq!(call["method"], "fs/read_text_file");
        let expected = if positional {
            serde_json::json!(["native-owner", "unused"])
        } else {
            serde_json::json!({"sessionId":"native-owner","path":"unused"})
        };
        assert_eq!(call["params"], expected);
        write_frame(
            stream,
            &serde_json::json!({"kind":"server_result","call_id":call["call_id"],"result":{"content":"owned"}}),
        );
    };
    let trigger = |stream: &mut UnixStream, id| {
        write_frame(
            stream,
            &serde_json::json!({"kind":"agent_call","call_id":id,"method":"test/callback","params":{"positional":id == 2}}),
        );
        let reply = read_frame(stream);
        assert_eq!(reply["kind"], "agent_result");
        assert_eq!(reply["call_id"], id);
    };
    let assert_quiet = |stream: &mut UnixStream| {
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let error = stream
            .read(&mut [0])
            .expect_err("callback overtook native identity or changed attachment");
        assert!(matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ));
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
    };

    let mut first = attach();
    initialize(&mut first);
    write_frame(
        &mut first,
        &serde_json::json!({"kind":"establish_session","method":"session/new","request":{"cwd":home,"mcpServers":[]}}),
    );
    assert_eq!(read_frame(&mut first)["kind"], "session_ready");
    assert_eq!(read_frame(&mut first)["method"], "_aoe/session_replayed");
    answer(&mut first, false);
    drop(first);

    // A stable runner can emit callbacks before the replacement daemon asks
    // ResumeSession. Keep them on that attachment, behind its authority frame.
    let mut second = attach();
    trigger(&mut second, 2);
    // Request-scoped elicitation is independent of native session authority.
    let mut elicitation = read_frame(&mut second);
    assert_eq!(elicitation["kind"], "server_call");
    assert_eq!(elicitation["method"], "elicitation/create");
    let request: agent_client_protocol::schema::v1::CreateElicitationRequest =
        serde_json::from_value(elicitation["params"].take()).unwrap();
    assert!(matches!(
        request.scope(),
        agent_client_protocol::schema::v1::ElicitationScope::Request(_)
    ));
    write_frame(
        &mut second,
        &serde_json::json!({"kind":"server_result","call_id":elicitation["call_id"],"result":{"action":"cancel"}}),
    );
    assert_quiet(&mut second);
    initialize(&mut second);
    resume(&mut second);
    answer(&mut second, true);
    drop(second);

    // A callback waiting for a dead attachment must be answered as disconnected,
    // not retargeted to the next daemon simply because its native ID is equal.
    let mut third = attach();
    trigger(&mut third, 3);
    assert_quiet(&mut third);
    // A newly registered native reset must release the cached callback gate:
    // otherwise stdout cannot reach the reset response behind that callback.
    write_frame(
        &mut third,
        &serde_json::json!({"kind":"agent_call","call_id":99,"method":"session/new","params":{"cwd":home,"mcpServers":[]}}),
    );
    answer(&mut third, false);
    let reset_reply = read_frame(&mut third);
    assert_eq!(reset_reply["kind"], "agent_result");
    assert_eq!(reset_reply["call_id"], 99);
    assert_quiet(&mut third);
    drop(third);
    let mut fourth = attach();
    initialize(&mut fourth);
    resume(&mut fourth);
    assert_quiet(&mut fourth);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let responses = std::fs::read_to_string(&answers).unwrap_or_default();
        if let Some(answer) = responses
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|answer| answer["id"] == "callback-4")
        {
            assert!(
                answer["error"].is_object(),
                "detached callback was not rejected: {answer}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "agent never received detached callback rejection"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
