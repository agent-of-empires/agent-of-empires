//! Full-binary e2e for the built-in attention/triage worker.
//!
//! Runs the real `aoe __plugin-attention` subcommand and drives one poll cycle:
//! it should request `sessions.list`, and once we feed a response back it should
//! push a `home-pane` board grouping the sessions by urgency plus a status-bar
//! count of the ones waiting on input. This covers the worker's request ->
//! parse -> push loop over the plugin protocol; the grouping itself is
//! unit-tested in `src/plugin/attention.rs`.
//!
//! Not serve-gated: the subcommand is always compiled (it talks to the host
//! over stdio with no serve dependency). Run via:
//!
//! ```sh
//! cargo test --features e2e-tests --test e2e -- plugin_attention_worker
//! ```

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{json, Value};
use serial_test::parallel;

#[test]
#[parallel]
fn plugin_attention_worker_requests_sessions_and_pushes_the_board() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_aoe"))
        .arg("__plugin-attention")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn aoe __plugin-attention");

    let mut stdin = child.stdin.take().expect("worker stdin");
    let stdout = child.stdout.take().expect("worker stdout");
    let (tx, rx) = mpsc::channel::<Value>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(msg) = serde_json::from_str::<Value>(&line) {
                let _ = tx.send(msg);
            }
        }
    });

    // First the worker asks the host for the session list.
    let request = recv_matching(&rx, |m| m["method"] == "sessions.list")
        .expect("worker requests sessions.list");
    let request_id = request["id"].clone();
    assert!(request_id.is_u64(), "the request carries an id: {request}");

    // Feed a response: one waiting session and one running.
    let response = json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "result": {"sessions": [
            {"id": "a", "title": "fix-the-bug", "project_path": "/repo", "status": "Waiting"},
            {"id": "b", "title": "run-tests", "project_path": "/repo", "status": "Running"},
        ]},
    });
    writeln!(stdin, "{response}").expect("feed sessions.list response");
    stdin.flush().ok();

    // The worker pushes the home-pane board reflecting those sessions.
    let board = recv_matching(&rx, |m| {
        m["method"] == "ui.state.set" && m["params"]["slot"] == "home-pane"
    })
    .expect("worker pushes a home-pane board");
    let _ = child.kill();
    let _ = child.wait();

    assert_eq!(board["params"]["id"], "board");
    let blocks = board["params"]["payload"]["blocks"]
        .as_array()
        .expect("board has blocks");
    let headings: Vec<&str> = blocks
        .iter()
        .filter(|b| b["kind"] == "heading")
        .map(|b| b["text"].as_str().unwrap_or(""))
        .collect();
    assert!(
        headings.iter().any(|h| h.starts_with("Needs input (1)")),
        "waiting session is bucketed: {headings:?}"
    );
    assert!(
        blocks
            .iter()
            .any(|b| b["kind"] == "row" && b["label"] == "fix-the-bug"),
        "the waiting session's row is present: {blocks:?}"
    );
}

/// Receive JSON messages until one matches `pred`, or time out.
fn recv_matching(rx: &mpsc::Receiver<Value>, pred: impl Fn(&Value) -> bool) -> Option<Value> {
    let deadline = Duration::from_secs(30);
    loop {
        let msg = rx.recv_timeout(deadline).ok()?;
        if pred(&msg) {
            return Some(msg);
        }
    }
}
