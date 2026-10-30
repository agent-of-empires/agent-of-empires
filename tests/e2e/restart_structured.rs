//! `POST /api/sessions/{id}/restart` on a live structured session replaces its
//! worker: the old runner stops, a new one starts, and it accepts a prompt.

use std::time::{Duration, Instant};

use serial_test::parallel;

use crate::harness::{require_node, require_tmux, wait_until, TuiTestHarness};

/// The runner pids `aoe ps --acp` lists as alive for the session.
fn alive_worker_pids(h: &TuiTestHarness, session_id: &str) -> Vec<u64> {
    let out = h.run_cli(&["ps", "--acp", "--json"]);
    let records: serde_json::Value =
        serde_json::from_slice(&out.stdout).unwrap_or(serde_json::json!([]));
    records
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["session_id"] == session_id && r["alive"] == true)
        .filter_map(|r| r["pid"].as_u64())
        .collect()
}

#[test]
#[parallel]
fn restart_replaces_a_live_structured_worker() {
    require_tmux!();
    require_node!();
    let h = TuiTestHarness::new_acp(
        "RestartStructured",
        r#"{ "turns": [ { "updates": [], "stopReason": "end_turn" } ] }"#,
    );
    let (port, session_id) = h.start_structured_session("RestartStructured");
    h.prompt_until_accepted(&session_id, "hello", Duration::from_secs(30));
    let before = wait_until(Duration::from_secs(10), Duration::from_millis(200), || {
        alive_worker_pids(&h, &session_id)
            .first()
            .copied()
            .ok_or_else(|| "no alive worker listed".to_string())
    });

    tokio::runtime::Runtime::new()
        .expect("tokio runtime")
        .block_on(async {
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client");
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let resp = client
                    .post(format!(
                        "http://127.0.0.1:{port}/api/sessions/{session_id}/restart"
                    ))
                    .json(&serde_json::json!({}))
                    .send()
                    .await
                    .expect("POST restart send");
                let status = resp.status();
                if status.is_success() {
                    break;
                }
                let body = resp.text().await.unwrap_or_default();
                // The CLI-added Session reaches the server's list on its next reload,
                // which can trail the worker accepting a prompt.
                assert!(
                    status == reqwest::StatusCode::NOT_FOUND && Instant::now() < deadline,
                    "restart POST failed: {status} {body}"
                );
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        });

    let after =
        wait_until(
            Duration::from_secs(20),
            Duration::from_millis(200),
            || match alive_worker_pids(&h, &session_id).as_slice() {
                [pid] if *pid != before => Ok(*pid),
                pids => Err(format!("worker not replaced yet: {pids:?} (was {before})")),
            },
        );
    assert_ne!(after, before);
    h.prompt_until_accepted(&session_id, "hello again", Duration::from_secs(30));
}
