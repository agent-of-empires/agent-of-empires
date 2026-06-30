//! Full-stack e2e: the web archive handler's structured-view teardown branch
//! (`src/server/api/sessions.rs` `update_session_archive`, #1868 / #2179).
//!
//! The handler shuts the ACP worker down via `acp_supervisor.shutdown()`
//! (unconditional, regardless of `kill_pane`) and then, only when `kill_pane`
//! is set, calls `kill_ancillary_tmux_sessions()` to tear down the tool
//! sub-sessions. `shutdown()` deliberately does NOT fire `session/delete`, so
//! the agent transcript stays resumable (#1710).
//!
//! `archive_restore.rs` covers the four-kind tmux teardown via the CLI; the
//! Playwright live spec covers the dashboard path. Neither drives a real
//! ACP/structured-view session through the web archive handler, which is the
//! gap this test closes (#2185).
//!
//! Oracles:
//!   - Worker shut down: `aoe acp ps --json` no longer lists the session
//!     (`shutdown()` removes the registry record). Asserted in BOTH kill_pane
//!     modes, so it catches a regression that skips the unconditional shutdown
//!     when `kill_pane = false`.
//!   - Tool sub-session: a pre-created `aoe_tool_*` tmux session is killed when
//!     `kill_pane = true` and survives when `kill_pane = false`.
//!   - Transcript preserved: the shared fake-ACP agent logs every inbound RPC
//!     to `FAKE_ACP_DEBUG_LOG`. A regression that swaps `shutdown()` for
//!     `shutdown_and_delete()` dispatches `session/delete` unconditionally
//!     (`acp_client::delete_session` sends regardless of advertised
//!     capability), so asserting the log never contains `session/delete`
//!     locks the #1710 invariant. The session row also stays on disk with
//!     `archived_at` set (archived, not deleted).
//!
//! The shutdown -> ancillary-kill ORDER is not directly observable end-to-end
//! without intrusive hooks; it stays covered by the handler's code comment and
//! the `acp.rs:1304-1310` precedent.
//!
//! Compiled only with `--features serve`. Run via:
//!
//! ```sh
//! cargo test --features serve,e2e-tests --test e2e -- archive_structured
//! ```
#![cfg(feature = "serve")]

use std::process::Command;
use std::time::{Duration, Instant};

use serial_test::serial;

use crate::harness::{
    app_dir_in, pick_free_port, require_node, require_tmux, wait_for_port, TuiTestHarness,
};

/// One-turn fake-ACP script: complete the ACP handshake, accept a prompt, and
/// end the turn. The worker stays alive (idle) after the turn, so the archive
/// teardown has a live worker to shut down.
const SCRIPT: &str = r#"{
  "turns": [
    { "updates": [], "stopReason": "end_turn" }
  ]
}"#;

fn parse_session_id(add_stdout: &str) -> String {
    add_stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("ID:"))
        .map(|rest| rest.trim().to_string())
        .unwrap_or_else(|| panic!("could not find session ID in `aoe add` output:\n{add_stdout}"))
}

/// `aoe acp prompt` 404s until the worker is live and handshaked, so a
/// successful call is the readiness oracle: by the time it returns the worker
/// is running and (importantly for the `session/delete` oracle) has a stored
/// ACP session id in the registry.
fn prompt_until_accepted(h: &TuiTestHarness, session_id: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        let out = h.run_cli(&["acp", "prompt", session_id, "hello"]);
        if out.status.success() {
            return;
        }
        if Instant::now() >= deadline {
            let ps = h.run_cli(&["acp", "ps", "--json"]);
            panic!(
                "structured view worker never accepted a prompt within {timeout:?}.\n\
                 stdout: {}\nstderr: {}\nacp ps: {}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
                String::from_utf8_lossy(&ps.stdout),
            );
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// True while `aoe acp ps --json` still lists `session_id` as a live worker.
fn worker_listed(h: &TuiTestHarness, session_id: &str) -> bool {
    let out = h.run_cli(&["acp", "ps", "--json"]);
    let body = String::from_utf8_lossy(&out.stdout);
    let records: serde_json::Value = serde_json::from_str(&body).unwrap_or(serde_json::json!([]));
    records
        .as_array()
        .map(|rs| rs.iter().any(|r| r["session_id"] == session_id))
        .unwrap_or(false)
}

/// Poll until the worker disappears from `acp ps`. Panics with the last
/// listing on timeout.
fn wait_for_worker_gone(h: &TuiTestHarness, session_id: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if !worker_listed(h, session_id) {
            return;
        }
        if Instant::now() >= deadline {
            let ps = h.run_cli(&["acp", "ps", "--json"]);
            panic!(
                "worker {session_id} still listed after archive within {timeout:?}.\nacp ps: {}",
                String::from_utf8_lossy(&ps.stdout),
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn tmux_has_session(name: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn kill_tmux(name: &str) {
    let _ = Command::new("tmux")
        .args(["kill-session", "-t", name])
        .output();
}

/// Pre-create a tool sub-session whose name matches what
/// `kill_ancillary_tmux_sessions()` sweeps for `session_id` (by `_<id8>`
/// suffix). Created on the default tmux server, the same one the daemon's
/// teardown uses (it inherits the test process env). Mirrors the precedent in
/// `archive_restore.rs`.
fn create_tool_session(session_id: &str, title: &str) -> String {
    let name = agent_of_empires::tmux::ToolSession::new(session_id, title, "tooltest")
        .session_name()
        .to_string();
    let create = Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            &name,
            "-x",
            "80",
            "-y",
            "24",
            "sleep",
            "600",
        ])
        .output()
        .expect("tmux new-session");
    assert!(
        create.status.success(),
        "failed to create tool tmux session {name}: {}",
        String::from_utf8_lossy(&create.stderr)
    );
    name
}

/// PATCH `/api/sessions/:id/archive` with `{archived: true, kill_pane}` against
/// the no-auth daemon. Asserts a 2xx so a routing/handler regression fails the
/// test rather than silently skipping the teardown.
fn archive_via_api(port: u16, session_id: &str, kill_pane: bool) {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    rt.block_on(async {
        let client = reqwest::Client::new();
        let resp = client
            .patch(format!(
                "http://127.0.0.1:{port}/api/sessions/{session_id}/archive"
            ))
            .json(&serde_json::json!({ "archived": true, "kill_pane": kill_pane }))
            .send()
            .await
            .expect("PATCH archive send");
        assert!(
            resp.status().is_success(),
            "archive PATCH failed: {} {}",
            resp.status(),
            resp.text().await.unwrap_or_default(),
        );
    });
}

/// Read the archived_at for `session_id` from the isolated sessions.json.
fn archived_at(h: &TuiTestHarness, session_id: &str) -> Option<String> {
    let path = app_dir_in(h.home_path()).join("profiles/default/sessions.json");
    let content =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let rows: serde_json::Value = serde_json::from_str(&content).expect("invalid sessions JSON");
    rows.as_array()
        .and_then(|rs| rs.iter().find(|r| r["id"] == session_id))
        .and_then(|r| r["archived_at"].as_str())
        .map(|s| s.to_string())
}

/// Assert the fake-ACP agent never received a `session/delete` RPC, which
/// would mean the handler fired `shutdown_and_delete` and dropped the agent
/// transcript (#1710).
fn assert_no_session_delete(h: &TuiTestHarness) {
    let log = app_dir_in(h.home_path()).join("fake-acp.log");
    let body = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        !body.contains("session/delete"),
        "archive must NOT fire session/delete (transcript loss #1710).\nfake-acp.log:\n{body}"
    );
}

/// Stand up a live daemon with a structured-view session, drive a prompt so the
/// worker is live, pre-create a tool sub-session, and return the pieces the
/// per-mode assertions need.
fn setup(h: &mut TuiTestHarness, title: &str) -> (u16, String, String) {
    let script_path = h.home_path().join("archive-script.json");
    std::fs::write(&script_path, SCRIPT).expect("write fake-acp script");
    h.install_acp_shim(&script_path);
    h.stop_daemon_on_drop();

    // A structured view session needs a git repo as its workspace.
    let project = h.project_path();
    for args in [
        vec!["init", "-q"],
        vec!["commit", "--allow-empty", "-q", "-m", "init"],
    ] {
        let out = Command::new("git")
            .args(&args)
            .current_dir(&project)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let port = pick_free_port();
    let port_s = port.to_string();
    let start = h.run_cli(&["serve", "--daemon", "--port", &port_s, "--no-auth"]);
    assert!(
        start.status.success(),
        "aoe serve --daemon failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr),
    );
    assert!(
        wait_for_port(port, Duration::from_secs(10)),
        "daemon never bound port {port}"
    );

    let add = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        title,
        "-c",
        "claude",
        "--structured-view",
    ]);
    assert!(
        add.status.success(),
        "aoe add --structured-view failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&add.stdout),
        String::from_utf8_lossy(&add.stderr),
    );
    let session_id = parse_session_id(&String::from_utf8_lossy(&add.stdout));

    prompt_until_accepted(h, &session_id, Duration::from_secs(30));
    // Worker is live and registered. Pre-create the tool sub-session now so
    // archive's `kill_ancillary_tmux_sessions()` has something to find.
    let tool_name = create_tool_session(&session_id, title);

    (port, session_id, tool_name)
}

/// `kill_pane = true`: worker shut down, tool sub-session killed, transcript
/// preserved (no `session/delete`), session row archived on disk.
#[test]
#[serial]
fn archive_kills_worker_and_tool_session() {
    require_tmux!();
    require_node!();

    let mut h = TuiTestHarness::new_in_tmp("archive_structured_kill");
    let title = "ArchiveStructKill";
    let (port, session_id, tool_name) = setup(&mut h, title);

    assert!(
        tmux_has_session(&tool_name),
        "precondition: tool session {tool_name} should exist before archive"
    );

    archive_via_api(port, &session_id, true);

    wait_for_worker_gone(&h, &session_id, Duration::from_secs(10));

    let tool_alive = tmux_has_session(&tool_name);
    // Clean up before asserting so a failure can't leak into the next test.
    if tool_alive {
        kill_tmux(&tool_name);
    }
    assert!(
        !tool_alive,
        "tool sub-session {tool_name} must be killed by archive with kill_pane=true"
    );

    assert_no_session_delete(&h);
    assert!(
        archived_at(&h, &session_id).is_some_and(|s| !s.is_empty()),
        "session must be archived (not deleted) on disk"
    );
}

/// `kill_pane = false`: the worker is STILL shut down (unconditional), but the
/// tool sub-session survives. Transcript preserved, session row archived.
#[test]
#[serial]
fn archive_no_kill_shuts_worker_but_keeps_tool_session() {
    require_tmux!();
    require_node!();

    let mut h = TuiTestHarness::new_in_tmp("archive_structured_nokill");
    let title = "ArchiveStructNoKill";
    let (port, session_id, tool_name) = setup(&mut h, title);

    assert!(
        tmux_has_session(&tool_name),
        "precondition: tool session {tool_name} should exist before archive"
    );

    archive_via_api(port, &session_id, false);

    // Worker shutdown is unconditional, so it must be gone even with kill_pane=false.
    wait_for_worker_gone(&h, &session_id, Duration::from_secs(10));

    let tool_alive = tmux_has_session(&tool_name);
    // The survivor would pollute the next serial test; kill it before asserting.
    kill_tmux(&tool_name);
    assert!(
        tool_alive,
        "tool sub-session {tool_name} must survive archive with kill_pane=false"
    );

    assert_no_session_delete(&h);
    assert!(
        archived_at(&h, &session_id).is_some_and(|s| !s.is_empty()),
        "session must be archived (not deleted) on disk"
    );
}
