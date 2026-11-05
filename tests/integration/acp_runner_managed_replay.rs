use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::future::Future;
use std::path::Path;
use std::time::Duration;

use super::managed::{with_managed, ManagedClient, ManagedScope, NativePeer};
use crate::acp::acp_client::SdkTestOptions;
use crate::acp::control_protocol::{self, ControlBody};
use crate::acp::establishment::NativeRequestScope;
use crate::acp::state::Event;
use crate::process::runner::test_support::managed_runner::{child_if_requested, RunnerInspection};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(30), future)
        .await
        .expect("real managed transport completes before deadline")
}

fn test_name(item: &str) -> String {
    format!("{}::{item}", module_path!().split_once("::").unwrap().1)
}

async fn original(
    scope: &mut ManagedScope,
    inspection_name: &str,
    echo_observations: bool,
) -> (ManagedClient, NativePeer, RunnerInspection) {
    let directory = scope.tempdir();
    let inspection = scope.inspect_next_runner(inspection_name);
    let (mut config, native) =
        scope.native_peer(directory.path().into(), "codex", "codex", move |request| {
            let id = request.get("id")?;
            match request["method"].as_str().unwrap() {
                "initialize" => Some(serde_json::json!({
                    "jsonrpc":"2.0","id":id,
                    "result":{"protocolVersion":1,"agentCapabilities":{}}
                })),
                "session/new" => Some(serde_json::json!({
                    "jsonrpc":"2.0","id":id,"result":{"sessionId":"managed-control-native"}
                })),
                "session/list" if echo_observations => Some(serde_json::json!({
                    "jsonrpc":"2.0","id":id,"method":"x/transport-observation",
                    "params":request["params"]
                })),
                other => panic!("unexpected native request: {other}"),
            }
        });
    config.spec.env_allowlist = Some(vec!["AOE_E2E_PARTIAL_FRAME_FILE".into()]);
    let client = scope.spawn(config, "managed-control").await.unwrap();
    client.ready().await;
    let receipt = client
        .actual()
        .establishment_projection()
        .unwrap()
        .receipt_snapshot()
        .unwrap();
    assert_eq!(client.actual().connected_runner(), Some(receipt.owner));
    assert!(receipt.owner.birth_fingerprint.is_some());
    assert!(inspection
        .native_spawn_proof(&client.lease)
        .unwrap()
        .is_some());
    (client, native, inspection)
}

async fn attached(path: &Path, id: &str) -> UnixStream {
    let mut stream = UnixStream::connect(path).await.unwrap();
    assert!(
        matches!(control_protocol::read_frame(&mut stream).await.unwrap(),
        Some(ControlBody::Hello { session_id, control_protocol_version, .. })
            if session_id == id && control_protocol_version == control_protocol::CONTROL_PROTOCOL_VERSION)
    );
    control_protocol::write_frame(
        &mut stream,
        &ControlBody::Attach {
            control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
        },
    )
    .await
    .unwrap();
    stream
}

async fn frame(stream: &mut UnixStream) -> ControlBody {
    bounded(control_protocol::read_frame(stream))
        .await
        .unwrap()
        .expect("live real runner frame")
}

async fn cached_resume(stream: &mut UnixStream) -> ControlBody {
    control_protocol::write_frame(
        stream,
        &ControlBody::Initialize {
            request: serde_json::json!({"protocolVersion":1}),
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        frame(stream).await,
        ControlBody::Initialized { .. }
    ));
    control_protocol::write_frame(stream, &ControlBody::ResumeSession)
        .await
        .unwrap();
    let ready = frame(stream).await;
    assert!(matches!(ready, ControlBody::SessionReady { .. }));
    ready
}

fn assert_empty_replay_fence(body: ControlBody, ready: &ControlBody) {
    let ControlBody::SessionReady {
        receipt, delivery, ..
    } = ready
    else {
        panic!("replay fence requires the actual SessionReady frame");
    };
    assert_eq!(receipt.replay.notifications, 0);
    assert!(receipt.replay.closed && !receipt.replay.lost);
    assert_eq!(delivery.after, 0);
    assert_eq!(delivery.through, 0);
    assert!(!delivery.lost);
    let ControlBody::Notify {
        method,
        params,
        replay,
        evidence,
    } = body
    else {
        panic!("expected this attachment's replay fence, got {body:?}");
    };
    assert_eq!(method, "_aoe/session_replayed");
    assert_eq!(
        params,
        serde_json::json!({
            "attempt_id":receipt.attempt.id, "closure":receipt.replay, "delivery":delivery,
        })
    );
    assert!(replay.is_none());
    assert_eq!(evidence, control_protocol::NativeNotifyEvidence::default());
}

fn control_path(client: &ManagedClient) -> std::path::PathBuf {
    let record = crate::process::worker_registry::load(&client.id)
        .unwrap()
        .unwrap();
    assert_eq!(
        Some(crate::acp::runner_lifecycle::RunnerIdentity::from_record(
            &record
        )),
        client.actual().connected_runner()
    );
    crate::process::worker::control_socket_sibling(&record.socket_path)
}

async fn blocked_callback(inspection: &RunnerInspection) {
    bounded(async {
        loop {
            let state = inspection.query("outbound-observation").await;
            assert!(state["native_loss"].is_null());
            if state["native_dispatching"] == true && state["callbacks_announced"] == false {
                assert_eq!(state["attached"], true);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
}

async fn observe(stream: &mut UnixStream, id: u64, index: usize) {
    control_protocol::write_frame(
        stream,
        &ControlBody::AgentCall {
            call_id: id,
            method: "session/list".into(),
            params: serde_json::json!({"index":index}),
            scope: NativeRequestScope::Observation,
        },
    )
    .await
    .unwrap();
}

async fn reverse(stream: &mut UnixStream, index: usize) -> u64 {
    match frame(stream).await {
        ControlBody::ServerCall {
            call_id,
            method,
            params,
            ..
        } => {
            assert_eq!(method, "x/transport-observation");
            assert_eq!(params, serde_json::json!({"index":index}));
            call_id
        }
        other => panic!("expected real reverse call, got {other:?}"),
    }
}

async fn answer(stream: &mut UnixStream, call_id: u64, index: usize) {
    control_protocol::write_frame(
        stream,
        &ControlBody::ServerResult {
            call_id,
            result: serde_json::json!({"index":index}),
        },
    )
    .await
    .unwrap();
}

async fn settled(stream: &mut UnixStream, expected: u64) {
    let body = frame(stream).await;
    assert!(
        matches!(&body, ControlBody::ServerCallSettled { call_id } if *call_id == expected),
        "expected settlement for reverse call {expected}, got {body:?}"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn runner_proxies_agent_requests_over_the_control_channel() {
    let name = test_name("runner_proxies_agent_requests_over_the_control_channel");
    if child_if_requested(&name).await {
        return;
    }
    with_managed(|scope| Box::pin(async move {
        let directory = scope.tempdir();
        let partial = directory.path().join("partial-header");
        scope.save_environment(&["AOE_E2E_PARTIAL_FRAME_FILE"]);
        std::env::set_var("AOE_E2E_PARTIAL_FRAME_FILE", &partial);
        let (original, mut native, _) = original(scope, &name, true).await;
        let id = original.id.clone();
        let path = control_path(&original);
        let adopted = scope.adopt_with(original, false, || async {
            let mut raw = attached(&path, &id).await;
            // Observation is the one explicitly authority-free forward method.
            // Split its real header while cached Initialize is independently in flight.
            control_protocol::write_frame(&mut raw, &ControlBody::Initialize {
                request: serde_json::json!({"protocolVersion":1}),
            }).await.unwrap();
            let wire = control_protocol::encode_frame(&ControlBody::AgentCall {
                call_id: 1, method: "session/list".into(),
                params: serde_json::json!({"index":0}), scope: NativeRequestScope::Observation,
            }).unwrap();
            let _ = std::fs::remove_file(&partial);
            raw.write_all(&wire[..2]).await.unwrap();
            bounded(async {
                while std::fs::read_to_string(&partial).ok().as_deref() != Some("2") {
                    tokio::task::yield_now().await;
                }
            }).await;
            assert!(matches!(frame(&mut raw).await, ControlBody::Initialized { .. }));
            raw.write_all(&wire[2..]).await.unwrap();
            let call = reverse(&mut raw, 0).await;
            answer(&mut raw, call, 0).await;
            answer(&mut raw, call, 999).await;
            let reply = bounded(native.reply()).await;
            let first_native_id = reply["id"].clone();
            assert_eq!(reply["result"], serde_json::json!({"index":0}));
            // Settlement releases callback custody; only the Native echo below supplies the result.
            settled(&mut raw, call).await;
            native.send(reply).await;
            assert!(matches!(frame(&mut raw).await,
                ControlBody::AgentResult { call_id:1, result } if result == serde_json::json!({"index":0})));
            for index in 0..16 { observe(&mut raw, 100 + index as u64, index).await; }
            let mut calls = HashSet::new();
            let mut settlements = HashSet::new();
            let mut next_callback = 0;
            while next_callback < 16 || settlements.len() < 16 {
                match frame(&mut raw).await {
                    ControlBody::ServerCall { call_id, method, params, .. } => {
                        assert!(next_callback < 16, "no extra reverse callback");
                        assert_eq!(method, "x/transport-observation");
                        assert_eq!(params, serde_json::json!({"index":next_callback}));
                        assert_ne!(call_id, call, "reverse call IDs never repeat");
                        assert!(calls.insert(call_id), "exactly one original callback");
                        answer(&mut raw, call_id, next_callback).await;
                        next_callback += 1;
                    }
                    ControlBody::ServerCallSettled { call_id } => {
                        assert!(calls.contains(&call_id), "only an answered callback can settle");
                        assert!(settlements.insert(call_id), "exactly one callback settlement");
                    }
                    other => panic!("expected reverse callback or its exact settlement, got {other:?}"),
                }
            }
            assert_eq!(settlements, calls);
            let mut ids = HashSet::new();
            for index in 0..16 {
                let reply = bounded(native.reply()).await;
                assert_ne!(reply["id"], first_native_id, "duplicate reply reached Native");
                assert!(ids.insert(reply["id"].as_i64().expect("actual Native request ID")));
                assert_eq!(reply["result"], serde_json::json!({"index":index}));
                native.send(reply).await;
                assert!(matches!(frame(&mut raw).await,
                    ControlBody::AgentResult { call_id, result }
                        if call_id == 100 + index as u64 && result == serde_json::json!({"index":index})));
            }
            assert!(tokio::time::timeout(Duration::from_millis(100), native.reply()).await.is_err(),
                "all reverse answers were delivered exactly once");
            drop(raw);
        }).await;
        adopted.ready().await;
        assert_eq!(native.requests().iter().filter(|m| *m == "initialize").count(), 1);
        assert_eq!(native.requests().iter().filter(|m| *m == "session/new").count(), 1);
        assert_eq!(native.requests().iter().filter(|m| *m == "session/list").count(), 17);
        adopted.shutdown().await.unwrap();
    })).await;
}

#[tokio::test]
#[serial_test::serial]
async fn runner_requeues_large_frame_after_stalled_writer() {
    let name = test_name("runner_requeues_large_frame_after_stalled_writer");
    if child_if_requested(&name).await {
        return;
    }
    with_managed(|scope| Box::pin(async move {
        let (original, native, inspection) = original(scope, &name, false).await;
        let receipt = original.actual().establishment_projection().unwrap().receipt_snapshot().unwrap();
        let path = control_path(&original);
        let id = original.id.clone();
        let adopted = scope.adopt_with(original, false, || async {
            let mut stalled = attached(&path, &id).await;
            assert!(matches!(cached_resume(&mut stalled).await,
                ControlBody::SessionReady { receipt: observed, .. } if observed.as_ref() == receipt.as_ref()));
            native.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{
                "sessionId":"managed-control-native","update":{
                    "sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"x".repeat(17*1024*1024)}
                }
            }})).await;
            let leased = bounded(async {
                loop {
                    let state = inspection.query("outbound-observation").await;
                    if let Some(entry) = state["entries"].as_array().unwrap().iter()
                        .find(|entry| entry["kind"] == "Notify" && entry["wire_bytes"].as_u64().unwrap() > 17*1024*1024)
                    {
                        if state["in_flight"] == entry["id"] { break entry.clone(); }
                    }
                    tokio::task::yield_now().await;
                }
            }).await;
            // Keep stalled open and unread. Only the actual writer's deadline can release its accept slot.
            let detached = bounded(async {
                loop {
                    let state = inspection.query("outbound-observation").await;
                    if state["attached"] == false {
                        let same = state["entries"].as_array().unwrap().iter().find(|entry| entry["id"] == leased["id"])
                            .expect("original full wire remains queued after the timed-out write");
                        assert_eq!(same, &leased);
                        break state;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }).await;
            assert!(detached["in_flight"].is_null());
            let mut replacement = attached(&path, &id).await;
            assert!(matches!(cached_resume(&mut replacement).await,
                ControlBody::SessionReady { receipt: observed, .. } if observed.as_ref() == receipt.as_ref()));
            let replay = loop {
                let mut length = [0; 4];
                bounded(replacement.read_exact(&mut length)).await.unwrap();
                let mut wire = vec![0; 4 + u32::from_be_bytes(length) as usize];
                wire[..4].copy_from_slice(&length);
                bounded(replacement.read_exact(&mut wire[4..])).await.unwrap();
                let body: serde_json::Value = serde_json::from_slice(&wire[4..]).unwrap();
                if body["kind"] == "notify" && body["method"] == "session/update" { break (wire, body); }
            };
            assert_eq!(replay.1["params"]["update"]["content"]["text"].as_str().unwrap().len(), 17*1024*1024);
            let digest: [u8; 32] = Sha256::digest(&replay.0).into();
            assert_eq!(leased["wire_bytes"], replay.0.len());
            assert_eq!(leased["wire_sha256"], serde_json::json!(digest));
            bounded(async {
                loop {
                    let state = inspection.query("outbound-observation").await;
                    if !state["entries"].as_array().unwrap().iter().any(|entry| entry["id"] == leased["id"]) { break; }
                    tokio::task::yield_now().await;
                }
            }).await;
            drop(replacement);
            drop(stalled);
        }).await;
        adopted.ready().await;
        assert_eq!(native.requests(), ["initialize", "session/new"]);
        adopted.shutdown().await.unwrap();
    })).await;
}

#[tokio::test]
#[serial_test::serial]
async fn runner_native_identity_is_announced_before_callbacks_and_never_reassigned() {
    let name =
        test_name("runner_native_identity_is_announced_before_callbacks_and_never_reassigned");
    if child_if_requested(&name).await {
        return;
    }
    with_managed(|scope| Box::pin(async move {
        let (original, mut native, inspection) = original(scope, &name, false).await;
        let path = control_path(&original);
        let id = original.id.clone();
        let receipt = original.actual().establishment_projection().unwrap().receipt_snapshot().unwrap();
        assert_eq!(receipt.session_id(), Some("managed-control-native"),
            "identity comes from the original Native session/new result");
        let adopted = scope.adopt_with(original, false, || async {
            let mut raw = attached(&path, &id).await;
            native.send(serde_json::json!({"jsonrpc":"2.0","id":"connection-elicitation","method":"elicitation/create","params":{
                "mode":"form","message":"Connection authentication","requestId":1,
                "requestedSchema":{"type":"object","properties":{}}
            }})).await;
            let connection_call = match frame(&mut raw).await {
                ControlBody::ServerCall { call_id, method, .. } => {
                    assert_eq!(method, "elicitation/create");
                    control_protocol::write_frame(&mut raw, &ControlBody::ServerResult {
                        call_id, result: serde_json::json!({"action":"cancel"}),
                    }).await.unwrap();
                    call_id
                }
                other => panic!("connection request must not require conversation authority: {other:?}"),
            };
            assert_eq!(bounded(native.reply()).await["id"], "connection-elicitation");
            settled(&mut raw, connection_call).await;
            native.send(serde_json::json!({"jsonrpc":"2.0","id":"positional-owned","method":"fs/read_text_file",
                "params":["managed-control-native","unused"]})).await;
            blocked_callback(&inspection).await;
            assert!(tokio::time::timeout(Duration::from_millis(100), control_protocol::read_frame(&mut raw)).await.is_err(),
                "session callback cannot overtake this attachment's cached identity");
            let ready = cached_resume(&mut raw).await;
            assert!(matches!(&ready,
                ControlBody::SessionReady { receipt: observed, .. } if observed.as_ref() == receipt.as_ref()));
            // Only the exact cached SessionReady receipt above releases session callbacks.
            // Hello's daemon session ID and callback metadata grant no Native identity.
            let mut callback = None;
            let mut callback_settled = false;
            let mut fence_seen = false;
            for _ in 0..3 {
                match frame(&mut raw).await {
                    ControlBody::ServerCall { call_id, method, params, .. } => {
                        assert!(callback.is_none(), "exactly one original callback");
                        callback = Some(call_id);
                        assert_eq!(method, "fs/read_text_file");
                        assert_eq!(params, serde_json::json!(["managed-control-native","unused"]));
                        control_protocol::write_frame(&mut raw, &ControlBody::ServerResult {
                            call_id, result: serde_json::json!({"content":"owned"}),
                        }).await.unwrap();
                    }
                    ControlBody::ServerCallSettled { call_id } => {
                        assert_eq!(Some(call_id), callback, "only the answered original callback settles");
                        assert!(!callback_settled, "exactly one callback settlement");
                        callback_settled = true;
                    }
                    body @ ControlBody::Notify { .. } => {
                        assert!(!fence_seen, "exactly one attachment replay fence");
                        assert_empty_replay_fence(body, &ready);
                        fence_seen = true;
                    }
                    other => panic!("expected callback, exact settlement, or replay fence after identity, got {other:?}"),
                }
            }
            assert!(callback.is_some() && callback_settled && fence_seen);
            let answer = bounded(native.reply()).await;
            assert_eq!(answer["id"], "positional-owned");
            assert_eq!(answer["result"]["content"], "owned");
            drop(raw);
            let mut old = attached(&path, &id).await;
            native.send(serde_json::json!({"jsonrpc":"2.0","id":"dead-attachment","method":"fs/read_text_file",
                "params":{"sessionId":"managed-control-native","path":"unused"}})).await;
            blocked_callback(&inspection).await;
            assert!(tokio::time::timeout(Duration::from_millis(100), control_protocol::read_frame(&mut old)).await.is_err());
            drop(old);
            let rejected = bounded(native.reply()).await;
            assert_eq!(rejected["id"], "dead-attachment");
            assert!(rejected["error"].is_object(), "dead attachment callback must not move to equal-SID replacement");
            let mut next = attached(&path, &id).await;
            let ready = cached_resume(&mut next).await;
            assert!(matches!(&ready,
                ControlBody::SessionReady { receipt: observed, .. } if observed.as_ref() == receipt.as_ref()));
            assert_empty_replay_fence(frame(&mut next).await, &ready);
            assert!(tokio::time::timeout(Duration::from_millis(100), control_protocol::read_frame(&mut next)).await.is_err());
            drop(next);
        }).await;
        adopted.ready().await;
        assert_eq!(adopted.actual().establishment_projection().unwrap().receipt_snapshot().unwrap().as_ref(), receipt.as_ref());
        assert_eq!(native.requests(), ["initialize", "session/new"]);
        adopted.shutdown().await.unwrap();
    })).await;
}

#[tokio::test]
#[serial_test::serial]
async fn streaming_during_reattach_does_not_overflow_pending_replay() {
    let name = test_name("streaming_during_reattach_does_not_overflow_pending_replay");
    if child_if_requested(&name).await {
        return;
    }
    with_managed(|scope| Box::pin(async move {
        let (original, mut native, inspection) = original(scope, &name, false).await;
        let receipt = original.actual().establishment_projection().unwrap().receipt_snapshot().unwrap();
        let cap = control_protocol::MAX_CONTROL_QUEUE_FRAMES;
        bounded(async {
            loop {
                let state = inspection.query("outbound-observation").await;
                assert_eq!(state["attached"], true);
                assert!(state["native_loss"].is_null());
                if state["in_flight"].is_null() && state["entries"].as_array().unwrap().is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        }).await;
        let (native_seen, seen) = tokio::sync::oneshot::channel();
        scope.configure_sdk(SdkTestOptions { native_session_seen: Some(native_seen), ..Default::default() });
        let chunk = |index: usize| serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{
            "sessionId":"managed-control-native","update":{
                "sessionUpdate":"agent_message_chunk","content":{"type":"text","text":format!("chunk-{index}")}
            }
        }});
        let (begin, begin_backlog) = tokio::sync::oneshot::channel();
        let (filled, backlog_ready) = tokio::sync::oneshot::channel();
        let (return_native, returned) = tokio::sync::oneshot::channel();
        // This helper already owns the real Native writer before reattachment starts.
        let stream = scope.spawn_helper(async move {
            bounded(begin_backlog).await.unwrap();
            for index in 0..cap { native.send(chunk(index)).await; }
            // A real detached reverse response proves stdout consumed the whole backlog.
            native.send(serde_json::json!({"jsonrpc":"2.0","id":"backlog-read","method":"x/barrier","params":{}})).await;
            let reply = bounded(native.reply()).await;
            assert_eq!(reply["id"], "backlog-read");
            filled.send(()).unwrap();
            // This producer starts its continuing stream at the production reader's
            // actual SessionReady boundary, not after the caller observes readiness.
            // Never fence bootstrap while adding unbounded live traffic.
            bounded(seen).await.unwrap();
            for index in cap..cap + 2048 {
                native.send(chunk(index)).await;
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            return_native.send(native).ok().expect("return original Native peer after streaming");
        });
        let mut resumed = scope.adopt_with(original, false, || async {
            begin.send(()).unwrap();
            bounded(backlog_ready).await.unwrap();
            let state = inspection.query("outbound-observation").await;
            assert_eq!(state["attached"], false);
            assert!(state["in_flight"].is_null());
            assert!(state["native_loss"].is_null());
            let entries = state["entries"].as_array().unwrap();
            assert_eq!(entries.len(), cap, "only the genuine detached Native backlog remains");
            assert!(entries.windows(2).all(|pair|
                pair[0]["id"].as_u64().unwrap() < pair[1]["id"].as_u64().unwrap()));
            let queued_bytes: u64 = entries.iter().map(|entry| entry["wire_bytes"].as_u64().unwrap()).sum();
            assert!(queued_bytes + control_protocol::MAX_NATIVE_LOSS_FRAME_BYTES as u64
                <= control_protocol::MAX_CONTROL_QUEUE_BYTES as u64);
            assert_eq!(state["entries"].as_array().unwrap().iter().filter(|entry| entry["kind"] == "Notify").count(), cap);
        }).await;
        resumed.ready().await;
        let mut chunks = Vec::new();
        let mut assigned = false;
        bounded(async {
            while chunks.len() < cap + 2048 || !assigned {
                match resumed.next_event().await.unwrap() {
                    Event::AcpSessionAssigned { acp_session_id, .. } => {
                        assert_eq!(acp_session_id, "managed-control-native");
                        assigned = true;
                    }
                    Event::AgentMessageChunk { text, .. } => chunks.push(text),
                    Event::AgentStartupError { message, .. } => panic!("pending replay overflowed: {message}"),
                    Event::AcpConnectionUnavailable { .. } => panic!("actual Native owner must remain live"),
                    Event::Stopped { .. } => panic!("silence and output never complete a Native turn"),
                    _ => {}
                }
            }
        }).await;
        stream.await.unwrap();
        let native = returned.await.unwrap();
        assert_eq!(chunks, (0..cap + 2048).map(|index| format!("chunk-{index}")).collect::<Vec<_>>());
        assert_eq!(native.requests(), ["initialize", "session/new"]);
        assert_eq!(resumed.actual().establishment_projection().unwrap().receipt_snapshot().unwrap().as_ref(), receipt.as_ref());
        resumed.shutdown().await.unwrap();
    })).await;
}

#[tokio::test]
#[serial_test::serial]
async fn fresh_native_identity_precedes_its_immediate_callback() {
    let name = test_name("fresh_native_identity_precedes_its_immediate_callback");
    if child_if_requested(&name).await {
        return;
    }
    with_managed(|scope| Box::pin(async move {
        let directory = scope.tempdir();
        let own_file = directory.path().join("owned");
        std::fs::write(&own_file, "original callback content").unwrap();
        let inspection = scope.inspect_next_runner(&name);
        let (new_request, requested) = tokio::sync::oneshot::channel();
        let mut new_request = Some(new_request);
        let (config, mut native) = scope.native_peer(directory.path().into(), "codex", "codex", move |request| {
            let id = request.get("id")?;
            match request["method"].as_str().unwrap() {
                "initialize" => Some(serde_json::json!({"jsonrpc":"2.0","id":id,
                    "result":{"protocolVersion":1,"agentCapabilities":{}}})),
                "session/new" => {
                    new_request.take().unwrap().send(id.clone()).unwrap();
                    None
                }
                method => panic!("unexpected native request: {method}"),
            }
        });
        let (id, events) = scope.create_observed(config, "immediate-owned-callback").await;
        let native_id = bounded(requested).await.unwrap();
        assert_eq!(inspection.query("observe-producer").await["observing"], true);
        native.send(serde_json::json!({"jsonrpc":"2.0","id":native_id,
            "result":{"sessionId":"immediate-native-owner"}})).await;
        native.send(serde_json::json!({"jsonrpc":"2.0","id":"immediate-original-callback",
            "method":"fs/read_text_file","params":{"sessionId":"immediate-native-owner","path":own_file}})).await;
        let client = scope.observe_external_resume(id, events).await;
        client.ready().await;
        let reply = bounded(native.reply()).await;
        assert_eq!(reply["id"], "immediate-original-callback");
        assert_eq!(reply["result"]["content"], "original callback content");
        let receipt = client.actual().establishment_projection().unwrap().receipt_snapshot().unwrap();
        assert_eq!(client.actual().connected_runner(), Some(receipt.owner));
        assert!(inspection.native_spawn_proof(&client.lease).unwrap().is_some());
        bounded(async {
            loop {
                let observation = inspection.query("producer-snapshot").await;
                let events = observation["events"].as_array().unwrap();
                let identity = events.iter().position(|event|
                    event["stage"] == "committed" && event["body"]["kind"] == "session_ready");
                let callback = events.iter().position(|event|
                    event["stage"] == "committed" && event["body"]["kind"] == "server_call"
                        && event["body"]["params"]["path"] == serde_json::json!(own_file));
                if let (Some(identity), Some(callback)) = (identity, callback) {
                    assert!(identity < callback, "actual full identity write and flush precedes the immediate callback");
                    break;
                }
                tokio::task::yield_now().await;
            }
        }).await;
        assert_eq!(native.requests(), ["initialize", "session/new"]);
        client.shutdown().await.unwrap();
    })).await;
}
