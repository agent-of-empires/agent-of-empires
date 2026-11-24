//! Control recovery preserves the original resources and turn ownership.

use super::*;
use crate::acp::control_protocol::{ControlBody, PromptCompletedMarker};
use crate::acp::establishment::NativeRequestScope;
use crate::acp::event_sender::EventScope;
use crate::acp::protocol::{AcpOriginalRequestResult, AcpPromptObservation, AcpPromptWindowId};
use crate::acp::runner_lifecycle::RunnerIdentity;
use crate::acp::state::{Event, NativeEventOrigin};
use crate::acp::supervisor::test_support::managed::{
    with_managed, ManagedClient, ManagedScope, NativePeer,
};
use crate::process::runner::test_support::managed_runner::RunnerInspection;
use futures_util::FutureExt as _;
use serde_json::{json, Value};
use std::future::Future;
use std::time::Duration;
use tokio::time::{timeout, timeout_at, Instant};

const BOUND: Duration = Duration::from_secs(15);

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    timeout(BOUND, future)
        .await
        .expect("actual recovery boundary completes")
}

#[derive(Clone)]
struct OriginalPrompt {
    request: Value,
    scope: NativeRequestScope,
    window: AcpPromptWindowId,
    origin: NativeEventOrigin,
    prefix: u64,
}

impl OriginalPrompt {
    fn id(&self) -> i64 {
        self.request["id"]
            .as_i64()
            .expect("runner allocates a numeric Native prompt ID")
    }
}

struct Runner {
    client: ManagedClient,
    native: NativePeer,
    requests: mpsc::UnboundedReceiver<Value>,
    controls: mpsc::UnboundedReceiver<ControlBody>,
    inspection: RunnerInspection,
    directory: Arc<tempfile::TempDir>,
    store: Arc<crate::acp::event_store::EventStore>,
    sid: String,
    callback: u64,
    events: Vec<Event>,
}

impl Runner {
    async fn new(scope: &mut ManagedScope) -> Self {
        let directory = scope.tempdir();
        let name = std::thread::current().name().unwrap().to_owned();
        let inspection = scope.inspect_next_runner(&name);
        let sid = format!("native-{}", uuid::Uuid::new_v4());
        let selected = sid.clone();
        let (seen, requests) = mpsc::unbounded_channel();
        let (control_tx, controls) = mpsc::unbounded_channel();
        scope.configure_sdk(SdkTestOptions {
            control_output: Some(control_tx),
            ..Default::default()
        });
        let (config, native) =
            scope.native_peer(directory.path().into(), "codex", "codex", move |request| {
                seen.send(request.clone()).unwrap();
                match request["method"].as_str().unwrap() {
                    "initialize" => Some(json!({
                        "jsonrpc":"2.0", "id":request["id"],
                        "result":{"protocolVersion":1,"agentCapabilities":{}}
                    })),
                    "session/new" => Some(json!({
                        "jsonrpc":"2.0", "id":request["id"],
                        "result":{"sessionId":selected}
                    })),
                    "session/prompt" => {
                        assert_eq!(request["params"]["sessionId"], selected);
                        None
                    }
                    "session/cancel" => {
                        assert_eq!(request["params"]["sessionId"], selected);
                        None
                    }
                    method => panic!("unexpected Native operation: {method}"),
                }
            });
        let client = scope
            .spawn(config, format!("recovery-{sid}"))
            .await
            .unwrap();
        client.ready().await;
        let store = Arc::clone(&scope.state.acp_event_store);
        let mut runner = Self {
            client,
            native,
            requests,
            controls,
            inspection,
            directory,
            store,
            sid,
            callback: 0,
            events: Vec::new(),
        };
        runner.assert_owner().await;
        runner.assigned().await;
        assert_eq!(runner.native.requests(), ["initialize", "session/new"]);
        runner
    }

    fn actual(&self) -> Arc<AcpClient> {
        self.client.actual()
    }

    fn state(&self) -> Arc<SessionState> {
        Arc::clone(&self.actual().reattach.as_ref().unwrap().state)
    }

    async fn assert_owner(&self) -> RunnerIdentity {
        let owner = self.actual().connected_runner().unwrap();
        assert_ne!(owner.pid, std::process::id());
        let record = bounded(async {
            loop {
                match crate::process::worker_registry::load_strict(&self.client.id) {
                    Err(error) if error.is::<crate::process::worker_registry::RegistryBusy>() => {
                        tokio::task::yield_now().await;
                    }
                    result => {
                        break result
                            .unwrap()
                            .expect("actual owned runner has registry custody")
                    }
                }
            }
        })
        .await;
        assert_eq!(RunnerIdentity::from_record(&record), owner);
        let birth = crate::process::process_birth_identity(owner.pid)
            .unwrap()
            .expect("same original physical runner remains live");
        assert_eq!(owner.birth_fingerprint, Some(birth.fingerprint()));
        owner
    }

    fn observe_controls(&mut self) {
        let (send, receive) = mpsc::unbounded_channel();
        *self
            .actual()
            .test_connection_state()
            .test_control_output
            .lock()
            .unwrap() = Some(send);
        self.controls = receive;
    }

    async fn recv(&mut self) -> Event {
        let event = bounded(self.client.next_event())
            .await
            .expect("actual managed publication remains available");
        if let Event::AgentStartupError { message, .. } = &event {
            panic!("unexpected startup failure: {message}");
        }
        self.events.push(event.clone());
        event
    }

    async fn assigned(&mut self) {
        loop {
            if let Event::AcpSessionAssigned {
                acp_session_id,
                connection_ready,
                ..
            } = self.recv().await
            {
                assert_eq!(acp_session_id, self.sid);
                assert!(connection_ready);
                assert_eq!(self.actual().selected_native_session(), self.sid);
                assert!(self
                    .actual()
                    .establishment_projection()
                    .unwrap()
                    .applied()
                    .is_some());
                return;
            }
        }
    }

    async fn native_request(&mut self, method: &str) -> Value {
        bounded(async {
            loop {
                let request = self
                    .requests
                    .recv()
                    .await
                    .expect("actual Native reader remains live");
                if request["method"] == method {
                    return request;
                }
                assert!(
                    matches!(
                        request["method"].as_str(),
                        Some("initialize" | "session/new")
                    ),
                    "unexpected queued Native operation while awaiting {method}: {request}"
                );
            }
        })
        .await
    }

    async fn start(&mut self, text: &str) -> OriginalPrompt {
        let prefix = self.store.highest_seq(&self.client.id).unwrap();
        self.client.send_prompt(text, &[]).await.unwrap();
        let request = self.native_request("session/prompt").await;
        let control = self.actual().test_control();
        let (scope, window) = bounded(async {
            loop {
                let frozen = {
                    let completion = control.completion.lock().unwrap();
                    match &*completion {
                        control::PromptCompletion::Pending {
                            prompt_req_id: Some(id),
                            scope,
                            window_id: Some(window),
                            ..
                        } if Some(*id) == request["id"].as_i64() => Some((scope.clone(), *window)),
                        _ => None,
                    }
                };
                if let Some(frozen) = frozen {
                    break frozen;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        let receipt = self
            .actual()
            .establishment_projection()
            .unwrap()
            .receipt_snapshot()
            .unwrap();
        let origin = scope.origin(Some(receipt.as_ref())).unwrap();
        assert_eq!(scope.owner(), Some(self.assert_owner().await));
        assert_eq!(origin.owner, self.actual().connected_runner());
        assert!(scope.correlation().is_some());
        OriginalPrompt {
            request,
            scope,
            window,
            origin,
            prefix,
        }
    }

    async fn adopt(mut self, scope: &mut ManagedScope, original: &OriginalPrompt) -> Self {
        let owner = self.assert_owner().await;
        let (send, receive) = mpsc::unbounded_channel();
        scope.configure_sdk(SdkTestOptions {
            control_output: Some(send),
            ..Default::default()
        });
        self.client = scope.adopt(self.client, true).await;
        self.controls = receive;
        self.client.ready().await;
        self.assigned().await;
        assert_eq!(self.assert_owner().await, owner);
        let control = self.actual().test_control();
        let completion = control.completion.lock().unwrap();
        let control::PromptCompletion::Adopted(Some(head)) = &*completion else {
            panic!("actual adoption must retain the genuine in-flight Native head");
        };
        assert_eq!(head.prompt_req_id, original.id());
        assert_eq!(head.scope, original.scope);
        assert_eq!(head.window_id, original.window);
        drop(completion);
        self
    }

    async fn close_control(&self) -> Arc<AcpClient> {
        let actual = self.actual();
        let owner = self.assert_owner().await;
        let closed = self.inspection.query("close-control").await;
        assert!(closed["closed_attachment"].as_u64().is_some());
        bounded(async {
            while actual.control_loss().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert_eq!(actual.connected_runner(), Some(owner));
        actual
    }

    async fn recover(&mut self, previous: &Arc<AcpClient>) {
        let owner = previous.connected_runner().unwrap();
        let state = Arc::clone(&previous.reattach.as_ref().unwrap().state);
        let lease = self.client.lease.clone();
        self.client.refresh_after_control_loss().await;
        assert!(!Arc::ptr_eq(previous, &self.actual()));
        assert_eq!(self.client.lease, lease);
        assert_eq!(self.assert_owner().await, owner);
        assert!(Arc::ptr_eq(&state, &self.state()));
        self.observe_controls();
        self.assigned().await;
        bounded(previous.observe_sdk_task()).await.unwrap();
        self.client.test_flush_commands().await;
    }

    async fn reply(&self, original: &OriginalPrompt, reason: &str) {
        self.native
            .send(json!({
                "jsonrpc":"2.0", "id":original.request["id"], "result":{"stopReason":reason}
            }))
            .await;
    }

    async fn quota(&self, original: &OriginalPrompt) {
        self.native
            .send(json!({
                "jsonrpc":"2.0", "id":original.request["id"],
                "error":{"code":-32000,"message":"rate limit exceeded",
                    "data":{"errorKind":"rate_limit","privateTokenMarker":"must-not-leak"}}
            }))
            .await;
    }

    async fn update(&self, update: Value) {
        self.native
            .send(json!({
                "jsonrpc":"2.0","method":"session/update",
                "params":{"sessionId":self.sid,"update":update}
            }))
            .await;
    }

    async fn call(&mut self, method: &str, mut params: Value) -> Value {
        self.callback += 1;
        let id = format!("recovery-callback-{}", self.callback);
        params["sessionId"] = self.sid.clone().into();
        self.native
            .send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await;
        let reply = bounded(self.native.reply()).await;
        assert_eq!(reply["id"], id);
        reply
    }

    async fn barrier(&mut self) {
        self.callback += 1;
        let id = format!("ordered-denial-{}", self.callback);
        let forbidden = self.directory.path().join(&id);
        assert!(!forbidden.exists());
        self.native.send(json!({
            "jsonrpc":"2.0","id":id,"method":"fs/write_text_file",
            "params":{"sessionId":"not-the-selected-session","path":forbidden,"content":"forbidden"}
        })).await;
        let denied = bounded(self.native.reply()).await;
        assert_eq!(denied["id"], id);
        assert_eq!(denied["error"]["code"], -32602);
        assert!(!forbidden.exists());
    }

    async fn cancel(&mut self) -> NativeRequestScope {
        let owner = self.assert_owner().await;
        self.client.cancel_prompt().await.unwrap();
        let frame = bounded(async {
            loop {
                let frame = self
                    .controls
                    .recv()
                    .await
                    .expect("actual control output observer");
                if matches!(frame, ControlBody::Cancel { .. }) {
                    break frame;
                }
            }
        })
        .await;
        let ControlBody::Cancel { scope } = frame else {
            unreachable!()
        };
        self.native_request("session/cancel").await;
        assert_eq!(self.assert_owner().await, owner);
        assert_eq!(scope.owner(), Some(owner));
        assert!(scope.correlation().is_some());
        scope
    }

    async fn event_since(&mut self, start: usize, wanted: impl Fn(&Event) -> bool) -> usize {
        bounded(async {
            loop {
                if let Some(index) = self.events[start..].iter().position(&wanted) {
                    return start + index;
                }
                self.recv().await;
            }
        })
        .await
    }

    async fn stopped_since(&mut self, start: usize, expected: &str) -> usize {
        let index = self
            .event_since(start, |event| matches!(event, Event::Stopped { .. }))
            .await;
        assert!(
            matches!(&self.events[index], Event::Stopped { reason } if reason == expected),
            "unexpected terminal: {:?}",
            self.events[index]
        );
        index
    }

    fn drain_observer(&mut self) {
        while let Ok(event) = self.client.try_recv() {
            assert!(!matches!(event, Event::AgentStartupError { .. }));
            self.events.push(event);
        }
    }

    fn assert_original(
        &self,
        original: &OriginalPrompt,
        outcome: AcpOriginalRequestResult,
        scope: EventScope,
    ) {
        let stored = self
            .store
            .replay_from(&self.client.id, original.prefix)
            .unwrap();
        let terminals: Vec<_> = stored
            .iter()
            .filter(|stored| {
                matches!(stored.event.as_ref(), Event::Stopped { .. })
                    && stored.metadata.native_prompt_id == Some(original.id())
            })
            .collect();
        assert_eq!(
            terminals.len(),
            1,
            "the exact original Native result publishes once"
        );
        let metadata = &terminals[0].metadata;
        assert_eq!(metadata.scope, scope);
        assert_eq!(metadata.native_origin, Some(original.origin));
        assert_eq!(
            metadata.operation.unwrap().correlation,
            original.scope.correlation().unwrap()
        );
        assert_eq!(metadata.operation.unwrap().outcome, Some(outcome));
        assert_eq!(
            metadata.prompt_observation,
            Some(AcpPromptObservation::Written {
                window_id: original.window
            })
        );
        assert!(!metadata.host_action);
        assert!(!serde_json::to_string(&self.events)
            .unwrap()
            .contains("must-not-leak"));
    }

    async fn assert_cancel_grace(&self, original: &CancelGrace) {
        let state = self.state();
        let grace = state
            .cancel_deadline
            .lock()
            .unwrap()
            .clone()
            .expect("retained cancellation grace");
        assert_eq!(grace.deadline, original.deadline);
        assert_eq!(grace.turn, original.turn);
        assert_eq!(grace.turn, state.terminal_claim.current_turn());
        let before = original
            .scope
            .as_ref()
            .expect("genuine Native cancel context");
        let after = grace
            .scope
            .as_ref()
            .expect("genuine recovered Native cancel context");
        assert_eq!(after.scope, before.scope);
        assert_eq!(after.receipt, before.receipt);
        assert_eq!(after.origin(), before.origin());
        assert_eq!(after.correlation(), before.correlation());
        assert_eq!(after.scope.owner(), Some(self.assert_owner().await));
        assert!(after.correlation().is_some());
        assert!(after.receipt.is_some());
    }

    async fn no_terminal_before(&mut self, deadline: Instant) {
        loop {
            match timeout_at(deadline, self.client.next_event()).await {
                Err(_) => return,
                Ok(Some(event)) => {
                    assert!(
                        !matches!(
                            event,
                            Event::Stopped { .. } | Event::AgentStartupError { .. }
                        ),
                        "{event:?}"
                    );
                    self.events.push(event);
                }
                Ok(None) => panic!("the recovered actor ended before its original grace"),
            }
        }
    }

    async fn finish_cancel(&mut self, grace: &CancelGrace, start: usize) {
        self.no_terminal_before(grace.deadline - Duration::from_millis(1))
            .await;
        self.stopped_since(start, "agent_unresponsive").await;
        let actual = self.actual();
        bounded(actual.observe_sdk_task()).await.unwrap();
        let stored = self.store.replay_from(&self.client.id, 0).unwrap();
        let terminals: Vec<_> = stored.iter().filter(|stored| {
            matches!(stored.event.as_ref(), Event::Stopped { reason } if reason == "agent_unresponsive")
        }).collect();
        assert_eq!(
            terminals.len(),
            1,
            "the retained turn escalates exactly once"
        );
        let operation = terminals[0]
            .metadata
            .operation
            .expect("the retained cancel operation owns escalation");
        assert_eq!(
            operation.correlation,
            grace.scope.as_ref().unwrap().correlation().unwrap()
        );
        assert_eq!(
            operation.outcome, None,
            "timeout cannot forge a positive Native result"
        );
        assert_eq!(terminals[0].metadata.native_prompt_id, None);
        self.client.shutdown().await.unwrap();
        self.drain_observer();
        assert_eq!(
            self.events
                .iter()
                .filter(|event| matches!(event,
            Event::Stopped { reason } if reason == "agent_unresponsive"))
                .count(),
            1
        );
    }

    async fn shutdown(&mut self) {
        self.client.shutdown().await.unwrap();
        self.drain_observer();
        assert!(!self
            .events
            .iter()
            .any(|event| matches!(event, Event::AgentStartupError { .. })));
    }
}

async fn armed(state: &Arc<SessionState>) -> CancelGrace {
    bounded(async {
        loop {
            if let Some(grace) = state.cancel_deadline.lock().unwrap().clone() {
                break grace;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
}

#[tokio::test]
#[serial_test::serial]
async fn terminal_callbacks_keep_their_ids_after_control_recovery() {
    with_managed(|scope| {
        Box::pin(async move {
            let mut runner = Runner::new(scope).await;
            let created = runner
                .call(
                    "terminal/create",
                    json!({"command":"echo","args":["retained"]}),
                )
                .await;
            assert!(created.get("error").is_none(), "{created}");
            let id = created["result"]["terminalId"].as_str().unwrap().to_owned();
            let old = runner.close_control().await;
            runner.recover(&old).await;
            let fields = json!({"terminalId":id});
            let exited = runner.call("terminal/wait_for_exit", fields.clone()).await;
            assert_eq!(exited["result"]["exitCode"], 0);
            let output = runner.call("terminal/output", fields.clone()).await;
            assert_eq!(output["result"]["output"], "retained\n");
            let released = runner.call("terminal/release", fields.clone()).await;
            assert!(released.get("error").is_none(), "{released}");
            let missing = runner.call("terminal/output", fields).await;
            assert!(
                missing.get("error").is_some(),
                "released original terminal must not reappear: {missing}"
            );
            assert_eq!(runner.native.requests(), ["initialize", "session/new"]);
            assert!(!runner
                .events
                .iter()
                .any(|event| matches!(event, Event::Stopped { .. })));
            runner.shutdown().await;
        })
    })
    .await;
}

#[tokio::test]
#[serial_test::serial]
async fn completed_turn_keeps_its_terminal_claim_when_eof_snapshots_it_active() {
    with_managed(|scope| {
        Box::pin(async move {
            let mut runner = Runner::new(scope).await;
            let original = runner
                .start("complete before the active snapshot is cleared")
                .await;
            let state = runner.state();
            let turn = state.terminal_claim.current_turn();
            let (entered, reached) = oneshot::channel();
            let (release, released) = oneshot::channel();
            assert!(state
                .terminal_before_clear
                .lock()
                .unwrap()
                .replace((entered, released))
                .is_none());
            let start = runner.events.len();
            runner.reply(&original, "end_turn").await;
            bounded(reached).await.unwrap();
            runner.stopped_since(start, "prompt_complete").await;
            assert!(state.terminal_claim.claimed());
            assert_eq!(state.terminal_claim.current_turn(), turn);
            runner.assert_original(
                &original,
                AcpOriginalRequestResult::Succeeded,
                EventScope::OperationCurrent,
            );
            let old = runner.close_control().await;
            assert!(
                old.control_loss().unwrap().turn_in_flight,
                "actual EOF captures the active flag before original publication cleanup"
            );
            assert!(!old.claim_lost_turn(old.control_loss().unwrap()));
            release.send(()).unwrap();
            runner.recover(&old).await;
            runner.barrier().await;
            runner.client.test_flush_commands().await;
            runner.drain_observer();
            assert_eq!(
                runner
                    .events
                    .iter()
                    .filter(|event| matches!(event, Event::Stopped { .. }))
                    .count(),
                1
            );
            assert_eq!(state.terminal_claim.current_turn(), turn);
            let replayed = runner.close_control().await;
            assert!(
                !replayed.control_loss().unwrap().turn_in_flight,
                "replayed completion cannot keep the recovered turn active"
            );
            runner.recover(&replayed).await;
            runner.assert_original(
                &original,
                AcpOriginalRequestResult::Succeeded,
                EventScope::OperationCurrent,
            );
            assert_eq!(
                runner
                    .native
                    .requests()
                    .iter()
                    .filter(|method| *method == "session/prompt")
                    .count(),
                1
            );
            runner.shutdown().await;
        })
    })
    .await;
}

#[tokio::test]
#[serial_test::serial]
async fn terminal_publication_survives_eof_while_the_event_channel_is_full() {
    with_managed(|scope| {
        Box::pin(async move {
            for (local, limited, cancel) in [
                (true, false, false),
                (false, false, false),
                (true, true, false),
                (false, true, false),
                (false, true, true),
            ] {
                let mut runner = Runner::new(scope).await;
                let original = runner
                    .start("real original under publication backpressure")
                    .await;
                if !local {
                    runner = runner.adopt(scope, &original).await;
                }
                let actual = runner.actual();
                let state = runner.state();
                let turn = state.terminal_claim.current_turn();
                if cancel {
                    let start = runner.events.len();
                    let sent = runner.cancel().await;
                    runner
                        .event_since(start, |event| {
                            matches!(event, Event::CancelRequested { .. })
                        })
                        .await;
                    let grace = armed(&state).await;
                    assert_eq!(sent, grace.scope.as_ref().unwrap().scope);
                    runner.assert_cancel_grace(&grace).await;
                }
                let start = runner.events.len();
                let pause = runner.client.pause_drain().await;
                let sender = actual.test_event_queue().upgrade().unwrap();
                let capacity = sender.capacity();
                assert!(capacity > 0, "drain ACK precedes the deliberate pressure");
                while let Some(result) = sender
                    .historical()
                    .send(Event::AuthStatusUpdated { status: None })
                    .now_or_never()
                {
                    result.unwrap();
                }
                assert_eq!(sender.capacity(), 0);
                let (observed, mut relay) = mpsc::unbounded_channel();
                *actual
                    .test_connection_state()
                    .test_relay_observer
                    .lock()
                    .unwrap() = Some(observed);
                let (entered, reached) = oneshot::channel();
                let (release, released) = oneshot::channel();
                *actual
                    .test_connection_state()
                    .test_prompt_completed
                    .lock()
                    .unwrap() = Some((entered, released));
                let queued = capacity + if cancel { 2 } else { 0 };
                for index in 0..queued {
                    runner
                        .update(json!({"sessionUpdate":"agent_message_chunk",
                    "content":{"type":"text","text":format!("queued-{index}")}}))
                        .await;
                }
                if limited {
                    runner.quota(&original).await;
                } else {
                    runner.reply(&original, "end_turn").await;
                }
                bounded(reached).await.unwrap();
                assert_eq!(sender.capacity(), 0);
                assert!(
                    !state.terminal_claim.claimed(),
                    "control receipt is not ordered SDK publication"
                );
                assert_eq!(state.terminal_claim.current_turn(), turn);
                release.send(()).unwrap();
                bounded(async {
                    loop {
                        let observation = relay
                            .recv()
                            .await
                            .expect("actual completion relay remains observed");
                        if observation.stage == control::TestRelayStage::Relayed
                            && observation.payload["method"] == "_aoe/prompt_completed"
                        {
                            let marker: PromptCompletedMarker =
                                serde_json::from_value(observation.payload["params"].clone())
                                    .unwrap();
                            assert_eq!(
                                marker.prompt_req_id.or_else(|| marker
                                    .adopted
                                    .as_ref()
                                    .map(|adopted| adopted.prompt_req_id)),
                                Some(original.id())
                            );
                            if !local {
                                let adopted = marker.adopted.unwrap();
                                let evidence = adopted.evidence.unwrap();
                                assert_eq!(evidence.scope, original.scope);
                                assert_eq!(evidence.window_id, original.window);
                                assert_eq!(evidence.origin, original.origin);
                            }
                            break;
                        }
                    }
                })
                .await;
                runner.barrier().await;
                assert_eq!(
                    sender.capacity(),
                    0,
                    "the public lane remains full at actual control EOF"
                );
                let old = runner.close_control().await;
                assert!(old.control_loss().unwrap().turn_in_flight);
                drop(sender);
                drop(pause);
                runner.recover(&old).await;
                let stop = runner
                    .stopped_since(
                        start,
                        if limited {
                            "rate_limited"
                        } else {
                            "prompt_complete"
                        },
                    )
                    .await;
                runner.barrier().await;
                runner.client.test_flush_commands().await;
                runner.drain_observer();
                assert!(
                    !old.claim_lost_turn(old.control_loss().unwrap()),
                    "fallback cannot settle the published original twice"
                );
                assert!(state.terminal_claim.claimed());
                assert_eq!(state.terminal_claim.current_turn(), turn);
                assert!(state.cancel_deadline.lock().unwrap().is_none());
                let events = &runner.events[start..];
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| matches!(event, Event::Stopped { .. }))
                        .count(),
                    1
                );
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| matches!(event, Event::RateLimit { .. }))
                        .count(),
                    usize::from(limited)
                );
                let chunks: Vec<_> = runner.events[start..stop]
                    .iter()
                    .filter_map(|event| {
                        if let Event::AgentMessageChunk { text, .. } = event {
                            Some(text.clone())
                        } else {
                            None
                        }
                    })
                    .collect();
                assert_eq!(
                    chunks,
                    (0..queued)
                        .map(|index| format!("queued-{index}"))
                        .collect::<Vec<_>>()
                );
                runner.assert_original(
                    &original,
                    if limited {
                        AcpOriginalRequestResult::Failed
                    } else {
                        AcpOriginalRequestResult::Succeeded
                    },
                    if local {
                        EventScope::OperationCurrent
                    } else {
                        EventScope::HistoryOnly
                    },
                );
                assert_eq!(
                    runner
                        .native
                        .requests()
                        .iter()
                        .filter(|method| *method == "session/prompt")
                        .count(),
                    1
                );
                runner.shutdown().await;
            }
        })
    })
    .await;
}

#[tokio::test]
#[serial_test::serial]
async fn unfinished_lost_turn_is_claimed_once_by_the_fallback() {
    with_managed(|scope| {
        Box::pin(async move {
            let mut runner = Runner::new(scope).await;
            let original = runner.start("unfinished genuine original").await;
            let state = runner.state();
            let turn = state.terminal_claim.current_turn();
            let pause = runner.client.pause_drain().await;
            let old = runner.close_control().await;
            let loss = old.control_loss().unwrap();
            assert!(loss.turn_in_flight);
            assert!(!state.terminal_claim.claimed());
            assert!(old.claim_lost_turn(loss));
            assert!(
                !old.claim_lost_turn(loss),
                "one fallback claim per actual lost turn"
            );
            assert_eq!(state.terminal_claim.current_turn(), turn);
            assert!(
                !runner
                    .store
                    .replay_from(&runner.client.id, original.prefix)
                    .unwrap()
                    .iter()
                    .any(|stored| {
                        stored
                            .metadata
                            .operation
                            .is_some_and(|operation| operation.outcome.is_some())
                    }),
                "claiming a lost slot cannot fabricate a Native outcome"
            );
            drop(pause);
            runner.recover(&old).await;
            assert!(state.terminal_claim.claimed());
            assert!(!old.claim_lost_turn(loss));
            let start = runner.events.len();
            runner.reply(&original, "end_turn").await;
            runner.stopped_since(start, "prompt_complete").await;
            runner.assert_original(
                &original,
                AcpOriginalRequestResult::Succeeded,
                EventScope::HistoryOnly,
            );
            runner.shutdown().await;
        })
    })
    .await;
}

#[tokio::test]
#[serial_test::serial]
async fn cancelling_turn_recovers_with_only_the_original_grace_remaining() {
    with_managed(|scope| {
        Box::pin(async move {
            let mut runner = Runner::new(scope).await;
            let _original = runner.start("cancel before control recovery").await;
            let state = runner.state();
            let start = runner.events.len();
            let sent = runner.cancel().await;
            runner
                .event_since(start, |event| {
                    matches!(event, Event::CancelRequested { .. })
                })
                .await;
            let grace = armed(&state).await;
            assert_eq!(sent, grace.scope.as_ref().unwrap().scope);
            runner.assert_cancel_grace(&grace).await;
            tokio::time::pause();
            tokio::time::advance(Duration::from_secs(6)).await;
            tokio::time::resume();
            let old = runner.close_control().await;
            runner.recover(&old).await;
            runner.assert_cancel_grace(&grace).await;
            let repeated = runner.cancel().await;
            assert_ne!(
                repeated.correlation(),
                sent.correlation(),
                "two host cancel operations have distinct admission"
            );
            runner.client.test_flush_commands().await;
            runner.assert_cancel_grace(&grace).await;
            runner.finish_cancel(&grace, start).await;
        })
    })
    .await;
}

#[tokio::test]
#[serial_test::serial]
async fn captured_cancel_survives_physical_admission_contention() {
    with_managed(|scope| {
        Box::pin(async move {
            let mut runner = Runner::new(scope).await;
            let original = runner
                .start("cancel across actual lifecycle exclusion")
                .await;
            let owner = runner.assert_owner().await;
            let state = runner.state();
            let connection = runner.actual().test_connection_state();
            let gate = state.cancel_send_gate.write().await;
            let start = runner.events.len();
            runner.client.cancel_prompt().await.unwrap();
            let grace = armed(&state).await;
            let (busy, observed) = oneshot::channel();
            *connection.test_prompt_admission_busy.lock().unwrap() = Some(busy);
            let storage = crate::session::Storage::open_reference("test").unwrap();
            let exclusion = storage
                .acquire_instance_lifecycle_lock(&runner.client.id)
                .unwrap();
            drop(gate);
            bounded(observed)
                .await
                .expect("captured cancel encounters physical contention");
            assert!(
                timeout(Duration::from_millis(100), async {
                    loop {
                        if runner
                            .native
                            .requests()
                            .iter()
                            .any(|method| method == "session/cancel")
                        {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .is_err(),
                "no Native cancel while its physical fence is held"
            );
            drop(exclusion);
            runner
                .event_since(start, |event| {
                    matches!(event, Event::CancelRequested { .. })
                })
                .await;
            runner.assert_cancel_grace(&grace).await;
            assert_eq!(runner.assert_owner().await, owner);
            bounded(async {
                while !runner
                    .native
                    .requests()
                    .iter()
                    .any(|method| method == "session/cancel")
                {
                    tokio::task::yield_now().await;
                }
            })
            .await;
            let stored = runner
                .store
                .replay_from(&runner.client.id, original.prefix)
                .unwrap();
            let cancels: Vec<_> = stored
                .iter()
                .filter(|stored| matches!(stored.event.as_ref(), Event::CancelRequested { .. }))
                .collect();
            assert_eq!(cancels.len(), 1);
            assert_eq!(cancels[0].metadata.scope, EventScope::OperationCurrent);
            assert_eq!(
                cancels[0].metadata.operation.unwrap().correlation,
                grace.scope.as_ref().unwrap().correlation().unwrap()
            );
            assert_eq!(
                runner
                    .native
                    .requests()
                    .iter()
                    .filter(|method| method.as_str() == "session/cancel")
                    .count(),
                1
            );
            runner.reply(&original, "end_turn").await;
            runner.stopped_since(start, "prompt_complete").await;
            runner.shutdown().await;
        })
    })
    .await;
}

#[tokio::test]
#[serial_test::serial]
async fn cancel_armed_before_its_send_survives_control_loss_mid_send() {
    let thread = std::thread::current();
    if crate::process::runner::test_support::managed_runner::child_if_requested(
        thread.name().unwrap(),
    )
    .await
    {
        return;
    }
    #[derive(Clone, Copy, Debug)]
    enum Cancel {
        Local,
        Adopted,
        SilentOrphan,
    }
    for case in [Cancel::Local, Cancel::Adopted, Cancel::SilentOrphan] {
        match case {
            Cancel::SilentOrphan => {
                let _environment = crate::session::test_support::EnvGuard::set(&[
                    ("AOE_SILENT_ORPHAN_GRACE_MS", "50"),
                    ("AOE_SILENT_ORPHAN_CHECK_INTERVAL_MS", "10"),
                ]);
                connection::test_local_silent_orphan_cancel_recovery().await;
            }
            Cancel::Local | Cancel::Adopted => {
                with_managed(|scope| Box::pin(async move {
                    let mut runner = Runner::new(scope).await;
                    let original = runner.start("actual turn before held cancel send").await;
                    if matches!(case, Cancel::Adopted) { runner = runner.adopt(scope, &original).await; }
                    let state = runner.state();
                    let gate = state.cancel_send_gate.write().await;
                    let start = runner.events.len();
                    runner.client.cancel_prompt().await.unwrap();
                    let grace = armed(&state).await;
                    runner.assert_cancel_grace(&grace).await;
                    assert!(!runner.native.requests().iter().any(|method| method == "session/cancel"),
                        "{case:?}: the held send cannot reach Native");
                    while let Ok(frame) = runner.controls.try_recv() {
                        assert!(!matches!(frame, ControlBody::Cancel { .. }), "{case:?}: no actual control cancel before gate release");
                    }
                    tokio::time::pause();
                    tokio::time::advance(Duration::from_secs(6)).await;
                    tokio::time::resume();
                    let old = runner.close_control().await;
                    drop(gate);
                    runner.recover(&old).await;
                    runner.assert_cancel_grace(&grace).await;
                    runner.client.test_flush_commands().await;
                    assert!(!runner.native.requests().iter().any(|method| method == "session/cancel"),
                        "{case:?}: a lost send is not silently reissued on the recovered attachment");
                    runner.finish_cancel(&grace, start).await;
                })).await;
            }
        }
    }
}

#[tokio::test]
#[serial_test::serial]
async fn silent_initial_adoption_releases_its_watchdog_on_control_loss() {
    with_managed(|scope| Box::pin(async move {
        let mut runner = Runner::new(scope).await;
        let original = runner.start("actual original remains silent during initial adoption").await;
        runner = runner.adopt(scope, &original).await;
        let state = runner.state();
        let turn = state.terminal_claim.current_turn();
        let start = runner.events.len();
        runner.update(json!({"sessionUpdate":"agent_message_chunk", "content":{"type":"text","text":"observed Native progress"}})).await;
        runner.event_since(start, |event| matches!(event, Event::AgentMessageChunk { text, .. } if text == "observed Native progress")).await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(30)).await;
        runner.client.test_flush_commands().await;
        tokio::time::resume();
        assert!(state.cancel_deadline.lock().unwrap().is_none(), "Native silence does not arm the local orphan watchdog");
        assert!(!state.terminal_claim.claimed());
        assert_eq!(state.terminal_claim.current_turn(), turn);
        assert!(!runner.native.requests().iter().any(|method| method == "session/cancel"));
        let old = runner.close_control().await;
        assert!(old.control_loss().unwrap().turn_in_flight);
        runner.recover(&old).await;
        assert!(!state.terminal_claim.claimed());
        runner.reply(&original, "end_turn").await;
        runner.stopped_since(start, "prompt_complete").await;
        runner.assert_original(&original, AcpOriginalRequestResult::Succeeded, EventScope::HistoryOnly);
        assert_eq!(runner.events[start..].iter().filter(|event| matches!(event, Event::Stopped { .. })).count(), 1);
        assert!(!runner.native.requests().iter().any(|method| method == "session/cancel"));
        runner.shutdown().await;
    })).await;
}

#[tokio::test]
#[serial_test::serial]
async fn recovered_completion_orders_updates_and_applies_rate_limit_parking() {
    with_managed(|scope| Box::pin(async move {
        for (report, stop_reason, reset_after, parked) in [
            ("rejected", "end_turn", 3600, false),
            ("rejected", "cancelled", 3600, false),
            ("allowed", "end_turn", 3600, false),
            ("rejected", "end_turn", -60, false),
            ("rejected", "error", 3600, true),
        ] {
            let mut runner = Runner::new(scope).await;
            let original = runner.start("original Native operation before quota recovery").await;
            let resets_at = crate::time::Timestamp::now().as_unix_secs() + reset_after;
            let usage_start = runner.events.len();
            runner.update(json!({"sessionUpdate":"usage_update","used":1,"size":2,
                "_meta":{"_claude/rateLimit":{"status":"rejected","rateLimitType":"five_hour","resetsAt":resets_at}}})).await;
            runner.event_since(usage_start, |event| matches!(event, Event::UsageUpdated { .. })).await;
            runner.barrier().await;
            let old = runner.close_control().await;
            runner.recover(&old).await;
            if report == "allowed" {
                let start = runner.events.len();
                runner.update(json!({"sessionUpdate":"usage_update","used":1,"size":2,
                    "_meta":{"_claude/rateLimit":{"status":"allowed"}}})).await;
                runner.event_since(start, |event| matches!(event, Event::UsageUpdated { .. })).await;
            }
            let start = runner.events.len();
            for index in 0..100 {
                runner.update(json!({"sessionUpdate":"agent_message_chunk",
                    "content":{"type":"text","text":format!("final-{index}")}})).await;
            }
            if stop_reason == "error" { runner.quota(&original).await; }
            else { runner.reply(&original, stop_reason).await; }
            let stop = runner.stopped_since(start,
                if parked { "rate_limited" } else if stop_reason == "cancelled" { "cancelled" } else { "prompt_complete" }).await;
            let events = &runner.events[start..=stop];
            let chunks: Vec<_> = events.iter().filter_map(|event| {
                if let Event::AgentMessageChunk { text, .. } = event { Some(text.clone()) } else { None }
            }).collect();
            assert_eq!(chunks, (0..100).map(|index| format!("final-{index}")).collect::<Vec<_>>());
            let limits: Vec<_> = events.iter().enumerate().filter_map(|(index, event)| {
                if let Event::RateLimit { info } = event { Some((index, info)) } else { None }
            }).collect();
            assert_eq!(limits.len(), usize::from(parked));
            if parked {
                assert_eq!(limits[0].1.resets_at.unwrap().as_unix_secs(), resets_at);
                assert!(limits[0].0 < events.len() - 1);
            }
            assert_eq!(events.iter().filter(|event| matches!(event, Event::Stopped { .. })).count(), 1);
            runner.assert_original(&original,
                if parked { AcpOriginalRequestResult::Failed } else { AcpOriginalRequestResult::Succeeded },
                EventScope::HistoryOnly);
            runner.barrier().await;
            assert!(runner.actual().connection_ready());
            runner.client.test_flush_commands().await;
            let follow_start = runner.events.len();
            let follow = runner.start("next explicit input uses the same actual Native").await;
            assert_ne!(follow.id(), original.id());
            assert_ne!(follow.scope.correlation(), original.scope.correlation());
            assert_ne!(follow.window, original.window);
            assert_eq!(follow.request["params"]["sessionId"], original.request["params"]["sessionId"]);
            runner.reply(&follow, "end_turn").await;
            runner.stopped_since(follow_start, "prompt_complete").await;
            runner.assert_original(&follow, AcpOriginalRequestResult::Succeeded, EventScope::OperationCurrent);
            assert!(!runner.events[follow_start..].iter().any(|event| matches!(event,
                Event::RateLimit { .. } | Event::AcpSessionAssigned { .. } | Event::SessionContextReset { .. })));
            let methods = runner.native.requests();
            assert_eq!(methods.iter().filter(|method| *method == "session/new").count(), 1);
            assert_eq!(methods.iter().filter(|method| *method == "session/prompt").count(), 2);
            assert!(!methods.iter().any(|method| matches!(method.as_str(), "session/load" | "session/fork")));
            runner.shutdown().await;
            assert!(bounded(runner.client.next_event()).await.is_none(), "explicit shutdown ends the actual observer stream");
        }
    })).await;
}
