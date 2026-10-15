//! Structured forks through the REST create endpoint against a live daemon and
//! the fake ACP agent, which mints `fake-acp-fork-<hex>` ids only from its
//! `session/fork` handler.

use std::time::Duration;

use serde_json::{json, Value};
use serial_test::parallel;

use crate::harness::{init_git_repo, require_node, require_tmux, wait_until, TuiTestHarness};

fn acp_id_by_title(h: &TuiTestHarness, title: &str) -> Option<String> {
    h.read_sessions()
        .as_array()?
        .iter()
        .find(|s| s["title"].as_str() == Some(title))
        .and_then(|s| s["acp_session_id"].as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The reconciler spawns workers without a prompt, so a persisted id means
/// the handshake completed.
fn wait_for_acp_id(h: &TuiTestHarness, title: &str) -> String {
    wait_until(Duration::from_secs(45), Duration::from_millis(250), || {
        acp_id_by_title(h, title).ok_or_else(|| {
            format!(
                "'{title}' has no acp_session_id.\nsessions.json: {}",
                h.try_read_sessions()
            )
        })
    })
}

fn post_create(port: u16, body: Value) -> (reqwest::StatusCode, Value) {
    tokio::runtime::Runtime::new()
        .expect("tokio runtime")
        .block_on(async move {
            let resp = reqwest::Client::new()
                .post(format!("http://127.0.0.1:{port}/api/sessions"))
                .json(&body)
                .send()
                .await
                .expect("POST /api/sessions");
            let status = resp.status();
            (status, resp.json().await.unwrap_or(Value::Null))
        })
}

#[test]
#[parallel]
fn structured_fork_mints_distinct_child_id_and_preserves_parent() {
    require_tmux!();
    require_node!();
    let h = TuiTestHarness::new_acp("fork_structured", "{}");
    let (port, _) = h.start_structured_session("ForkParent");
    let parent_acp_id = wait_for_acp_id(&h, "ForkParent");

    let parent_context_resume = tokio::runtime::Runtime::new()
        .expect("tokio runtime")
        .block_on(async {
            reqwest::get(format!("http://127.0.0.1:{port}/api/sessions"))
                .await
                .expect("GET /api/sessions")
                .json::<Value>()
                .await
                .expect("decode sessions response")["sessions"]
                .as_array()
                .and_then(|rows| rows.iter().find(|row| row["title"] == "ForkParent"))
                .map(|row| row["context_resume"].clone())
                .expect("ForkParent context_resume")
        });
    assert_eq!(
        parent_context_resume,
        json!({ "state": "available" }),
        "the negotiated loadSession capability must reach GET /api/sessions"
    );

    let (status, body) = post_create(
        port,
        json!({
            "title": "ForkChild",
            "path": h.project_path(),
            "tool": "claude",
            "view": "structured",
            "fork_from": parent_acp_id,
        }),
    );
    assert_eq!(status, reqwest::StatusCode::CREATED, "create body: {body}");

    let child_acp_id = wait_for_acp_id(&h, "ForkChild");
    assert_ne!(child_acp_id, parent_acp_id);
    // `session/new` ids lack the `fork-` segment.
    assert!(
        child_acp_id.contains("fork-"),
        "child id must come from session/fork, not session/new: {child_acp_id}"
    );
    assert_eq!(
        acp_id_by_title(&h, "ForkParent").as_deref(),
        Some(parent_acp_id.as_str())
    );
    let sessions = h.read_sessions();
    let child = crate::harness::session_by_title(&sessions, "ForkChild");
    assert!(
        child["fork_pending"].is_null(),
        "fork_pending must be cleared after the forked id is assigned, got: {:?}",
        child["fork_pending"]
    );
}

/// A rejected `session/fork` clears `fork_pending` instead of re-forking or
/// falling back to `session/new`, and leaves the parent untouched.
#[test]
#[parallel]
fn structured_fork_failure_clears_fork_pending_and_fails_cleanly() {
    require_tmux!();
    require_node!();
    let mut h = TuiTestHarness::new_in_tmp("fork_structured_fail");
    h.set_acp_fork_fail();
    let script_path = h.home_path().join("fork-script.json");
    std::fs::write(&script_path, "{}").expect("write fake-acp script");
    h.install_acp_shim(&script_path);
    h.stop_daemon_on_drop();

    let (port, _) = h.start_structured_session("FailParent");
    let parent_acp_id = wait_for_acp_id(&h, "FailParent");
    // Create succeeds; the fork fails later at the worker handshake.
    let (status, body) = post_create(
        port,
        json!({
            "title": "FailChild",
            "path": h.project_path(),
            "tool": "claude",
            "view": "structured",
            "fork_from": parent_acp_id,
        }),
    );
    assert_eq!(status, reqwest::StatusCode::CREATED, "create body: {body}");

    let child = wait_until(Duration::from_secs(45), Duration::from_millis(250), || {
        let sessions = h.try_read_sessions();
        sessions
            .as_array()
            .and_then(|rows| rows.iter().find(|s| s["title"] == "FailChild"))
            .filter(|child| child["fork_pending"].is_null())
            .cloned()
            .ok_or_else(|| format!("FailChild.fork_pending never cleared: {sessions}"))
    });
    assert!(
        child["acp_session_id"].as_str().is_none_or(str::is_empty),
        "a failed fork must not capture an acp_session_id, got: {:?}",
        child["acp_session_id"]
    );
    assert_eq!(
        acp_id_by_title(&h, "FailParent").as_deref(),
        Some(parent_acp_id.as_str())
    );
}

/// The create handler's fork mutex early returns, end to end.
#[test]
#[parallel]
fn create_handler_rejects_bad_fork_requests_with_400() {
    require_tmux!();
    require_node!();
    let h = TuiTestHarness::new_acp("fork_create_400", "{}");
    let project = h.project_path();
    init_git_repo(&project);
    let port = h.start_daemon();

    for (body, label) in [
        (
            json!({
                "title": "BadBoth", "path": project, "tool": "claude",
                "import_acp_session_id": "some-import-id", "fork_from": "parent-uuid_123",
            }),
            "both import and fork set",
        ),
        (
            json!({
                "title": "BadForkId", "path": project, "tool": "claude",
                "fork_from": "../etc/passwd",
            }),
            "malformed fork id",
        ),
        (
            json!({
                "title": "BadStructuredFork", "path": project, "tool": "aoe-agent",
                "view": "structured", "fork_from": "parent-uuid_123",
            }),
            "structured fork for a resume-only agent",
        ),
    ] {
        let (status, json) = post_create(port, body);
        assert_eq!(status, reqwest::StatusCode::BAD_REQUEST, "{label}");
        assert!(json["error"].as_str().is_some(), "{label}: {json}");
    }
}
