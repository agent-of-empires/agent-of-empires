//! Retained native outcomes keep their request identity and native stop reason.

use std::future::Future;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::acp::protocol::{AcpOriginalRequestResult, AcpPromptObservation};
use crate::acp::state::Event;

use super::managed::{with_managed, ManagedClient};

async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(15), future)
        .await
        .expect("managed cached-outcome operation completes before deadline")
}

fn assert_healthy(event: &Event) {
    assert!(
        !matches!(
            event,
            Event::AcpConnectionUnavailable { .. }
                | Event::AgentStartupError { .. }
                | Event::PromptRuntimeError { .. }
                | Event::SessionContextReset { .. }
        ),
        "retained native conversation unexpectedly failed: {event:?}"
    );
}

async fn stopped(client: &mut ManagedClient, expected: &str) {
    bounded(async {
        loop {
            let event = client
                .next_event()
                .await
                .expect("managed observer remains open");
            if let Event::Stopped { reason } = &event {
                assert_eq!(reason, expected);
                break;
            }
            assert_healthy(&event);
        }
    })
    .await;
}

fn assert_native_stop(client: &ManagedClient, after: u64, request: &Value, expected: &str) {
    let state = client.app_state();
    let stored = state
        .acp_event_store
        .replay_from(&client.id, after)
        .unwrap();
    let stops: Vec<_> = stored
        .iter()
        .filter(|entry| matches!(entry.event.as_ref(), Event::Stopped { .. }))
        .collect();
    assert_eq!(
        stops.len(),
        1,
        "one terminal for the actual native response"
    );
    assert!(matches!(stops[0].event.as_ref(), Event::Stopped { reason } if reason == expected));
    let metadata = &stops[0].metadata;
    assert_eq!(metadata.native_prompt_id, request["id"].as_i64());
    assert!(metadata.native_origin.is_some());
    assert_eq!(
        metadata.operation.unwrap().outcome,
        Some(AcpOriginalRequestResult::Succeeded)
    );
    assert!(matches!(
        metadata.prompt_observation,
        Some(AcpPromptObservation::Written { .. })
    ));
}

#[tokio::test]
#[serial_test::serial]
async fn cached_native_stop_reason_survives_retained_attachment_and_next_input() {
    with_managed(|scope| Box::pin(async move {
        for in_flight in [false, true] {
            for reason in ["max_tokens", "cancelled"] {
                let name = std::thread::current().name().unwrap().to_owned();
                let inspection = scope.inspect_next_runner(&name);
                let directory = scope.tempdir();
                let session_id = format!("midturn-{}", uuid::Uuid::new_v4());
                let (seen, mut prompts) = mpsc::unbounded_channel();
                let (config, native) = scope.native_peer(
                    directory.path().into(), "codex", "codex", move |request| {
                        let id = request.get("id")?;
                        match request["method"].as_str().unwrap() {
                            "initialize" => Some(json!({
                                "jsonrpc": "2.0", "id": id,
                                "result": {"protocolVersion": 1, "agentCapabilities": {}}
                            })),
                            "session/new" => Some(json!({
                                "jsonrpc": "2.0", "id": id,
                                "result": {"sessionId": session_id}
                            })),
                            "session/prompt" => {
                                assert_eq!(request["params"]["sessionId"], session_id);
                                seen.send(request.clone()).unwrap();
                                None
                            }
                            method => panic!("unexpected native operation: {method}"),
                        }
                    },
                );
                let original = bounded(scope.spawn(config, "midturn-cached-native")).await.unwrap();
                bounded(original.ready()).await;
                let owner = original.connected_runner().expect("actual original runner owner");
                bounded(original.send_prompt("original pending native request", &[])).await.unwrap();
                let first = bounded(prompts.recv()).await.expect("actual first prompt reached native stdin");
                let store = original.app_state().acp_event_store.clone();
                let before = store.highest_seq(&original.id);
                let mut client = bounded(scope.adopt_with(original, in_flight, || async {
                    // Joining the old SDK does not mean the runner has consumed its socket EOF.
                    bounded(async {
                        loop {
                            let snapshot = inspection.query("snapshot").await;
                            if snapshot["attached"] == false {
                                assert_eq!(snapshot["queued_frames"], 0);
                                assert_eq!(snapshot["retained_bytes"], 0);
                                break;
                            }
                            tokio::task::yield_now().await;
                        }
                    }).await;
                    bounded(native.send(json!({
                        "jsonrpc": "2.0", "id": first["id"], "result": {"stopReason": reason}
                    }))).await;
                    bounded(async {
                        loop {
                            let snapshot = inspection.query("snapshot").await;
                            assert_eq!(snapshot["attached"], false);
                            let completions = snapshot["queued_prompt_completions"].as_array().unwrap();
                            if completions.iter().any(|completion| completion[0] == first["id"]) {
                                assert_eq!(completions.len(), 1);
                                break;
                            }
                            tokio::task::yield_now().await;
                        }
                    }).await;
                })).await;
                bounded(client.ready()).await;
                assert_eq!(client.connected_runner(), Some(owner));
                stopped(&mut client, reason).await;
                assert_native_stop(&client, before, &first, reason);

                let before_next = store.highest_seq(&client.id);
                bounded(client.send_prompt("distinct next explicit native request", &[])).await.unwrap();
                let next = bounded(prompts.recv()).await.expect("actual next prompt reached native stdin");
                assert_ne!(next["id"], first["id"]);
                assert_eq!(next["params"]["sessionId"], first["params"]["sessionId"]);
                // The peer has received the new request and has not answered it.
                let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
                loop {
                    match tokio::time::timeout_at(deadline, client.next_event()).await {
                        Err(_) => break,
                        Ok(None) => panic!("managed observer closed before the native response"),
                        Ok(Some(event)) => {
                            assert!(!matches!(event, Event::Stopped { .. }),
                                "cached outcome cannot settle a different pending request: {event:?}");
                            assert_healthy(&event);
                        }
                    }
                }
                bounded(native.send(json!({
                    "jsonrpc": "2.0", "id": next["id"], "result": {"stopReason": "end_turn"}
                }))).await;
                stopped(&mut client, "prompt_complete").await;
                assert_native_stop(&client, before_next, &next, "prompt_complete");
                assert_eq!(client.connected_runner(), Some(owner));
                let methods = native.requests();
                for (method, expected) in [("initialize", 1), ("session/new", 1), ("session/prompt", 2)] {
                    assert_eq!(methods.iter().filter(|seen| seen.as_str() == method).count(), expected, "{method}");
                }
                assert!(!methods.iter().any(|method| matches!(method.as_str(),
                    "session/load" | "session/fork" | "session/cancel")));
                bounded(client.shutdown()).await.unwrap();
            }
        }
    })).await;
}
