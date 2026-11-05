use super::managed::{with_managed, IssuedOriginal, ManagedClient};
use crate::acp::control_protocol::{ControlBody, HandshakeFailureOrigin};
use crate::acp::establishment::{
    AcpEstablishment, AuthorityKey, AuthorityPhase, EstablishmentBlockReason, EstablishmentOrigin,
    EstablishmentTarget, NativeEstablishmentOutcome,
};
use crate::acp::state::Event;
use crate::process::runner::test_support::managed_runner::RunnerInspection;
use serde_json::{json, Value};
use std::future::Future;
use std::time::Duration;
use tokio::sync::oneshot;

async fn control_deadline<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(15), future)
        .await
        .expect("genuine managed control boundary before deadline")
}

fn control_test_name() -> String {
    std::thread::current()
        .name()
        .expect("managed libtest name")
        .to_owned()
}

fn durable_control_row(id: &str) -> crate::session::Instance {
    crate::session::Storage::open_unwatched("test")
        .unwrap()
        .load_instances_complete()
        .unwrap()
        .into_iter()
        .find(|row| row.id == id)
        .expect("factory-created durable row")
}

fn committed_control_frames(snapshot: &Value) -> Vec<ControlBody> {
    snapshot["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["stage"] == "committed")
        .filter_map(|event| event.get("body"))
        .map(|body| serde_json::from_value(body.clone()).unwrap())
        .collect()
}

async fn committed_control_until(
    inspection: &RunnerInspection,
    predicate: impl Fn(&[ControlBody]) -> bool,
) -> (Value, Vec<ControlBody>) {
    control_deadline(async {
        loop {
            let snapshot = inspection.query("producer-snapshot").await;
            let frames = committed_control_frames(&snapshot);
            if predicate(&frames) {
                return (snapshot, frames);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
}

async fn control_stopped(client: &mut ManagedClient, expected: &str) {
    control_deadline(async {
        loop {
            match client
                .next_event()
                .await
                .expect("actual managed publication")
            {
                Event::Stopped { reason } => {
                    assert_eq!(reason, expected);
                    break;
                }
                Event::AgentStartupError { message, .. } => panic!("{message}"),
                Event::AcpConnectionUnavailable { .. } => {
                    panic!("original native owner remains live")
                }
                _ => {}
            }
        }
    })
    .await;
}

#[tokio::test]
#[serial_test::serial]
async fn agent_request_during_session_new_does_not_deadlock_the_runner() {
    with_managed(|scope| {
        Box::pin(async move {
            let directory = scope.tempdir();
            let setup = directory.path().join("setup");
            std::fs::write(&setup, "setup data").unwrap();
            let inspection = scope.inspect_next_runner(&control_test_name());
            let (new_seen, new_received) = oneshot::channel();
            let mut new_seen = Some(new_seen);
            let (config, mut native) =
                scope.native_peer(directory.path().into(), "codex", "codex", move |request| {
                    let id = request.get("id")?;
                    match request["method"].as_str().unwrap() {
                        "initialize" => Some(json!({"jsonrpc":"2.0","id":id,"result":{
                            "protocolVersion":1,"agentCapabilities":{}
                        }})),
                        "session/new" => {
                            new_seen.take().unwrap().send(id.clone()).unwrap();
                            None
                        }
                        other => panic!("unexpected native operation during original New: {other}"),
                    }
                });
            let client = scope.spawn(config, "new-callback").await.unwrap();
            let new_id = control_deadline(new_received).await.unwrap();
            assert_eq!(
                inspection.query("observe-producer").await,
                json!({"observing":true})
            );
            let pending = durable_control_row(&client.id);
            let attempt = match pending.acp_establishment {
                Some(AcpEstablishment::Pending { attempt }) => attempt,
                other => panic!("actual Native New must remain Pending: {other:?}"),
            };
            assert_eq!(attempt.target, EstablishmentTarget::New);
            native
                .send(
                    json!({"jsonrpc":"2.0","id":"setup-read","method":"fs/read_text_file",
            "params":{"sessionId":"callback-native","path":setup}}),
                )
                .await;
            let reply = control_deadline(native.reply()).await;
            assert_eq!(reply["id"], "setup-read");
            assert_eq!(reply["result"]["content"], "setup data", "{reply}");
            assert!(
                reply.get("error").is_none(),
                "original pending New must authorize its own I/O: {reply}"
            );
            assert!(matches!(durable_control_row(&client.id).acp_establishment,
            Some(AcpEstablishment::Pending { attempt: current }) if current == attempt));
            native
                .send(json!({"jsonrpc":"2.0","id":new_id,"result":{"sessionId":"callback-native"}}))
                .await;
            control_deadline(client.ready()).await;
            let (_, frames) = committed_control_until(&inspection, |frames| {
                frames
                    .iter()
                    .any(|frame| matches!(frame, ControlBody::SessionReady { .. }))
            })
            .await;
            let ready = frames
                .iter()
                .find(|frame| matches!(frame, ControlBody::SessionReady { .. }))
                .unwrap();
            let ControlBody::SessionReady {
                acp_session_id,
                receipt,
                original_delivery,
                ..
            } = ready
            else {
                unreachable!()
            };
            assert_eq!(acp_session_id, "callback-native");
            assert_eq!(receipt.attempt.as_ref(), attempt.as_ref());
            assert_eq!(Some(receipt.owner), client.actual().connected_runner());
            assert!(receipt.owner.birth_fingerprint.is_some());
            assert!(*original_delivery);
            assert!(receipt.replay.closed && !receipt.replay.lost);
            let record = crate::process::worker_registry::load_strict(&client.id)
                .unwrap()
                .unwrap();
            assert_eq!(
                record.stored_acp_session_id.as_deref(),
                Some("callback-native")
            );
            assert_eq!(
                durable_control_row(&client.id).acp_session_id.as_deref(),
                Some("callback-native")
            );
            assert_eq!(native.requests(), ["initialize", "session/new"]);
            client.shutdown().await.unwrap();
        })
    })
    .await;
}

#[tokio::test]
#[serial_test::serial]
async fn runner_load_uses_requested_id_and_caches_response() {
    with_managed(|scope| Box::pin(async move {
        let directory = scope.tempdir();
        let (config, _) = scope.native_peer(directory.path().into(), "codex", "codex", |request| {
            let id = request.get("id")?;
            Some(match request["method"].as_str().unwrap() {
                "initialize" => json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":1,"agentCapabilities":{}}}),
                "session/new" => json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":"load-native"}}),
                other => panic!("unexpected priming operation: {other}"),
            })
        });
        let original = scope.spawn(config, "load-cache-prime").await.unwrap();
        original.ready().await;
        let inspection = scope.inspect_next_runner(&control_test_name());
        let (load_seen, load_received) = oneshot::channel();
        let (prompt_seen, prompt_received) = oneshot::channel();
        let (cancel_seen, cancel_received) = oneshot::channel();
        let (mut load_seen, mut prompt_seen, mut cancel_seen) =
            (Some(load_seen), Some(prompt_seen), Some(cancel_seen));
        let (config, native) = scope.native_peer(directory.path().into(), "codex", "codex", move |request| {
            match request["method"].as_str().unwrap() {
                "initialize" => Some(json!({"jsonrpc":"2.0","id":request["id"],"result":{
                    "protocolVersion":1,"agentCapabilities":{"loadSession":true}
                }})),
                "session/load" => {
                    assert_eq!(request["params"]["sessionId"], "load-native");
                    load_seen.take().unwrap().send(request["id"].clone()).unwrap(); None
                }
                "session/prompt" => {
                    assert_eq!(request["params"]["sessionId"], "load-native");
                    prompt_seen.take().unwrap().send(request["id"].clone()).unwrap(); None
                }
                "session/cancel" => {
                    assert_eq!(request["params"]["sessionId"], "load-native");
                    cancel_seen.take().unwrap().send(()).unwrap(); None
                }
                other => panic!("retained Load or adoption must not issue fallback/retry: {other}"),
            }
        });
        let mut client = scope.retained_after(original, config, "load-cache").await;
        let load_id = control_deadline(load_received).await.unwrap();
        inspection.query("observe-producer").await;
        native.send(json!({"jsonrpc":"2.0","method":"session/update","params":{
            "sessionId":"load-native","update":{"sessionUpdate":"agent_message_chunk",
                "content":{"type":"text","text":"historical Native answer"}}
        }})).await;
        native.send(json!({"jsonrpc":"2.0","id":load_id,"result":{}})).await;
        client.ready().await;
        let (_, first_frames) = committed_control_until(&inspection, |frames| frames.iter()
            .any(|frame| matches!(frame, ControlBody::SessionReady { original_delivery:true, .. }))).await;
        let (first_result, first_receipt) = first_frames.iter().find_map(|frame| match frame {
            ControlBody::SessionReady { acp_session_id, result, receipt, original_delivery:true, .. } => {
                assert_eq!(acp_session_id, "load-native");
                assert_eq!(result.as_ref(), &json!({}));
                assert_eq!(receipt.replay.notifications, 1);
                assert!(matches!(&receipt.attempt.target, EstablishmentTarget::Load { session_id, import:false }
                    if session_id == "load-native"));
                Some((result.clone(), receipt.clone()))
            }
            _ => None,
        }).unwrap();
        assert_eq!(first_receipt.owner, client.lease.spawned_runner().unwrap());
        client.send_prompt("cancel the loaded conversation", &[]).await.unwrap();
        let prompt_id = control_deadline(prompt_received).await.unwrap();
        client.cancel_prompt().await.unwrap();
        control_deadline(cancel_received).await.unwrap();
        native.send(json!({"jsonrpc":"2.0","id":prompt_id,"result":{"stopReason":"cancelled"}})).await;
        control_stopped(&mut client, "cancelled").await;
        let client = scope.adopt(client, false).await;
        client.ready().await;
        let (_, cached_frames) = committed_control_until(&inspection, |frames| frames.iter()
            .any(|frame| matches!(frame, ControlBody::SessionReady { original_delivery:false, .. }))).await;
        let cached = cached_frames.iter().rev().find_map(|frame| match frame {
            ControlBody::SessionReady { acp_session_id, result, receipt, original_delivery:false, .. } =>
                Some((acp_session_id, result, receipt)),
            _ => None,
        }).unwrap();
        assert_eq!(cached.0, "load-native");
        assert_eq!(cached.1.as_ref(), first_result.as_ref());
        assert_eq!(cached.2.as_ref(), first_receipt.as_ref());
        assert_eq!(native.requests().iter().filter(|method| *method == "initialize").count(), 1);
        assert_eq!(native.requests().iter().filter(|method| *method == "session/load").count(), 1);
        assert_eq!(native.requests().iter().filter(|method| *method == "session/new").count(), 0);
        assert_eq!(native.requests().iter().filter(|method| *method == "session/cancel").count(), 1);
        client.shutdown().await.unwrap();
    })).await;
}

#[tokio::test]
#[serial_test::serial]
async fn runner_forwards_session_error_data_in_handshake_failed() {
    with_managed(|scope| Box::pin(async move {
        let directory = scope.tempdir();
        let inspection = scope.inspect_next_runner(&control_test_name());
        let (new_seen, new_received) = oneshot::channel();
        let mut new_seen = Some(new_seen);
        let (config, native) = scope.native_peer(directory.path().into(), "codex", "codex", move |request| {
            let id = request.get("id")?;
            match request["method"].as_str().unwrap() {
                "initialize" => Some(json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":1,"agentCapabilities":{}}})),
                "session/new" => { new_seen.take().unwrap().send(id.clone()).unwrap(); None }
                other => panic!("no fallback after original native rejection: {other}"),
            }
        });
        let mut client = scope.spawn(config, "native-new-error").await.unwrap();
        let new_id = control_deadline(new_received).await.unwrap();
        inspection.query("observe-producer").await;
        let pending = durable_control_row(&client.id);
        let expected = pending.acp_establishment.unwrap().attempt().unwrap().clone();
        let error = json!({"code":-32603,"message":"Internal error","data":{
            "details":"native binary failed to launch","vendor":{"nested":[1,{"keep":true}]}
        }});
        native.send(json!({"jsonrpc":"2.0","id":new_id,"error":error.clone()})).await;
        let (_, frames) = committed_control_until(&inspection, |frames| frames.iter()
            .any(|frame| matches!(frame, ControlBody::EstablishmentFailed { .. }))).await;
        let receipt = frames.iter().find_map(|frame| match frame {
            ControlBody::EstablishmentFailed { receipt, error: actual, origin, original_delivery, delivery, .. } => {
                assert_eq!(actual.as_ref(), &error);
                assert_eq!(*origin, HandshakeFailureOrigin::NativeResponse);
                assert!(*original_delivery);
                assert_eq!(receipt.attempt.as_ref(), &expected);
                assert_eq!(receipt.attempt.target, EstablishmentTarget::New);
                assert_eq!(Some(receipt.owner), client.actual().connected_runner());
                assert!(receipt.owner.birth_fingerprint.is_some());
                assert_eq!(receipt.outcome, NativeEstablishmentOutcome::Failed { reason: EstablishmentBlockReason::Rejected });
                assert!(receipt.replay.closed && !receipt.replay.lost);
                assert!(delivery.matches(receipt.replay));
                Some(receipt.clone())
            }
            _ => None,
        }).unwrap();
        let mut expected_blocked = receipt.attempt.as_ref().clone();
        expected_blocked.destination.authority = Some(AuthorityKey {
            id: expected_blocked.id, phase: AuthorityPhase::Blocked,
        });
        control_deadline(async {
            loop {
                match durable_control_row(&client.id).acp_establishment {
                    Some(AcpEstablishment::Blocked { attempt, reason }) => {
                        assert_eq!(attempt.as_ref(), &expected_blocked);
                        assert_eq!(reason, EstablishmentBlockReason::Rejected);
                        break;
                    }
                    Some(AcpEstablishment::Applied { .. }) => panic!("native rejection cannot become Applied"),
                    _ => tokio::time::sleep(Duration::from_millis(5)).await,
                }
            }
            loop {
                match client.next_event().await.expect("actual startup diagnostic") {
                    Event::AgentStartupError { message, .. } => {
                        assert!(message.contains("native binary failed to launch"), "{message}"); break;
                    }
                    Event::AcpSessionAssigned { .. } => panic!("rejected native New cannot assign a session"),
                    _ => {}
                }
            }
        }).await;
        assert!(durable_control_row(&client.id).acp_session_id.is_none());
        assert_eq!(native.requests(), ["initialize", "session/new"]);
    })).await;
}

#[tokio::test]
#[serial_test::serial]
async fn resumed_client_uses_reset_committed_after_reattach() {
    with_managed(|scope| Box::pin(async move {
        for rebase in [false, true] {
        let directory = scope.tempdir();
        let original_file = directory.path().join("pending-original");
        std::fs::write(&original_file, "original pending IO").unwrap();
        let inspection = scope.inspect_next_runner(&control_test_name());
        let (reset_seen, reset_received) = oneshot::channel();
        let (prompt_seen, prompt_received) = oneshot::channel();
        let (mut reset_seen, mut prompt_seen) = (Some(reset_seen), Some(prompt_seen));
        let mut news = 0;
        let (config, mut native) = scope.native_peer(directory.path().into(), "codex", "codex", move |request| {
            let id = request.get("id")?;
            match request["method"].as_str().unwrap() {
                "initialize" => Some(json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":1,"agentCapabilities":{}}})),
                "session/new" => {
                    news += 1;
                    match news {
                        1 => Some(json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":"reset-old"}})),
                        2 => { reset_seen.take().unwrap().send(id.clone()).unwrap(); None }
                        _ => panic!("reattach must not issue another Native New"),
                    }
                }
                "session/prompt" => {
                    assert_eq!(news, 2);
                    assert_eq!(request["params"]["sessionId"], "reset-new");
                    prompt_seen.take().unwrap().send(id.clone()).unwrap(); None
                }
                other => panic!("pending original reset must not fallback or Load: {other}"),
            }
        });
        let original = scope.spawn(config, "reset-reattach").await.unwrap();
        original.ready().await;
        let old_receipt = original.actual().establishment_projection().unwrap().receipt_snapshot().unwrap();
        inspection.query("observe-producer").await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let proposal = original.prepare_reset(deadline).await;
        let issuing_client = original.actual();
        let request = scope.spawn_helper(async move {
            let _ = issuing_client.reset_session("clear", proposal, deadline, deadline).await;
        });
        let reset_id = control_deadline(reset_received).await.unwrap();
        let issued: IssuedOriginal = serde_json::from_value(
            inspection.query("pending-original").await,
        ).unwrap();
        assert_eq!(Some(issued.request_id), reset_id.as_i64());
        let expected = issued.receipt.attempt.clone();
        assert!(matches!(durable_control_row(&original.id).acp_establishment,
            Some(AcpEstablishment::Pending { attempt }) if attempt.as_ref() == expected.as_ref()));
        assert_eq!(expected.target, EstablishmentTarget::New);
        assert_eq!(expected.origin, EstablishmentOrigin::Fresh);
        assert_ne!(expected.id, old_receipt.attempt.id);
        request.abort();
        let _ = request.await;
        if rebase {
            let state = scope.state.clone();
            let id = original.id.clone();
            tokio::task::spawn_blocking(move || {
                use crate::session::SessionStore as _;
                let _title = crate::session::acquire_session_title_lock(&id).unwrap();
                let native = crate::server::session_store::NativeSessionStore::open(state, "test", None).unwrap();
                let _lifecycle = native.storage().acquire_instance_lifecycle_lock(&id).unwrap();
                let mut row = native.load().unwrap().into_iter().find(|row| row.id == id).unwrap();
                let before = row.lifecycle_generation;
                assert!(row.advance_auxiliary_generation_in(&native).unwrap() > before);
            }).await.unwrap();
        }
        let mut client = scope.adopt_pending(original, &inspection, issued, false).await;
        let (pending, _) = committed_control_until(&inspection, |frames| frames.iter()
            .any(|frame| matches!(frame, ControlBody::OriginalEstablishment { invocation }
                if invocation.attempt.as_ref() == expected.as_ref()))).await;
        let attachment = pending["attachment"].as_u64().unwrap();
        assert!(pending["events"].as_array().unwrap().iter().any(|event|
            event["stage"] == "committed" && event["attachment"] == attachment
                && event["body"]["kind"] == "original_establishment"));
        assert_eq!(native.requests().iter().filter(|method| *method == "session/new").count(), 2);
        assert_eq!(native.requests().iter().filter(|method| *method == "session/load").count(), 0);
        assert!(client.actual().establishment_projection().unwrap().receipt_snapshot().is_none());
        native.send(json!({"jsonrpc":"2.0","id":"pending-original-read",
            "method":"fs/read_text_file","params":{"sessionId":"reset-new","path":original_file}})).await;
        let reply = control_deadline(native.reply()).await;
        assert_eq!(reply["id"], "pending-original-read");
        assert_eq!(reply["result"]["content"], "original pending IO", "rebase={rebase}: {reply}");
        assert!(matches!(durable_control_row(&client.id).acp_establishment,
            Some(AcpEstablishment::Pending { attempt })
                if attempt.same_source(&expected) && attempt.destination == expected.destination));
        native.send(json!({"jsonrpc":"2.0","method":"session/update","params":{
            "sessionId":"reset-new","update":{"sessionUpdate":"agent_message_chunk",
                "content":{"type":"text","text":"pending-original-prefix"}}
        }})).await;
        native.send(json!({"jsonrpc":"2.0","id":reset_id,"result":{"sessionId":"reset-new"}})).await;
        client.ready().await;
        let receipt = client.actual().establishment_projection().unwrap().receipt_snapshot().unwrap();
        assert_eq!(receipt.attempt.as_ref(), expected.as_ref());
        assert_eq!(receipt.owner, old_receipt.owner);
        assert_eq!(receipt.session_id(), Some("reset-new"));
        assert!(receipt.replay.closed && !receipt.replay.lost);
        assert_eq!(receipt.replay.notifications, 1);
        client.send_prompt("after reset", &[]).await.unwrap();
        let prompt_id = control_deadline(prompt_received).await.unwrap();
        native.send(json!({"jsonrpc":"2.0","method":"session/update","params":{
            "sessionId":"reset-new","update":{"sessionUpdate":"agent_message_chunk",
                "content":{"type":"text","text":"reset-new:new-count=2"}}
        }})).await;
        native.send(json!({"jsonrpc":"2.0","id":prompt_id,"result":{"stopReason":"end_turn"}})).await;
        control_deadline(async {
            let (mut assigned, mut addressed, mut prefix) = (false, false, false);
            loop {
                match client.next_event().await.expect("replacement managed consumer event") {
                    Event::AcpSessionAssigned { acp_session_id, .. } => {
                        assert_eq!(acp_session_id, "reset-new"); assigned = true;
                    }
                    Event::AgentMessageChunk { text, .. } if text == "pending-original-prefix" => {
                        assert!(!prefix); prefix = true;
                    }
                    Event::AgentMessageChunk { text, .. } => {
                        assert_eq!(text, "reset-new:new-count=2"); addressed = true;
                    }
                    Event::Stopped { reason } => {
                        assert_eq!(reason, "prompt_complete"); assert!(assigned && addressed && prefix); break;
                    }
                    Event::AgentStartupError { message, .. } => panic!("{message}"),
                    Event::AcpConnectionUnavailable { .. } => panic!("original reset owner stays live"),
                    _ => {}
                }
            }
        }).await;
        assert_eq!(native.requests().iter().filter(|method| *method == "initialize").count(), 1);
        assert_eq!(native.requests().iter().filter(|method| *method == "session/new").count(), 2);
        assert_eq!(native.requests().iter().filter(|method| *method == "session/load").count(), 0);
        assert_eq!(durable_control_row(&client.id).acp_session_id.as_deref(), Some("reset-new"));
        client.shutdown().await.unwrap();
        }
    })).await;
}

#[tokio::test]
#[serial_test::serial]
async fn runner_owns_handshake_and_caches_across_attaches() {
    with_managed(|scope| Box::pin(async move {
        let directory = scope.tempdir();
        let inspection = scope.inspect_next_runner(&control_test_name());
        let (completion_gate, _release, completion_received) = scope.gate();
        scope.configure_sdk(crate::acp::acp_client::SdkTestOptions {
            prompt_completed: Some(completion_gate), ..Default::default()
        });
        let (prompt_seen, prompt_received) = oneshot::channel();
        let mut prompt_seen = Some(prompt_seen);
        let native_new = json!({"sessionId":"cache-native","_meta":{"nativeMarker":"original-New-response"}});
        let native_new_reply = native_new.clone();
        let (config, native) = scope.native_peer(directory.path().into(), "codex", "codex", move |request| {
            let id = request.get("id")?;
            match request["method"].as_str().unwrap() {
                "initialize" => Some(json!({"jsonrpc":"2.0","id":id,"result":{
                    "protocolVersion":1,"agentCapabilities":{}
                }})),
                "session/new" => Some(json!({"jsonrpc":"2.0","id":id,"result":native_new_reply.clone()})),
                "session/prompt" => {
                    assert_eq!(request["params"]["sessionId"], "cache-native");
                    prompt_seen.take().unwrap().send(id.clone()).unwrap(); None
                }
                other => panic!("cached adoption must not contact Native again: {other}"),
            }
        });
        let original = scope.spawn(config, "successful-cached-completion").await.unwrap();
        original.ready().await;
        let before_receipt = original.actual().establishment_projection().unwrap().receipt_snapshot().unwrap();
        let store = original.app_state().acp_event_store.clone();
        let before = store.highest_seq(&original.id);
        inspection.query("observe-producer").await;
        original.send_prompt("complete before replacement attaches", &[]).await.unwrap();
        let prompt_id = control_deadline(prompt_received).await.unwrap();
        native.send(json!({"jsonrpc":"2.0","id":prompt_id,"result":{"stopReason":"end_turn"}})).await;
        control_deadline(completion_received).await.unwrap();
        let (_, original_frames) = committed_control_until(&inspection, |frames| frames.iter().any(|frame|
            matches!(frame, ControlBody::PromptCompleted { prompt_req_id, .. }
                if Some(*prompt_req_id) == prompt_id.as_i64()))).await;
        let original_completion = original_frames.iter().find_map(|frame| match frame {
            ControlBody::PromptCompleted { prompt_req_id, outcome, .. }
                if Some(*prompt_req_id) == prompt_id.as_i64() => Some(outcome),
            _ => None,
        }).unwrap();
        assert!(matches!(original_completion,
            crate::acp::control_protocol::PromptOutcome::Completed { stop_reason:Some(reason) }
                if reason == "end_turn"));
        let mut client = scope.adopt(original, true).await;
        client.ready().await;
        let (_, replayed_frames) = committed_control_until(&inspection, |frames| frames.iter()
            .any(|frame| matches!(frame, ControlBody::SessionReady { original_delivery:false, .. }))).await;
        let cached = replayed_frames.iter().rev().find_map(|frame| match frame {
            ControlBody::SessionReady { acp_session_id, result, receipt, original_delivery:false, .. } =>
                Some((acp_session_id, result, receipt)),
            _ => None,
        }).unwrap();
        assert_eq!(cached.0, "cache-native");
        assert_eq!(cached.1.as_ref(), &native_new);
        assert_eq!(cached.2.as_ref(), before_receipt.as_ref());
        control_deadline(async {
            let (mut assigned, mut stopped) = (false, false);
            while !assigned || !stopped {
                match client.next_event().await.expect("actual successful cached replay publication") {
                    Event::AcpSessionAssigned { acp_session_id, .. } => {
                        assert_eq!(acp_session_id, "cache-native"); assigned = true;
                    }
                    Event::Stopped { reason } => {
                        assert_eq!(reason, "prompt_complete");
                        assert!(!stopped, "one cached Native result settles one original prompt");
                        stopped = true;
                    }
                    Event::SessionContextReset { .. } => panic!("cached owner retains its conversation"),
                    Event::AgentStartupError { message, .. } => panic!("{message}"),
                    Event::AcpConnectionUnavailable { .. } => panic!("cached original owner remains live"),
                    _ => {}
                }
            }
        }).await;
        let stored = store.replay_from(&client.id, before).unwrap();
        let terminals: Vec<_> = stored.iter().filter(|stored|
            matches!(stored.event.as_ref(), Event::Stopped { reason } if reason == "prompt_complete")
        ).collect();
        assert_eq!(terminals.len(), 1);
        assert_eq!(terminals[0].metadata.native_prompt_id, prompt_id.as_i64());
        assert!(terminals[0].metadata.operation.is_some_and(|operation| operation.outcome.is_some()));
        assert_eq!(native.requests().iter().filter(|method| *method == "initialize").count(), 1);
        assert_eq!(native.requests().iter().filter(|method| *method == "session/new").count(), 1);
        assert_eq!(native.requests().iter().filter(|method| *method == "session/prompt").count(), 1);
        assert_eq!(native.requests().iter().filter(|method| *method == "session/load").count(), 0);
        client.shutdown().await.unwrap();
    })).await;
}
