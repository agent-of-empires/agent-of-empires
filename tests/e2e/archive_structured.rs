//! Web archive handler teardown for a live structured session (#2185): the
//! worker always shuts down, tool sub-sessions die only with `kill_pane`, and
//! the transcript survives because no `session/delete` is sent (#1710).

use std::time::Duration;

use serial_test::parallel;

use crate::harness::{app_dir_in, require_node, require_tmux, wait_until, TuiTestHarness};

fn archive_case(name: &str, kill_pane: bool) {
    let h = TuiTestHarness::new_acp(
        name,
        r#"{ "turns": [ { "updates": [], "stopReason": "end_turn" } ] }"#,
    );
    let (port, session_id) = h.start_structured_session(name);
    h.prompt_until_accepted(&session_id, "hello", Duration::from_secs(30));

    // Named so `kill_ancillary_tmux_sessions()` sweeps it for this session.
    let tool_name = agent_of_empires::tmux::ToolSession::new(&session_id, name, "tooltest")
        .session_name()
        .to_string();
    h.tmux_new_detached(&tool_name, "sleep 600");
    assert!(h.tmux_has_session(&tool_name));

    tokio::runtime::Runtime::new()
        .expect("tokio runtime")
        .block_on(async {
            let resp = reqwest::Client::new()
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

    wait_until(Duration::from_secs(10), Duration::from_millis(200), || {
        let out = h.run_cli(&["ps", "--acp", "--dead", "--json"]);
        let body = String::from_utf8_lossy(&out.stdout);
        let records: serde_json::Value =
            serde_json::from_str(&body).unwrap_or(serde_json::json!([]));
        let listed = records
            .as_array()
            .is_some_and(|rs| rs.iter().any(|r| r["session_id"] == session_id));
        if listed {
            Err(format!("worker still listed after archive:\n{body}"))
        } else {
            Ok(())
        }
    });

    assert_eq!(
        h.tmux_has_session(&tool_name),
        !kill_pane,
        "tool sub-session survives only without kill_pane"
    );
    let log =
        std::fs::read_to_string(app_dir_in(h.home_path()).join("fake-acp.log")).unwrap_or_default();
    assert!(
        !log.contains("session/delete"),
        "archive must not fire session/delete:\n{log}"
    );
    let sessions = h.read_sessions();
    let row = sessions
        .as_array()
        .and_then(|rs| rs.iter().find(|r| r["id"] == session_id))
        .expect("session row kept on disk");
    assert!(row["archived_at"].as_str().is_some_and(|s| !s.is_empty()));
}

#[test]
#[parallel]
fn archive_kills_worker_and_tool_session() {
    require_tmux!();
    require_node!();
    archive_case("ArchiveStructKill", true);
}

#[test]
#[parallel]
fn archive_no_kill_shuts_worker_but_keeps_tool_session() {
    require_tmux!();
    require_node!();
    archive_case("ArchiveStructNoKill", false);
}
