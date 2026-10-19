//! Full-binary regression for strict approval restarts and runner reaping.
#![cfg(feature = "serve")]

use std::time::{Duration, Instant};

use serial_test::parallel;

use crate::harness::{pick_free_port, require_node, require_tmux, wait_for_port, TuiTestHarness};
use agent_of_empires::acp::{approvals::ApprovalDecision, protocol::ReplayResponse, Event};

#[tokio::test]
#[parallel]
async fn approval_restart_preserves_the_conversation_in_both_directions() {
    require_tmux!();
    require_node!();
    let mut h = TuiTestHarness::new_in_tmp("auto_approve");
    h.stop_daemon_on_drop();
    h.set_env("AOE_ACP_RUNNER_SOCKET_TIMEOUT_MS", "60000");
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web/tests/helpers/fakeAcpAgent.mjs");
    let script = h.home_path().join("approval.json");
    std::fs::write(&script, r#"{"turns":[{"updates":[{"sessionUpdate":"permission_request","toolCall":{"toolCallId":"restart-gate","title":"Restart gate","kind":"edit"}}],"stopReason":"end_turn"}]}"#).unwrap();
    let shim = h.install_path_command("omp").join("omp");
    std::fs::write(
        shim,
        format!(
            "#!/bin/sh\nexport FAKE_ACP_IMPERSONATE=omp\nexport FAKE_ACP_SCRIPT=\"{}\"\nexec node \"{}\" \"$@\"\n",
            script.display(), fixture.display()
        ),
    )
    .unwrap();
    let port = pick_free_port();
    let started = h.run_cli(&[
        "serve",
        "--daemon",
        "--port",
        &port.to_string(),
        "--no-auth",
    ]);
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    assert!(wait_for_port(port, Duration::from_secs(10)));
    let added = h.run_cli(&[
        "add",
        "--scratch",
        "--tool",
        "omp",
        "--structured-view",
        "--title",
        "Approval restart",
    ]);
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let id = String::from_utf8(added.stdout)
        .unwrap()
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("ID:")
                .map(|id| id.trim().to_owned())
        })
        .expect("session id");
    let http = reqwest::Client::new();
    let sessions_url = format!("http://127.0.0.1:{port}/api/sessions");
    let deadline = Instant::now() + Duration::from_secs(20);
    let conversation = loop {
        let snapshot = http
            .get(&sessions_url)
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        let conversation = snapshot["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"].as_str() == Some(id.as_str()))
            .and_then(|row| row["acp_session_id"].as_str());
        if let Some(conversation) = conversation {
            break conversation.to_owned();
        }
        assert!(
            Instant::now() < deadline,
            "worker did not establish a conversation"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let prompt = h.run_cli(&["acp", "prompt", &id, "start a gated tool"]);
    assert!(
        prompt.status.success(),
        "{}",
        String::from_utf8_lossy(&prompt.stderr)
    );
    let replay_url = format!("http://127.0.0.1:{port}/api/sessions/{id}/acp/replay");
    let deadline = Instant::now() + Duration::from_secs(10);
    let approval_seq = loop {
        let replay = http
            .get(&replay_url)
            .send()
            .await
            .unwrap()
            .json::<ReplayResponse>()
            .await
            .unwrap();
        if let Some(frame) = replay
            .frames
            .iter()
            .find(|frame| matches!(frame.event.as_ref(), Event::ApprovalRequested { .. }))
        {
            break frame.seq;
        }
        assert!(
            Instant::now() < deadline,
            "the tool never requested approval"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    for enabled in [true, false] {
        let response = http
            .post(format!(
                "http://127.0.0.1:{port}/api/sessions/{id}/auto-approve"
            ))
            .json(&serde_json::json!({ "enabled": enabled }))
            .send()
            .await
            .unwrap();
        let status = response.status();
        let acknowledgement = response.json::<serde_json::Value>().await.unwrap();
        assert!(status.is_success(), "{acknowledgement}");
        assert_eq!(acknowledgement["restarted"], true);
        let snapshot = http
            .get(&sessions_url)
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        let row = snapshot["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["id"].as_str() == Some(id.as_str()))
            .unwrap();
        assert_eq!(row["yolo_mode"], enabled);
        assert_eq!(row["acp_session_id"].as_str(), Some(conversation.as_str()));
        assert_eq!(row["acp_worker_state"], "running");
        let replay = http
            .get(format!("{replay_url}?since={approval_seq}"))
            .send()
            .await
            .unwrap()
            .json::<ReplayResponse>()
            .await
            .unwrap();
        assert!(!replay
            .frames
            .iter()
            .any(|frame| matches!(frame.event.as_ref(), Event::SessionContextReset { .. })));
        if enabled {
            assert!(
                replay.frames.iter().any(|frame| matches!(frame.event.as_ref(), Event::Stopped { .. })),
                "interrupting a pending approval must end the old turn for connected and replaying clients"
            );
            assert!(replay.frames.iter().any(|frame| matches!(
                frame.event.as_ref(),
                Event::ApprovalResolved {
                    decision: ApprovalDecision::Cancelled,
                    ..
                }
            )));
        }
    }
}
