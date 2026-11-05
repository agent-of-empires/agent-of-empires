//! Test fixtures shared by more than one submodule's tests.

use agent_client_protocol::schema::v1::SessionUpdate;

use crate::acp::agent_registry::AgentSpec;

use super::spawn::SpawnConfig;
use std::future::Future;

#[derive(Clone)]
pub(super) struct TestCommandSender(std::sync::Arc<super::AcpClient>);

impl TestCommandSender {
    pub(super) async fn send(
        &self,
        command: super::commands::ClientCmd,
    ) -> Result<(), super::AcpError> {
        let correlation = command
            .requires_operation()
            .then(|| crate::acp::establishment::NativeOperationCorrelation::new(None));
        let queued = self.0.queue_command(command, correlation).await?;
        self.0
            .cmd_tx
            .as_ref()
            .ok_or(super::AcpError::NotRunning)?
            .send(queued)
            .await
            .map_err(|_| super::AcpError::AgentExited)
    }
}

pub(crate) type SdkTestGate = (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);

pub(crate) struct TrackingObservation {
    pub(crate) tasks: std::sync::Arc<crate::acp::task_transcript::ActiveTasks>,
    pub(crate) agents: std::sync::Arc<crate::acp::background_agent::ActiveAgents>,
}

#[derive(Default)]
pub(crate) struct SdkTestOptions {
    pub(crate) command_capacity: Option<usize>,
    pub(crate) select_probe: Option<std::cell::RefCell<super::connection::TestSelectProbe>>,
    pub(crate) queue:
        Option<tokio::sync::oneshot::Sender<crate::acp::event_sender::WeakEventSender>>,
    pub(crate) tracking: Option<tokio::sync::oneshot::Sender<TrackingObservation>>,
    pub(crate) callback: Option<SdkTestGate>,
    pub(crate) reverse_send: Option<SdkTestGate>,
    pub(crate) default_publish: Option<SdkTestGate>,
    pub(crate) fresh_write_pending: Option<tokio::sync::oneshot::Sender<()>>,
    pub(crate) bootstrap_fence: Option<SdkTestGate>,
    pub(crate) initialize_response_application: Option<SdkTestGate>,
    pub(crate) fresh_response_application: Option<SdkTestGate>,
    pub(crate) establishment_usable: Option<SdkTestGate>,
    pub(crate) prompt_completed: Option<SdkTestGate>,
    pub(crate) control_output:
        Option<tokio::sync::mpsc::UnboundedSender<crate::acp::control_protocol::ControlBody>>,
    pub(crate) native_session_seen: Option<tokio::sync::oneshot::Sender<()>>,
    pub(crate) update: Option<SdkTestGate>,
}

tokio::task_local! {
    pub(super) static SDK_TEST_OPTIONS: std::sync::Mutex<Option<SdkTestOptions>>;
}

/// Observes the actual SDK task without transferring custody.
pub(crate) struct SdkTaskObservation(std::sync::Arc<super::AcpClient>);

impl SdkTaskObservation {
    pub(crate) fn is_finished(&self) -> bool {
        self.0
            .connection_task
            .lock()
            .unwrap()
            .as_ref()
            .is_none_or(|task| task.is_finished())
    }
}

impl Future for SdkTaskObservation {
    type Output = Result<(), tokio::task::JoinError>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let mut slot = self.0.connection_task.lock().unwrap();
        let Some(task) = slot.as_mut() else {
            return std::task::Poll::Ready(Ok(()));
        };
        let result = std::pin::Pin::new(task).poll(cx);
        if result.is_ready() {
            slot.take();
        }
        result
    }
}

impl super::AcpClient {
    pub(crate) fn selected_native_session(&self) -> String {
        self.establishment_projection()
            .and_then(|projection| projection.applied())
            .and_then(|postimage| postimage.choice.acp_session_id.clone())
            .expect("the genuine Applied original selects its Native session")
    }

    pub(super) fn test_control(&self) -> std::sync::Arc<super::control::DaemonControlClient> {
        self.connection_state
            .control
            .get()
            .and_then(std::sync::Weak::upgrade)
            .expect("the same actual SDK runner connection")
    }

    pub(super) fn gate_next_update(&self, gate: SdkTestGate) {
        assert!(self
            .connection_state
            .test_update
            .lock()
            .unwrap()
            .replace(gate)
            .is_none());
    }

    pub(crate) async fn with_sdk_test_options<F: Future>(
        options: SdkTestOptions,
        future: F,
    ) -> F::Output {
        SDK_TEST_OPTIONS
            .scope(std::sync::Mutex::new(Some(options)), future)
            .await
    }

    pub(super) fn observe_sdk_output(
        &self,
        sender: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
    ) {
        *self.connection_state.test_sdk_output.lock().unwrap() = Some(sender);
    }

    pub(super) async fn inject_sdk_line(&self, line: &str) {
        use tokio::io::AsyncWriteExt;
        let mut writer = self
            .connection_state
            .test_shim
            .get()
            .expect("real SDK shim")
            .lock()
            .await;
        writer.write_all(line.as_bytes()).await.unwrap();
        writer.write_all(b"\n").await.unwrap();
        writer.flush().await.unwrap();
    }

    pub(crate) async fn spawn_with_test_options(
        config: SpawnConfig,
        session_id: crate::acp::state::AcpSessionId,
        executable: std::path::PathBuf,
        options: SdkTestOptions,
    ) -> Result<Self, super::AcpError> {
        SDK_TEST_OPTIONS
            .scope(
                std::sync::Mutex::new(Some(options)),
                Self::spawn_with_test_runner(config, session_id, executable),
            )
            .await
    }

    pub(crate) fn observe_sdk_task(self: &std::sync::Arc<Self>) -> SdkTaskObservation {
        SdkTaskObservation(self.clone())
    }

    pub(super) fn test_command_sender(self: &std::sync::Arc<Self>) -> TestCommandSender {
        TestCommandSender(self.clone())
    }

    pub(super) fn test_pending_responders(&self) -> super::pending::PendingResponders {
        self.pending_responders.clone()
    }

    pub(super) fn test_connection_state(
        &self,
    ) -> std::sync::Arc<super::connection::ConnectionState> {
        self.connection_state.clone()
    }

    pub(super) fn test_event_queue(&self) -> crate::acp::event_sender::WeakEventSender {
        self.connection_state
            .test_queue
            .get()
            .expect("actual SDK queue observation")
            .clone()
    }
}

pub(super) fn text_chunk(text: &str, id: Option<&str>) -> SessionUpdate {
    use agent_client_protocol::schema::v1::{ContentBlock, ContentChunk, TextContent};
    let mut chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
    if let Some(id) = id {
        chunk = chunk.message_id(id);
    }
    SessionUpdate::AgentMessageChunk(chunk)
}

/// A `compaction_update` as claude-agent-acp sends it; `extra` adds fields.
pub(super) fn compaction_update(id: &str, status: &str, extra: serde_json::Value) -> SessionUpdate {
    let mut v = serde_json::json!({
        "sessionUpdate": "compaction_update",
        "compactionId": id,
        "status": status,
    });
    v.as_object_mut()
        .unwrap()
        .extend(extra.as_object().cloned().unwrap_or_default());
    serde_json::from_value(v).expect("compaction_update")
}

pub(super) fn compaction_chunk(id: &str, text: &str) -> SessionUpdate {
    serde_json::from_value(serde_json::json!({
        "sessionUpdate": "compaction_summary_chunk",
        "compactionId": id,
        "content": {"type": "text", "text": text},
    }))
    .expect("compaction_summary_chunk")
}

/// Build a minimal host (non-sandboxed) `SpawnConfig` for env tests.
pub(super) fn env_test_spawn_config(cwd: std::path::PathBuf) -> SpawnConfig {
    SpawnConfig {
        launch_lease: None,
        provider_routing: Vec::new(),
        wrapper_substitution: None,
        agent_key: "claude".into(),
        tool: "claude".into(),
        spec: AgentSpec {
            command: "claude-agent-acp".into(),
            args: vec![],
            description: "test".into(),
            env_allowlist: None,
        },
        cwd,
        additional_dirs: vec![],
        provider_env: vec![],
        host_environment: vec![],
        default_effort: None,
        default_effort_explicit: false,
        default_mode: None,
        default_model: None,
        socket_path: None,
        stored_acp_session_id: None,
        fork_from: None,
        seed_history_replay: false,
        prior_conversation: false,
        generation: 0,
        artifact_dir: None,
        sandbox_info: None,
        source_profile: None,
        mcp_servers: Vec::new(),
        claude_store_pin: None,
        base_host_environment: vec![],
    }
}

/// Spawn config for a `/bin/sh` fixture script.
///
/// The script is handed to `sh` as an argument rather than exec'd
/// directly. `execve` on a file any process still holds open for writing
/// fails with `ETXTBSY`, and a concurrent spawn elsewhere in the test
/// binary can fork between the fixture writer's `open` and its `close`,
/// leaving the child that writable descriptor until it execs. `sh` only
/// ever opens the script for reading, which removes the window instead of
/// retrying past it (#3790). Fixture writers therefore need no exec bit.
#[cfg(unix)]
pub(super) fn reset_fake_spawn_config(
    script: &std::path::Path,
    cwd: &std::path::Path,
) -> SpawnConfig {
    SpawnConfig {
        launch_lease: None,
        provider_routing: Vec::new(),
        wrapper_substitution: None,
        agent_key: "codex".into(),
        tool: "codex".into(),
        spec: AgentSpec {
            command: "/bin/sh".into(),
            args: vec![script.to_string_lossy().into_owned()],
            description: "scripted reset fake".into(),
            env_allowlist: None,
        },
        cwd: cwd.to_path_buf(),
        additional_dirs: vec![],
        provider_env: vec![],
        host_environment: vec![],
        default_effort: None,
        default_effort_explicit: false,
        default_mode: None,
        default_model: None,
        socket_path: None,
        stored_acp_session_id: None,
        fork_from: None,
        seed_history_replay: false,
        prior_conversation: false,
        generation: 0,
        artifact_dir: None,
        sandbox_info: None,
        source_profile: None,
        mcp_servers: Vec::new(),
        claude_store_pin: None,
        base_host_environment: vec![],
    }
}

#[cfg(unix)]
async fn original_decision_snapshot(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    nonce: &crate::acp::approvals::Nonce,
    ready: bool,
    case: (bool, bool, bool, bool),
) -> serde_json::Value {
    use futures_util::StreamExt;
    let mut last_snapshot = None;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let message = socket
                .next()
                .await
                .expect("the canonical websocket must not close")
                .expect("the canonical websocket remains valid");
            if let tokio_tungstenite::tungstenite::Message::Text(text) = message {
                let frame: serde_json::Value = serde_json::from_str(&text).unwrap();
                if frame["kind"] != "reduced_state" {
                    continue;
                }
                if !ready {
                    assert_eq!(
                        frame["state"]["connection_ready"], false,
                        "Original cards must not grant main-input readiness before Applied"
                    );
                }
                if frame["state"]["connection_ready"] == ready
                    && frame["state"]["original_decision_nonces"]
                        .as_array()
                        .is_some_and(|nonces| nonces.contains(&serde_json::json!(nonce)))
                {
                    return frame;
                }
                last_snapshot = Some(frame);
            }
        }
    })
    .await
    .unwrap_or_else(|error| panic!(
        "Original capability missing: ready={ready}, case(load,permission,result_before_decision,retire_pending)={case:?}, last_snapshot={last_snapshot:?}: {error}"
    ))
}

#[cfg(unix)]
async fn resolve_original_decision_http(
    state: std::sync::Arc<crate::server::AppState>,
    id: &str,
    nonce: &crate::acp::approvals::Nonce,
    permission: bool,
) -> axum::response::Response {
    use axum::extract::{Path, State};
    use axum::response::IntoResponse;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        if permission {
            crate::server::api::resolve_approval(
                State(state),
                Path((id.to_owned(), nonce.0.clone())),
                Ok(axum::Json(crate::acp::protocol::ResolveApprovalRequest {
                    decision: crate::acp::protocol::ApprovalDecisionWire::Allow,
                    option_id: Some("allow".into()),
                })),
            )
            .await
            .into_response()
        } else {
            crate::server::api::resolve_elicitation(
                State(state),
                Path((id.to_owned(), nonce.0.clone())),
                Ok(axum::Json(
                    crate::acp::elicitations::ElicitationResolution::Decline,
                )),
            )
            .await
            .into_response()
        }
    })
    .await
    .expect("answering an Original nonce must not await worker readiness or its Native result")
}

/// Original decisions belong to their captured constructor, not to a future
/// Current session stamp. Exercise the real Native peer and HTTP responder.
#[cfg(unix)]
#[tokio::test]
async fn original_human_decisions_cross_pending_and_applied_without_current_authority() {
    use crate::acp::event_sender::EventScope;
    use crate::acp::state::Event;
    use axum::extract::{Path, State};
    use axum::response::IntoResponse;
    use std::time::Duration;

    crate::acp::supervisor::test_support::managed::with_managed(|scope| {
        Box::pin(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let router = axum::Router::new()
                .route("/{id}", axum::routing::get(crate::server::acp_ws::acp_ws))
                .with_state(scope.state.clone());
            let server = scope.spawn_helper(async move {
                axum::serve(listener, router).await.unwrap();
            });
            for (load, permission, result_before_decision, retire_pending, contend_consumption) in [
                (false, true, false, false, false), (false, true, true, false, false),
                (false, false, false, false, false), (false, false, true, false, false),
                (true, true, false, false, false), (true, true, true, false, false),
                (true, false, false, false, false), (true, false, true, false, false),
                (false, true, true, true, false), (true, false, true, true, false),
                (false, true, true, false, true), (true, false, false, false, true),
            ] {
                        let directory = scope.tempdir();
                        let selected = if load {
                            let selected = uuid::Uuid::new_v4().to_string();
                            let native_selected = selected.clone();
                            let (config, _peer) = scope.native_peer(
                                directory.path().into(), "gemini", "gemini", move |request| {
                                    let id = request.get("id")?;
                                    let result = match request["method"].as_str()? {
                                        "initialize" => serde_json::json!({
                                            "protocolVersion": 1, "agentCapabilities": {"loadSession": true}
                                        }),
                                        "session/new" => serde_json::json!({"sessionId": native_selected}),
                                        "session/cancel" => return None,
                                        method => panic!("unexpected priming Native request: {method}"),
                                    };
                                    Some(serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}))
                                },
                            );
                            let original = scope.spawn(config, "original-human-prime").await.unwrap();
                            original.ready().await;
                            assert_eq!(original.selected_native_session(), selected);
                            Some(original)
                        } else {
                            None
                        };
                        let native_sid = selected.as_ref().map(|client| client.selected_native_session())
                            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                        let expected_method = if load { "session/load" } else { "session/new" };
                        let (entered, mut lifecycle) = tokio::sync::mpsc::unbounded_channel();
                        let (config, mut peer) = scope.native_peer(
                            directory.path().into(), "gemini", "gemini", move |request| {
                                let id = request.get("id")?;
                                match request["method"].as_str()? {
                                    "initialize" => Some(serde_json::json!({
                                        "jsonrpc":"2.0","id":id,"result":{
                                            "protocolVersion":1,"agentCapabilities":{"loadSession":true}
                                        }
                                    })),
                                    method if method == expected_method => {
                                        entered.send(request.clone()).unwrap();
                                        None
                                    }
                                    "session/prompt" => Some(serde_json::json!({
                                        "jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}
                                    })),
                                    "session/cancel" => None,
                                    method => panic!("unexpected Original Native request: {method}"),
                                }
                            },
                        );
                        let mut client = if let Some(original) = selected {
                            scope.retained_after(original, config, "original-human-load").await
                        } else {
                            scope.spawn(config, "original-human-new").await.unwrap()
                        };
                        let actual = client.actual();
                        let owner = client.connected_runner().expect("the actual constructor's physical owner");
                        let request = tokio::time::timeout(Duration::from_secs(10), lifecycle.recv())
                            .await.expect("the genuine lifecycle request reaches Native")
                            .expect("the lifecycle observer must not close");
                        if load {
                            assert_eq!(request["params"]["sessionId"], native_sid);
                        }
                        let projection = actual.establishment_projection().unwrap();
                        assert!(projection.applied().is_none(), "the held Native result cannot already be Applied");
                        assert!(client.send_prompt("Pending is not main-input authority", &[]).await.is_err());
                        let (mut socket, _) = tokio_tungstenite::connect_async(
                            format!("ws://{address}/{}?frames=0", client.id),
                        ).await.expect("the real canonical websocket connects before the Original callback");
                        let (method, params) = if permission {
                            ("session/request_permission", serde_json::json!({
                                "sessionId":native_sid,
                                "toolCall":{"toolCallId":"original-human-tool","title":"Original needs a human"},
                                "options":[{"optionId":"allow","name":"Allow","kind":"allow_once"}]
                            }))
                        } else {
                            ("elicitation/create", serde_json::json!({
                                "mode":"form","sessionId":native_sid,"message":"Original needs a form",
                                "requestedSchema":{"type":"object","properties":{"answer":{"type":"string"}}}
                            }))
                        };
                        if load {
                            let mut foreign = params.clone();
                            foreign["sessionId"] = serde_json::json!(uuid::Uuid::new_v4().to_string());
                            peer.send(serde_json::json!({
                                "jsonrpc":"2.0","id":"foreign-original-human","method":method,"params":foreign
                            })).await;
                            let denied = tokio::time::timeout(Duration::from_secs(10), peer.reply())
                                .await.expect("a foreign known-Load SID fails without human or lifecycle progress");
                            assert_eq!(denied["id"], "foreign-original-human");
                            assert_eq!(denied["error"]["code"], -32602);
                            assert!(actual.test_pending_responders().lock().await.is_empty());
                        }
                        peer.send(serde_json::json!({
                            "jsonrpc":"2.0","id":"original-human","method":method,"params":params
                        })).await;
                        let nonce = tokio::time::timeout(Duration::from_secs(10), async {
                            loop {
                                match client.next_event().await.expect("the Original decision observer must not close") {
                                    Event::ApprovalRequested { approval } if permission => break approval.nonce,
                                    Event::ElicitationRequested { elicitation } if !permission => break elicitation.nonce,
                                    Event::AgentStartupError { message, .. } => panic!("{message}"),
                                    Event::AcpConnectionUnavailable { .. } => panic!("the Original Native peer must remain live"),
                                    _ => {}
                                }
                            }
                        }).await.unwrap_or_else(|error| panic!(
                            "Original decision missing before Native result: load={load}, permission={permission}, result_before_decision={result_before_decision}, retire_pending={retire_pending}: {error}"
                        ));
                        let stored = scope.state.acp_event_store.replay_from(&client.id, 0).unwrap();
                        let published = stored.iter().find(|stored| match stored.event.as_ref() {
                            Event::ApprovalRequested { approval } => approval.nonce == nonce,
                            Event::ElicitationRequested { elicitation } => elicitation.nonce == nonce,
                            _ => false,
                        }).expect("the request has a real committed journal position");
                        assert_eq!(published.metadata.scope, EventScope::HistoryOnly,
                            "Original custody must not masquerade as Current");
                        assert!(published.metadata.native_origin.is_some());
                        let cache = scope.state.acp_event_store.control_cache().clone();
                        assert!(cache.original_decisions(&client.id, published.seq - 1).await.unwrap()
                            .nonces.is_empty(), "a future publication cannot grant an earlier snapshot capability");
                        let case = (load, permission, result_before_decision, retire_pending);
                        let snapshot = original_decision_snapshot(&mut socket, &nonce, false, case).await;
                        assert!(snapshot["seq"].as_u64().unwrap() >= published.seq);
                        let cards = if permission { "pending_approvals" } else { "pending_elicitations" };
                        assert!(snapshot["state"][cards].as_array().unwrap().iter()
                            .any(|card| card["nonce"] == serde_json::json!(nonce)),
                            "the actual Pending Original card and its capability share a canonical snapshot");
                        {
                            let responders = actual.test_pending_responders();
                            let responders = responders.lock().await;
                            let publication = responders.get(&nonce).unwrap().publication.as_ref().unwrap();
                            assert!(publication.is_original());
                            assert_eq!(publication.metadata().scope, EventScope::HistoryOnly);
                        }
                        let result = serde_json::json!({
                            "jsonrpc":"2.0","id":request["id"],
                            "result":if load { serde_json::json!({}) } else { serde_json::json!({"sessionId":native_sid}) }
                        });
                        if result_before_decision {
                            peer.send(result.clone()).await;
                            client.ready().await;
                            assert_eq!(client.selected_native_session(), native_sid);
                            let frontier = scope.state.acp_event_store.replay_from(&client.id, 0).unwrap()
                                .last().expect("the applied Original has committed history").seq;
                            assert_eq!(cache.original_decisions(&client.id, frontier).await.unwrap().nonces,
                                vec![nonce.clone()], "the same Applied Original retains its own pending decision");
                            original_decision_snapshot(&mut socket, &nonce, true, case).await;
                        }
                        if retire_pending {
                            assert!(result_before_decision);
                            client.stop_session().await;
                            let denied = resolve_original_decision_http(
                                scope.state.clone(), &client.id, &nonce, permission,
                            ).await;
                            let status = denied.status();
                            let body = axum::body::to_bytes(denied.into_body(), usize::MAX).await.unwrap();
                            assert!(matches!(status, axum::http::StatusCode::NOT_FOUND | axum::http::StatusCode::CONFLICT),
                                "Stop must revoke the still-unanswered Original nonce: {status}: {}",
                                String::from_utf8_lossy(&body));
                            let frontier = scope.state.acp_event_store.highest_seq(&client.id);
                            assert!(cache.original_decisions(&client.id, frontier).await.unwrap().nonces.is_empty());
                            assert!(!peer.requests().iter().any(|method| method == "session/prompt"));
                            assert_eq!(peer.requests().iter().filter(|method| method.as_str() == expected_method).count(), 1);
                            drop(socket);
                            let retained_sid = native_sid.clone();
                            let (config, replacement_peer) = scope.native_peer(
                                directory.path().into(), "gemini", "gemini", move |request| {
                                    let id = request.get("id")?;
                                    let result = match request["method"].as_str()? {
                                        "initialize" => serde_json::json!({
                                            "protocolVersion":1,"agentCapabilities":{"loadSession":true}
                                        }),
                                        "session/load" => {
                                            assert_eq!(request["params"]["sessionId"], retained_sid);
                                            serde_json::json!({})
                                        }
                                        "session/cancel" => return None,
                                        method => panic!("revoked HTTP nonce must not issue replacement input: {method}"),
                                    };
                                    Some(serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}))
                                },
                            );
                            let replacement = scope.resume_stopped(&client.id, config).await;
                            replacement.ready().await;
                            assert_eq!(replacement.selected_native_session(), native_sid);
                            assert_ne!(replacement.connected_runner(), Some(owner));
                            let denied = resolve_original_decision_http(
                                scope.state.clone(), &replacement.id, &nonce, permission,
                            ).await;
                            let status = denied.status();
                            let body = axum::body::to_bytes(denied.into_body(), usize::MAX).await.unwrap();
                            assert!(matches!(status, axum::http::StatusCode::NOT_FOUND | axum::http::StatusCode::CONFLICT),
                                "a genuine replacement cannot inherit an old Original nonce: {status}: {}",
                                String::from_utf8_lossy(&body));
                            assert_eq!(replacement_peer.requests().iter()
                                .filter(|method| method.as_str() == "session/load").count(), 1);
                            assert!(!replacement_peer.requests().iter()
                                .any(|method| method == "session/new" || method == "session/prompt"));
                            replacement.stop_and_forget().await;
                            scope.finish_conversation_case().await;
                            continue;
                        }
                        if !permission {
                            let responders = actual.test_pending_responders();
                            let publication = responders.lock().await.get(&nonce).unwrap()
                                .publication.as_ref().unwrap().clone();
                            let invalid = tokio::time::timeout(Duration::from_secs(10),
                                crate::server::api::resolve_elicitation(
                                    State(scope.state.clone()), Path((client.id.clone(), nonce.0.clone())),
                                    Ok(axum::Json(crate::acp::elicitations::ElicitationResolution::Accept {
                                        answers: [("not_declared".into(),
                                            crate::acp::elicitations::AnswerValue::Text("invalid".into()))]
                                            .into_iter().collect(),
                                    })),
                                ),
                            ).await.expect("invalid Original form validation must not await Native readiness")
                                .into_response();
                            let status = invalid.status();
                            let body = axum::body::to_bytes(invalid.into_body(), usize::MAX).await.unwrap();
                            assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                                "invalid form answers must preserve the exact responder: {}", String::from_utf8_lossy(&body));
                            let pending = responders.lock().await;
                            assert!(std::sync::Arc::ptr_eq(&publication,
                                pending.get(&nonce).unwrap().publication.as_ref().unwrap()));
                        }
                        if contend_consumption {
                            let attempt = projection.attempt().unwrap();
                            let storage = crate::session::Storage::open_reference(&attempt.profile).unwrap();
                            let lifecycle = storage.acquire_instance_lifecycle_lock(&client.id).unwrap();
                            let responders = actual.test_pending_responders();
                            let publication = responders.try_lock().unwrap().get(&nonce).unwrap()
                                .publication.as_ref().unwrap().clone();
                            let resolve = async {
                                if permission {
                                    super::pending::resolve_approval(&responders, &nonce,
                                        super::pending::ApprovalResolutionMessage::Decision {
                                            decision: crate::acp::approvals::ApprovalDecision::Allow,
                                            option_id: Some("allow".into()),
                                        }).await
                                } else {
                                    super::pending::resolve_elicitation(&responders, &nonce,
                                        crate::acp::elicitations::ElicitationResolution::Decline).await
                                }
                            };
                            tokio::pin!(resolve);
                            assert!(futures_util::poll!(resolve.as_mut()).is_pending(),
                                "benign lifecycle contention must wait, not refuse the Original answer");
                            {
                                let pending = responders.try_lock()
                                    .expect("guard waits must release the responder map");
                                assert!(std::sync::Arc::ptr_eq(&publication,
                                    pending.get(&nonce).unwrap().publication.as_ref().unwrap()));
                            }
                            drop(lifecycle);
                            tokio::time::timeout(Duration::from_secs(10), resolve).await
                                .expect("the same Original answer resumes after exclusion").unwrap();
                        } else {
                            let response = resolve_original_decision_http(
                                scope.state.clone(), &client.id, &nonce, permission,
                            ).await;
                            let status = response.status();
                            let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
                            assert_eq!(status, axum::http::StatusCode::NO_CONTENT,
                                "the exact Original HTTP responder must accept its human answer: {}",
                                String::from_utf8_lossy(&body));
                        }
                        let reply = tokio::time::timeout(Duration::from_secs(10), peer.reply())
                            .await.expect("the human answer reaches the same physical Native");
                        assert_eq!(reply["id"], "original-human");
                        if permission {
                            assert_eq!(reply["result"]["outcome"]["optionId"], "allow");
                        } else {
                            assert_eq!(reply["result"]["action"], "decline");
                        }
                        if !result_before_decision {
                            assert!(projection.applied().is_none(),
                                "answering an Original request does not select a Current identity");
                            assert!(client.send_prompt("answering is not main-input authority", &[]).await.is_err());
                            peer.send(result).await;
                            client.ready().await;
                        }
                        assert_eq!(client.selected_native_session(), native_sid);
                        assert_eq!(client.connected_runner(), Some(owner));
                        let frontier = scope.state.acp_event_store.replay_from(&client.id, 0).unwrap()
                            .last().expect("the resolved Original has committed history").seq;
                        assert!(cache.original_decisions(&client.id, frontier).await.unwrap().nonces.is_empty());
                        client.send_prompt("the one explicit next input", &[]).await.unwrap();
                        tokio::time::timeout(Duration::from_secs(10), async {
                            loop {
                                match client.next_event().await.expect("the same Native remains observable") {
                                    Event::Stopped { .. } => break,
                                    Event::AgentStartupError { message, .. } => panic!("{message}"),
                                    _ => {}
                                }
                            }
                        }).await.expect("the explicit next input completes");
                        let methods = peer.requests();
                        assert_eq!(methods.iter().filter(|method| method.as_str() == expected_method).count(), 1);
                        assert_eq!(methods.iter().filter(|method| method.as_str() == "session/prompt").count(), 1);
                        assert!(!methods.iter().any(|method| {
                            ["session/new", "session/load", "session/fork"].contains(&method.as_str())
                                && method != expected_method
                        }), "HTTP decisions must not start another lifecycle RPC");
                        drop(socket);
                        client.stop_and_forget().await;
                        scope.finish_conversation_case().await;
            }
            server.abort();
            let _ = server.await;
        })
    }).await;
}

#[cfg(unix)]
#[tokio::test]
async fn worker_readiness_waits_for_actual_usable_after_applied() {
    use crate::acp::state::Event;
    use std::time::Duration;

    crate::acp::supervisor::test_support::managed::with_managed(|scope| {
        Box::pin(async move {
            let directory = scope.tempdir();
            let (gate, release, entered) = scope.gate();
            scope.configure_sdk(SdkTestOptions {
                establishment_usable: Some(gate),
                ..Default::default()
            });
            let native_sid = uuid::Uuid::new_v4().to_string();
            let mut new_count = 0;
            let (config, peer) = scope.native_peer(
                directory.path().into(),
                "gemini",
                "gemini",
                move |request| {
                    let id = request.get("id")?;
                    let result = match request["method"].as_str()? {
                        "initialize" => serde_json::json!({
                            "protocolVersion": 1, "agentCapabilities": {}
                        }),
                        "session/new" => {
                            new_count += 1;
                            serde_json::json!({"sessionId": format!("{native_sid}-{new_count}")})
                        }
                        "session/prompt" => serde_json::json!({"stopReason": "end_turn"}),
                        "session/cancel" => return None,
                        method => panic!("unexpected Native readiness request: {method}"),
                    };
                    Some(serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}))
                },
            );
            let mut client = scope.spawn(config, "applied-before-usable").await.unwrap();
            tokio::time::timeout(Duration::from_secs(10), entered)
                .await
                .expect("the actual SDK reaches Applied before usability")
                .unwrap();
            let actual = client.actual();
            let applied = actual
                .establishment_projection()
                .unwrap()
                .applied()
                .unwrap();
            assert!(!actual.connection_ready());
            assert!(!peer
                .requests()
                .iter()
                .any(|method| method == "session/prompt"));
            let supervisor = scope.state.acp_supervisor.clone();
            let id = client.id.clone();
            let delayed_ready = supervisor.wait_until_ready(&id);
            tokio::pin!(delayed_ready);
            assert!(futures_util::poll!(delayed_ready.as_mut()).is_pending());
            {
                let ready = scope.state.acp_supervisor.wait_until_ready(&client.id);
                tokio::pin!(ready);
                assert!(
                    futures_util::poll!(ready.as_mut()).is_pending(),
                    "Applied alone cannot grant usability or cause a transient readiness failure"
                );
                assert!(release.release());
                tokio::time::timeout(Duration::from_secs(10), ready)
                    .await
                    .expect("actual usability wakes the same readiness waiter")
                    .unwrap();
            }
            assert!(actual.connection_ready());
            assert_eq!(
                actual
                    .establishment_projection()
                    .unwrap()
                    .applied()
                    .as_deref(),
                Some(applied.as_ref())
            );
            client
                .send_prompt("one explicit input after actual readiness", &[])
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(10), async {
                while let Some(event) = client.next_event().await {
                    if matches!(event, Event::Stopped { .. }) {
                        return;
                    }
                }
                panic!("the actual ready Native must complete its explicit input");
            })
            .await
            .unwrap();
            assert_eq!(
                peer.requests()
                    .iter()
                    .filter(|method| method.as_str() == "session/prompt")
                    .count(),
                1
            );
            supervisor
                .reset_session_context(&id, "/clear", None, false)
                .await
                .unwrap();
            assert!(matches!(
                tokio::time::timeout(Duration::from_secs(10), delayed_ready)
                    .await
                    .expect("the original readiness waiter settles after Fresh"),
                Err(crate::acp::supervisor::SupervisorError::Superseded(stale)) if stale == id
            ));
            let requests = peer.requests();
            assert_eq!(
                requests
                    .iter()
                    .filter(|method| method.as_str() == "initialize")
                    .count(),
                1
            );
            assert_eq!(
                requests
                    .iter()
                    .filter(|method| method.as_str() == "session/new")
                    .count(),
                2
            );
            assert!(!requests.iter().any(|method| method == "session/load"));
            client.stop_and_forget().await;
        })
    })
    .await;
}
