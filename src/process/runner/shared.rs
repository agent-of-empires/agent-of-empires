//! State shared by the control accept loop and the agent stdout fanout: the bounded
//! daemon-bound queue, request correlation, and the runner-owned ACP handshake.

use super::jsonrpc::{
    parse_agent_call_outcome, parse_notification, parse_request_value_id, parse_response,
    parse_response_id,
};
use crate::acp::control_protocol::{self, ControlBody, SessionReplayed};
use crate::process::worker_registry;
use agent_client_protocol::JsonRpcMessage;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

pub(super) struct RunnerShared {
    pub(super) prompt_requests: Mutex<HashSet<i64>>,
    pub(super) control: Mutex<ControlChannel>,
    pub(super) control_wake: tokio::sync::Notify,
    pub(super) control_space: tokio::sync::Notify,
    /// Connection-bound frames and correlations are purged by this id on disconnect.
    pub(super) next_attachment_id: AtomicU64,
    pub(super) handshake: Mutex<RunnerHandshake>,
    /// Serializes handshake round trips without blocking stdout fanout.
    pub(super) handshake_gate: Mutex<()>,
    pub(super) next_req_id: AtomicI64,
    pub(super) pending_client_responses:
        Mutex<HashMap<i64, tokio::sync::oneshot::Sender<HandshakeResponse>>>,
    pub(super) pending_server_calls: Mutex<HashMap<u64, PendingServerCall>>,
    pub(super) next_call_id: AtomicU64,
    pub(super) pending_agent_calls: Mutex<HashMap<i64, PendingAgentCall>>,
    /// Sent resets remain authoritative until their response is committed.
    pub(super) pending_resets: AtomicUsize,
    pub(super) reset_finished: tokio::sync::Notify,
    pub(super) registry_owner: Option<(String, u32)>,
    /// A durability failure makes further runner state unsafe to expose.
    pub(super) fatal: AtomicBool,
    pub(super) fatal_wake: tokio::sync::Notify,
}

pub(super) struct PendingServerCall {
    pub(super) agent_id: serde_json::Value,
    pub(super) method: String,
    pub(super) attachment_id: u64,
}

pub(super) struct PendingAgentCall {
    pub(super) call_id: u64,
    pub(super) attachment_id: u64,
    pub(super) method: String,
}

/// A handshake response whose following stdout dispatch waits for publication.
pub(super) struct HandshakeResponse {
    pub(super) value: serde_json::Value,
    pub(super) release: tokio::sync::oneshot::Sender<()>,
}

#[derive(Default)]
pub(super) struct RunnerHandshake {
    pub(super) initialized: Option<serde_json::Value>,
    pub(super) session: Option<(String, serde_json::Value)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeliveryScope {
    Persistent,
    Attachment(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum QueuedKind {
    Notify,
    PromptStarted,
    PromptCompleted,
    Handshake,
    ServerCall,
    AgentReply,
    // Not `Notify`: shedding it would leave the daemon's session/load waiting.
    SessionReplayed,
}

pub(super) struct QueuedControl {
    pub(super) id: u64,
    pub(super) scope: DeliveryScope,
    pub(super) kind: QueuedKind,
    pub(super) wire: Arc<[u8]>,
}

#[derive(Default)]
pub(super) struct ControlChannel {
    pub(super) queue: VecDeque<QueuedControl>,
    pub(super) queued_bytes: usize,
    pub(super) active_attachment: Option<u64>,
    pub(super) session_announced: bool,
    pub(super) next_entry_id: u64,
    pub(super) in_flight: Option<u64>,
    pub(super) last_prompt_completion: Option<Arc<[u8]>>,
}

impl ControlChannel {
    pub(super) fn remove_at(&mut self, index: usize) -> QueuedControl {
        let frame = self.queue.remove(index).expect("queue index exists");
        self.queued_bytes = self.queued_bytes.saturating_sub(frame.wire.len());
        frame
    }

    pub(super) fn make_room(&mut self, incoming_bytes: usize) -> bool {
        while self.queue.len() >= control_protocol::MAX_CONTROL_QUEUE_FRAMES
            || self.queued_bytes.saturating_add(incoming_bytes)
                > control_protocol::MAX_CONTROL_QUEUE_BYTES
        {
            let Some(index) = self.queue.iter().position(|frame| {
                frame.kind == QueuedKind::Notify && self.in_flight != Some(frame.id)
            }) else {
                return false;
            };
            self.remove_at(index);
        }
        true
    }

    pub(super) fn push(&mut self, scope: DeliveryScope, kind: QueuedKind, wire: Arc<[u8]>) {
        let id = self.next_entry_id;
        self.next_entry_id = self.next_entry_id.wrapping_add(1);
        self.queued_bytes += wire.len();
        self.queue.push_back(QueuedControl {
            id,
            scope,
            kind,
            wire,
        });
    }

    pub(super) fn purge_attachment(&mut self, attachment_id: u64) {
        let mut index = 0;
        while index < self.queue.len() {
            if self.queue[index].scope == DeliveryScope::Attachment(attachment_id) {
                self.remove_at(index);
            } else {
                index += 1;
            }
        }
        if self.active_attachment == Some(attachment_id) {
            self.active_attachment = None;
        }
        self.in_flight = None;
    }
}

/// Gets a semantic `cancelled` outcome on disconnect; other methods get a JSON-RPC error.
pub(super) const PERMISSION_METHOD: &str = "session/request_permission";

pub(super) fn disconnected_server_call_outcome(
    method: &str,
) -> Result<serde_json::Value, control_protocol::JsonRpcError> {
    if method == PERMISSION_METHOD {
        Ok(serde_json::json!({ "outcome": { "outcome": "cancelled" } }))
    } else {
        Err(control_protocol::JsonRpcError::new(
            control_protocol::DAEMON_GONE,
            "daemon disconnected; request cancelled",
        ))
    }
}

pub(super) const PROMPT_METHOD: &str = "session/prompt";

pub(super) const RUNNER_REQUEST_ID_BASE: i64 = 1 << 48;

pub(super) const CONTROL_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) const CONTROL_ATTACH_TIMEOUT: Duration = Duration::from_secs(2);

pub(super) async fn write_control_frame(
    out: &mut tokio::net::unix::OwnedWriteHalf,
    body: &ControlBody,
) -> bool {
    matches!(
        tokio::time::timeout(
            CONTROL_WRITE_TIMEOUT,
            control_protocol::write_frame(out, body)
        )
        .await,
        Ok(Ok(()))
    )
}

/// At the cap new requests are refused rather than evicting one the agent is parked on.
pub(super) const MAX_OUTSTANDING_REQUESTS: usize = 1024;

impl RunnerShared {
    pub(super) fn new(registry_owner: Option<(String, u32)>) -> Self {
        Self {
            prompt_requests: Mutex::new(HashSet::new()),
            control: Mutex::new(ControlChannel::default()),
            control_wake: tokio::sync::Notify::new(),
            control_space: tokio::sync::Notify::new(),
            next_attachment_id: AtomicU64::new(1),
            handshake: Mutex::new(RunnerHandshake::default()),
            handshake_gate: Mutex::new(()),
            next_req_id: AtomicI64::new(RUNNER_REQUEST_ID_BASE),
            pending_client_responses: Mutex::new(HashMap::new()),
            pending_server_calls: Mutex::new(HashMap::new()),
            next_call_id: AtomicU64::new(1),
            pending_agent_calls: Mutex::new(HashMap::new()),
            pending_resets: AtomicUsize::new(0),
            reset_finished: tokio::sync::Notify::new(),
            registry_owner,
            fatal: AtomicBool::new(false),
            fatal_wake: tokio::sync::Notify::new(),
        }
    }

    pub(super) fn persist_acp_session_id(
        &self,
        acp_session_id: &str,
    ) -> std::result::Result<(), control_protocol::JsonRpcError> {
        let Some((session_id, owner_pid)) = self.registry_owner.as_ref() else {
            return Ok(());
        };
        if let Err(error) =
            worker_registry::update_stored_acp_session_id(session_id, *owner_pid, acp_session_id)
        {
            warn!(
                target: "acp.runner",
                session = %session_id,
                %error,
                "failed to persist ACP session identity; terminating runner"
            );
            self.fatal.store(true, Ordering::Release);
            self.fatal_wake.notify_one();
            return Err(control_protocol::JsonRpcError::new(
                control_protocol::INTERNAL_ERROR,
                format!("failed to persist ACP session identity: {error}"),
            ));
        }
        Ok(())
    }

    pub(super) async fn fatal_triggered(&self) {
        loop {
            let notified = self.fatal_wake.notified();
            if self.fatal.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
    pub(super) async fn deliver_line(
        &self,
        line: &[u8],
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
    ) {
        if let Some(id) = parse_response_id(line) {
            let responder = self.pending_client_responses.lock().await.remove(&id);
            if let Some(tx) = responder {
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) {
                    let (release, released) = tokio::sync::oneshot::channel();
                    let _ = tx.send(HandshakeResponse { value, release });
                    // Publish the authoritative handshake frame before parsing an
                    // immediately following native callback. Control loss drops the
                    // release sender too, so stdout cannot strand.
                    let _ = released.await;
                }
                return;
            }
        }

        if !self.prompt_requests.lock().await.is_empty() {
            if let Some((id, outcome)) = parse_response(line) {
                if self.prompt_requests.lock().await.remove(&id) {
                    self.enqueue(
                        DeliveryScope::Persistent,
                        QueuedKind::PromptCompleted,
                        ControlBody::PromptCompleted {
                            prompt_req_id: id,
                            outcome,
                        },
                    )
                    .await;
                    return;
                }
            }
        }

        if let Some(id) = parse_response_id(line) {
            let pending = self.pending_agent_calls.lock().await.remove(&id);
            if let Some(pending) = pending {
                let frame = match parse_agent_call_outcome(line) {
                    Ok(result) => match self
                        .refresh_session_from_reset(&pending.method, &result)
                        .await
                    {
                        Ok(()) => ControlBody::AgentResult {
                            call_id: pending.call_id,
                            result,
                        },
                        Err(error) => ControlBody::AgentError {
                            call_id: pending.call_id,
                            error,
                        },
                    },
                    Err(error) => ControlBody::AgentError {
                        call_id: pending.call_id,
                        error,
                    },
                };
                if pending.method == "session/new" {
                    self.finish_reset();
                }
                self.enqueue(
                    DeliveryScope::Attachment(pending.attachment_id),
                    QueuedKind::AgentReply,
                    frame,
                )
                .await;
                return;
            }
        }

        if let Some((agent_id, method)) = parse_request_value_id(line) {
            self.forward_server_call(agent_id, method, line, agent_stdin)
                .await;
            return;
        }

        if let Some((method, params)) = parse_notification(line) {
            if SessionReplayed::matches_method(&method) {
                return;
            }
            self.enqueue(
                DeliveryScope::Persistent,
                QueuedKind::Notify,
                ControlBody::Notify { method, params },
            )
            .await;
        }
    }

    pub(super) async fn forward_server_call(
        &self,
        agent_id: serde_json::Value,
        method: String,
        line: &[u8],
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
    ) {
        let params = serde_json::from_slice::<serde_json::Value>(line)
            .ok()
            .and_then(|mut v| v.get_mut("params").map(std::mem::take))
            .unwrap_or(serde_json::Value::Null);
        let Some(attachment_id) = self.control.lock().await.active_attachment else {
            self.answer_agent(
                agent_stdin,
                &agent_id,
                disconnected_server_call_outcome(&method),
            )
            .await;
            return;
        };
        let cached_session = {
            let handshake = self.handshake.lock().await;
            handshake.initialized.is_some() && handshake.session.is_some()
        };
        // These v1 callbacks also accept positional params with the session ID first.
        // Elicitation has a flattened object scope; request-scoped elicitation must
        // remain independent of session announcement.
        if cached_session
            && (params.get("sessionId").is_some()
                || (params.get(0).is_some()
                    && matches!(
                        method.as_str(),
                        "session/request_permission"
                            | "fs/read_text_file"
                            | "fs/write_text_file"
                            | "terminal/create"
                            | "terminal/output"
                            | "terminal/release"
                            | "terminal/wait_for_exit"
                            | "terminal/kill"
                    )))
        {
            loop {
                let changed = self.control_space.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                {
                    let channel = self.control.lock().await;
                    if channel.active_attachment != Some(attachment_id)
                        || channel.session_announced
                        || self.pending_resets.load(Ordering::Acquire) != 0
                    {
                        break;
                    }
                }
                // Stable cached reattach needs no native response. A sent reset
                // disables this wait so its response can still reach stdout.
                changed.await;
            }
        }
        let call_id = self.next_call_id.fetch_add(1, Ordering::Relaxed);
        let body = ControlBody::ServerCall {
            call_id,
            method: method.clone(),
            params,
        };
        let wire: Arc<[u8]> = match control_protocol::encode_frame(&body) {
            Ok(frame) => frame.into(),
            Err(error) => {
                warn!(target: "acp.runner", %error, "reverse call exceeds control frame limit");
                self.answer_agent(
                    agent_stdin,
                    &agent_id,
                    Err(control_protocol::JsonRpcError::new(
                        control_protocol::DAEMON_GONE,
                        "runner reverse call exceeds transport capacity",
                    )),
                )
                .await;
                return;
            }
        };

        let mut channel = self.control.lock().await;
        if channel.active_attachment != Some(attachment_id) {
            drop(channel);
            self.answer_agent(
                agent_stdin,
                &agent_id,
                disconnected_server_call_outcome(&method),
            )
            .await;
            return;
        }
        let mut pending = self.pending_server_calls.lock().await;
        if pending.len() >= MAX_OUTSTANDING_REQUESTS || !channel.make_room(wire.len()) {
            warn!(
                target: "acp.runner",
                method = %method,
                outstanding = pending.len(),
                "reverse-call capacity reached; refusing the request"
            );
            drop(pending);
            drop(channel);
            self.answer_agent(
                agent_stdin,
                &agent_id,
                Err(control_protocol::JsonRpcError::new(
                    control_protocol::DAEMON_GONE,
                    "runner reverse-call capacity exceeded",
                )),
            )
            .await;
            return;
        }

        pending.insert(
            call_id,
            PendingServerCall {
                agent_id,
                method,
                attachment_id,
            },
        );
        channel.push(
            DeliveryScope::Attachment(attachment_id),
            QueuedKind::ServerCall,
            wire,
        );
        drop(pending);
        drop(channel);
        self.control_wake.notify_one();
    }

    pub(super) async fn answer_agent(
        &self,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        agent_id: &serde_json::Value,
        outcome: Result<serde_json::Value, control_protocol::JsonRpcError>,
    ) -> bool {
        let response = match outcome {
            Ok(result) => serde_json::json!({
                "jsonrpc": "2.0",
                "id": agent_id,
                "result": result,
            }),
            Err(error) => serde_json::json!({
                "jsonrpc": "2.0",
                "id": agent_id,
                "error": error,
            }),
        };
        self.write_agent_line(agent_stdin, &response).await
    }

    pub(super) async fn resolve_server_call(
        &self,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        attachment_id: u64,
        call_id: u64,
        outcome: Result<serde_json::Value, control_protocol::JsonRpcError>,
        session_id: &str,
    ) {
        let pending = {
            let mut pending = self.pending_server_calls.lock().await;
            match pending.get(&call_id) {
                Some(call) if call.attachment_id == attachment_id => pending.remove(&call_id),
                _ => None,
            }
        };
        let Some(pending) = pending else {
            debug!(
                target: "acp.runner",
                session = %session_id,
                call_id,
                "ignoring answer for an unknown or detached reverse call"
            );
            return;
        };
        if !self
            .answer_agent(agent_stdin, &pending.agent_id, outcome)
            .await
        {
            warn!(
                target: "acp.runner",
                session = %session_id,
                call_id,
                method = %pending.method,
                "agent stdin write failed answering a reverse call"
            );
        }
    }

    pub(super) async fn disconnect_control(
        &self,
        attachment_id: u64,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        session_id: &str,
    ) {
        let drained = {
            let mut channel = self.control.lock().await;
            let mut pending = self.pending_server_calls.lock().await;
            let ids: Vec<u64> = pending
                .iter()
                .filter_map(|(id, call)| (call.attachment_id == attachment_id).then_some(*id))
                .collect();
            let drained: Vec<_> = ids
                .into_iter()
                .filter_map(|id| pending.remove(&id).map(|call| (id, call)))
                .collect();
            self.pending_agent_calls.lock().await.retain(|_, call| {
                call.attachment_id != attachment_id || call.method == "session/new"
            });
            channel.purge_attachment(attachment_id);
            drained
        };
        self.control_space.notify_waiters();
        if !drained.is_empty() {
            info!(
                target: "acp.runner",
                session = %session_id,
                count = drained.len(),
                "synthesising responses for reverse calls on control disconnect"
            );
        }
        for (_, pending) in drained {
            if !self
                .answer_agent(
                    agent_stdin,
                    &pending.agent_id,
                    disconnected_server_call_outcome(&pending.method),
                )
                .await
            {
                warn!(target: "acp.runner", session = %session_id, "agent stdin write failed during disconnect cleanup");
                break;
            }
        }
    }

    pub(super) async fn abort_agent_calls(&self, session_id: &str) {
        let drained: Vec<PendingAgentCall> = {
            let mut map = self.pending_agent_calls.lock().await;
            map.drain().map(|(_, pending)| pending).collect()
        };
        if drained.is_empty() {
            return;
        }
        info!(
            target: "acp.runner",
            session = %session_id,
            count = drained.len(),
            "failing in-flight forward calls; agent is gone"
        );
        for pending in drained {
            if pending.method == "session/new" {
                self.finish_reset();
            }
            self.enqueue(
                DeliveryScope::Attachment(pending.attachment_id),
                QueuedKind::AgentReply,
                ControlBody::AgentError {
                    call_id: pending.call_id,
                    error: control_protocol::JsonRpcError::new(
                        control_protocol::DAEMON_GONE,
                        "agent exited before answering",
                    ),
                },
            )
            .await;
        }
    }

    pub(super) async fn enqueue(
        &self,
        scope: DeliveryScope,
        kind: QueuedKind,
        body: ControlBody,
    ) -> bool {
        let announces_session = matches!(&body, ControlBody::SessionReady { .. });
        let wire: Arc<[u8]> = match control_protocol::encode_frame(&body) {
            Ok(frame) => frame.into(),
            Err(error) => {
                warn!(target: "acp.runner", %error, "dropping unrepresentable control frame");
                return false;
            }
        };
        loop {
            let space = self.control_space.notified();
            {
                let mut channel = self.control.lock().await;
                if let DeliveryScope::Attachment(attachment_id) = scope {
                    if channel.active_attachment != Some(attachment_id) {
                        return false;
                    }
                }
                if channel.make_room(wire.len()) {
                    channel.push(scope, kind, Arc::clone(&wire));
                    if announces_session {
                        channel.session_announced = true;
                    }
                    #[cfg(debug_assertions)]
                    if kind == QueuedKind::PromptCompleted {
                        if let Some(path) = std::env::var_os("AOE_E2E_PROMPT_COMPLETED_FILE") {
                            std::fs::write(path, b"queued").expect("publish e2e prompt completion");
                        }
                    }
                    drop(channel);
                    if announces_session {
                        self.control_space.notify_waiters();
                    }
                    self.control_wake.notify_one();
                    return true;
                }
                if kind == QueuedKind::Notify {
                    return false;
                }
            }
            space.await;
        }
    }

    /// Queue a handshake reply; an established session is followed by its replay barrier.
    pub(super) async fn enqueue_handshake(
        &self,
        attachment_id: u64,
        frame: ControlBody,
        established: bool,
    ) {
        let scope = DeliveryScope::Attachment(attachment_id);
        if self.enqueue(scope, QueuedKind::Handshake, frame).await && established {
            let marker = SessionReplayed::default();
            self.enqueue(
                scope,
                QueuedKind::SessionReplayed,
                ControlBody::Notify {
                    method: marker.method().into(),
                    params: serde_json::json!({}),
                },
            )
            .await;
        }
    }

    pub(super) async fn begin_attachment(&self) -> u64 {
        let attachment_id = self.next_attachment_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut channel = self.control.lock().await;
            channel.active_attachment = Some(attachment_id);
            channel.session_announced = false;
        }
        self.control_space.notify_waiters();
        self.control_wake.notify_one();
        attachment_id
    }
    pub(super) async fn cached_prompt_completion(&self) -> Option<Arc<[u8]>> {
        self.control.lock().await.last_prompt_completion.clone()
    }

    pub(super) async fn clear_prompt_completion(&self) {
        self.control.lock().await.last_prompt_completion = None;
    }

    pub(super) async fn install_control(
        &self,
        out: &mut Option<tokio::net::unix::OwnedWriteHalf>,
        session_id: &str,
    ) -> bool {
        let hello = ControlBody::Hello {
            control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
            session_id: session_id.to_string(),
        };
        write_control_frame(out.as_mut().expect("write half present"), &hello).await
    }

    pub(super) async fn next_outbound(&self, attachment_id: u64) -> Option<(u64, Arc<[u8]>)> {
        let mut channel = self.control.lock().await;
        if channel.active_attachment != Some(attachment_id) || channel.in_flight.is_some() {
            return None;
        }
        // Handshake frames go first, and Persistent frames wait for SessionReady, so the
        // daemon never buffers unbounded pre-identity session updates.
        let announced = channel.session_announced;
        let index = channel
            .queue
            .iter()
            .position(|frame| frame.kind == QueuedKind::Handshake)
            .or_else(|| {
                channel.queue.iter().position(|frame| {
                    announced || !matches!(frame.scope, DeliveryScope::Persistent)
                })
            })?;
        let (id, wire) = {
            let frame = channel.queue.get(index)?;
            (frame.id, Arc::clone(&frame.wire))
        };
        channel.in_flight = Some(id);
        Some((id, wire))
    }

    pub(super) async fn commit_outbound(&self, attachment_id: u64, entry_id: u64) {
        let mut channel = self.control.lock().await;
        if channel.active_attachment == Some(attachment_id) && channel.in_flight == Some(entry_id) {
            if let Some(index) = channel.queue.iter().position(|frame| frame.id == entry_id) {
                if channel.queue[index].kind == QueuedKind::PromptCompleted {
                    channel.last_prompt_completion = Some(Arc::clone(&channel.queue[index].wire));
                }
                channel.remove_at(index);
            }
        }
        if channel.in_flight == Some(entry_id) {
            channel.in_flight = None;
        }
        drop(channel);
        self.control_space.notify_waiters();
    }

    pub(super) async fn release_outbound(&self, entry_id: u64) {
        let mut channel = self.control.lock().await;
        if channel.in_flight == Some(entry_id) {
            channel.in_flight = None;
        }
    }
    pub(super) async fn write_agent_line(
        &self,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        value: &serde_json::Value,
    ) -> bool {
        let mut bytes = match serde_json::to_vec(value) {
            Ok(b) => b,
            Err(_) => return false,
        };
        bytes.push(b'\n');
        let mut stdin = agent_stdin.lock().await;
        stdin.write_all(&bytes).await.is_ok() && stdin.flush().await.is_ok()
    }

    pub(super) async fn agent_request(
        &self,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        method: &str,
        params: serde_json::Value,
        dispatch_release: &mut Option<tokio::sync::oneshot::Sender<()>>,
    ) -> Option<serde_json::Value> {
        let id = self.next_req_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending_client_responses.lock().await.insert(id, tx);
        if !self
            .write_agent_line(agent_stdin, &rpc_request(id, method, params))
            .await
        {
            self.pending_client_responses.lock().await.remove(&id);
            return None;
        }
        let HandshakeResponse {
            value: response,
            release,
        } = rx.await.ok()?;
        *dispatch_release = Some(release);
        Some(response)
    }

    pub(super) async fn agent_prompt(
        &self,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        attachment_id: u64,
        params: serde_json::Value,
    ) -> Option<i64> {
        let id = self.next_req_id.fetch_add(1, Ordering::Relaxed);
        if !self
            .enqueue(
                DeliveryScope::Attachment(attachment_id),
                QueuedKind::PromptStarted,
                ControlBody::PromptStarted { prompt_req_id: id },
            )
            .await
        {
            return None;
        }
        self.clear_prompt_completion().await;
        self.prompt_requests.lock().await.insert(id);
        if !self
            .write_agent_line(agent_stdin, &rpc_request(id, PROMPT_METHOD, params))
            .await
        {
            self.prompt_requests.lock().await.remove(&id);
            return None;
        }
        Some(id)
    }

    pub(super) async fn agent_cancel(
        &self,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        acp_session_id: &str,
    ) {
        let note = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": acp_session_id },
        });
        let _ = self.write_agent_line(agent_stdin, &note).await;
    }

    pub(super) async fn run_or_replay_initialize(
        &self,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        request: serde_json::Value,
        dispatch_release: &mut Option<tokio::sync::oneshot::Sender<()>>,
    ) -> Result<serde_json::Value, serde_json::Value> {
        let _gate = self.handshake_gate.lock().await;
        if let Some(cached) = self.handshake.lock().await.initialized.clone() {
            return Ok(cached);
        }
        let response = self
            .agent_request(agent_stdin, "initialize", request, dispatch_release)
            .await
            .ok_or_else(|| transport_error("agent closed before answering initialize"))?;
        let result = handshake_result(&response)?;
        self.handshake.lock().await.initialized = Some(result.clone());
        Ok(result)
    }

    pub(super) async fn run_or_replay_session(
        &self,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        method: &str,
        request: serde_json::Value,
        dispatch_release: &mut Option<tokio::sync::oneshot::Sender<()>>,
    ) -> Result<(String, serde_json::Value), serde_json::Value> {
        let _gate = self.handshake_gate.lock().await;
        if let Some(cached) = self.handshake.lock().await.session.clone() {
            return Ok(cached);
        }
        let response = self
            .agent_request(agent_stdin, method, request.clone(), dispatch_release)
            .await
            .ok_or_else(|| transport_error(&format!("agent closed before answering {method}")))?;
        let result = handshake_result(&response)?;
        let acp_session_id = established_session_id(method, &request, &result)?;
        self.persist_acp_session_id(&acp_session_id)
            .map_err(|error| serde_json::to_value(error).expect("JSON-RPC error serializes"))?;
        let cached = (acp_session_id, result);
        self.handshake.lock().await.session = Some(cached.clone());
        Ok(cached)
    }

    pub(super) fn finish_reset(&self) {
        self.pending_resets.fetch_sub(1, Ordering::AcqRel);
        self.reset_finished.notify_waiters();
    }

    /// The accept loop admits no new daemon until the previous attachment's requests are
    /// registered, so its sent resets are visible here.
    pub(super) async fn resume_session(
        &self,
    ) -> Result<(String, serde_json::Value), serde_json::Value> {
        loop {
            let finished = self.reset_finished.notified();
            if self.pending_resets.load(Ordering::Acquire) == 0 {
                if self.fatal.load(Ordering::Acquire) {
                    return Err(transport_error(
                        "runner session state could not be committed",
                    ));
                }
                return self
                    .handshake
                    .lock()
                    .await
                    .session
                    .clone()
                    .ok_or_else(|| transport_error("runner has no established session to resume"));
            }
            finished.await;
        }
    }

    pub(super) async fn acp_session_id(&self) -> Option<String> {
        self.handshake
            .lock()
            .await
            .session
            .as_ref()
            .map(|(id, _)| id.clone())
    }
    pub(super) async fn issue_agent_call(
        &self,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        attachment_id: u64,
        call_id: u64,
        method: &str,
        params: serde_json::Value,
    ) {
        let req_id = self.next_req_id.fetch_add(1, Ordering::Relaxed);
        if method == "session/new" {
            self.pending_resets.fetch_add(1, Ordering::AcqRel);
            self.control_space.notify_waiters();
        }
        self.pending_agent_calls.lock().await.insert(
            req_id,
            PendingAgentCall {
                call_id,
                attachment_id,
                method: method.to_string(),
            },
        );
        if !self
            .write_agent_line(agent_stdin, &rpc_request(req_id, method, params))
            .await
        {
            if let Some(pending) = self.pending_agent_calls.lock().await.remove(&req_id) {
                if pending.method == "session/new" {
                    self.finish_reset();
                }
            } else {
                return; // The agent already answered while the write was finishing.
            }
            self.enqueue(
                DeliveryScope::Attachment(attachment_id),
                QueuedKind::AgentReply,
                ControlBody::AgentError {
                    call_id,
                    error: control_protocol::JsonRpcError::new(
                        control_protocol::DAEMON_GONE,
                        format!("agent stdin write failed for {method}"),
                    ),
                },
            )
            .await;
        }
    }

    pub(super) async fn refresh_session_from_reset(
        &self,
        method: &str,
        result: &serde_json::Value,
    ) -> std::result::Result<(), control_protocol::JsonRpcError> {
        if method != "session/new" {
            return Ok(());
        }
        let Some(sid) = result.get("sessionId").and_then(|value| value.as_str()) else {
            return Ok(());
        };
        if self
            .handshake
            .lock()
            .await
            .session
            .as_ref()
            .is_some_and(|(current, _)| current == sid)
        {
            return Ok(());
        }
        self.persist_acp_session_id(sid)?;
        info!(
            target: "acp.runner",
            new_acp_session_id = %sid,
            "daemon-driven session/new observed; refreshing handshake cache"
        );
        self.handshake.lock().await.session = Some((sid.to_string(), result.clone()));
        Ok(())
    }
}

pub(super) fn rpc_request(id: i64, method: &str, params: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

pub(super) fn handshake_result(
    response: &serde_json::Value,
) -> Result<serde_json::Value, serde_json::Value> {
    if let Some(err) = response.get("error") {
        return Err(err.clone());
    }
    response
        .get("result")
        .cloned()
        .ok_or_else(|| transport_error("response had neither result nor error"))
}

/// `session/load` responses carry no id per ACP; an extension id must agree with the request.
pub(super) fn established_session_id(
    method: &str,
    request: &serde_json::Value,
    result: &serde_json::Value,
) -> Result<String, serde_json::Value> {
    match method {
        "session/load" => {
            let requested = request
                .get("sessionId")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| transport_error("session/load request missing sessionId"))?;
            let result = result
                .as_object()
                .ok_or_else(|| transport_error("session/load response result was not an object"))?;
            if let Some(returned) = result.get("sessionId") {
                let returned = returned.as_str().ok_or_else(|| {
                    transport_error("session/load response sessionId was not a string")
                })?;
                if returned != requested {
                    return Err(transport_error(
                        "session/load response sessionId did not match request",
                    ));
                }
            }
            Ok(requested.to_string())
        }
        "session/new" | "session/fork" => result
            .get("sessionId")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| transport_error(&format!("{method} response missing sessionId")))
            .map(str::to_string),
        _ => Err(transport_error(&format!(
            "unsupported session establishment method {method}"
        ))),
    }
}

pub(super) fn transport_error(message: &str) -> serde_json::Value {
    serde_json::json!({ "code": -32603, "message": message })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::control_protocol::PromptOutcome;
    use std::process::Stdio;
    use tokio::process::Command;

    async fn shared_with_stdin() -> (
        Arc<RunnerShared>,
        Mutex<tokio::process::ChildStdin>,
        tokio::process::Child,
    ) {
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .expect("spawn cat");
        let stdin = child.stdin.take().expect("stdin piped");
        (Arc::new(RunnerShared::new(None)), Mutex::new(stdin), child)
    }

    async fn queued(shared: &RunnerShared) -> Vec<ControlBody> {
        let channel = shared.control.lock().await;
        channel
            .queue
            .iter()
            .map(|entry| {
                serde_json::from_slice(&entry.wire[4..])
                    .expect("queued control frame is valid JSON")
            })
            .collect()
    }

    async fn read_agent_stdin(
        stdin: &Mutex<tokio::process::ChildStdin>,
        child: &mut tokio::process::Child,
    ) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut input = stdin.lock().await;
            input.write_all(b"\x1e").await.expect("write stream fence");
            input.flush().await.expect("flush stream fence");
            drop(input);
            let out = child.stdout.as_mut().expect("stdout piped");
            let mut buf = Vec::new();
            loop {
                let byte = out.read_u8().await.expect("echo before EOF");
                if byte == 0x1e {
                    return String::from_utf8(buf).expect("utf8");
                }
                buf.push(byte);
            }
        })
        .await
        .expect("agent echo did not reach the stream fence")
    }

    #[test]
    fn established_session_id_is_method_sensitive() {
        let cases = [
            (
                "session/new",
                serde_json::json!({}),
                serde_json::json!({"sessionId": "new-id"}),
                Ok("new-id"),
            ),
            (
                "session/new",
                serde_json::json!({}),
                serde_json::json!({}),
                Err("session/new response missing sessionId"),
            ),
            (
                "session/fork",
                serde_json::json!({"sessionId": "parent-id"}),
                serde_json::json!({"sessionId": "fork-id"}),
                Ok("fork-id"),
            ),
            (
                "session/fork",
                serde_json::json!({"sessionId": "parent-id"}),
                serde_json::json!({}),
                Err("session/fork response missing sessionId"),
            ),
            (
                "session/load",
                serde_json::json!({"sessionId": "existing-id"}),
                serde_json::json!({"configOptions": []}),
                Ok("existing-id"),
            ),
            (
                "session/load",
                serde_json::json!({"sessionId": "existing-id"}),
                serde_json::json!({"sessionId": "existing-id"}),
                Ok("existing-id"),
            ),
            (
                "session/load",
                serde_json::json!({"sessionId": "existing-id"}),
                serde_json::json!({"sessionId": "different-id"}),
                Err("session/load response sessionId did not match request"),
            ),
            (
                "session/load",
                serde_json::json!({}),
                serde_json::json!({}),
                Err("session/load request missing sessionId"),
            ),
            (
                "session/load",
                serde_json::json!({"sessionId": "existing-id"}),
                serde_json::json!({"sessionId": 42}),
                Err("session/load response sessionId was not a string"),
            ),
            (
                "session/load",
                serde_json::json!({"sessionId": "existing-id"}),
                serde_json::json!(null),
                Err("session/load response result was not an object"),
            ),
            (
                "session/resume",
                serde_json::json!({}),
                serde_json::json!({}),
                Err("unsupported session establishment method session/resume"),
            ),
        ];

        for (method, request, result, expected) in cases {
            match (established_session_id(method, &request, &result), expected) {
                (Ok(actual), Ok(expected)) => assert_eq!(actual, expected, "{method}"),
                (Err(actual), Err(expected)) => {
                    assert_eq!(actual["message"], expected, "{method}")
                }
                (actual, expected) => panic!("{method}: got {actual:?}, expected {expected:?}"),
            }
        }
    }

    #[tokio::test]
    async fn reset_refresh_only_follows_a_response_carrying_a_session_id() {
        let shared = RunnerShared::new(None);
        shared.handshake.lock().await.session =
            Some(("sid-1".into(), serde_json::json!({ "sessionId": "sid-1" })));

        shared
            .refresh_session_from_reset("session/new", &serde_json::json!({"sessionId": "sid-2"}))
            .await
            .unwrap();
        assert_eq!(
            shared.acp_session_id().await.as_deref(),
            Some("sid-2"),
            "the cache follows the reset"
        );

        shared
            .refresh_session_from_reset("session/set_mode", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(shared.acp_session_id().await.as_deref(), Some("sid-2"));
    }

    #[tokio::test]
    async fn prompt_completion_follows_the_notifications_before_it() {
        let (shared, stdin, _child) = shared_with_stdin().await;
        let attachment_id = shared.begin_attachment().await;
        let id = shared
            .agent_prompt(&stdin, attachment_id, serde_json::json!({"sessionId": "s"}))
            .await
            .expect("prompt written");
        assert!(shared.prompt_requests.lock().await.contains(&id));

        for text in ["first", "second"] {
            let line = format!(
                "{{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{{\"text\":\"{text}\"}}}}\n"
            );
            shared.deliver_line(line.as_bytes(), &stdin).await;
        }
        let resp = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"stopReason\":\"end_turn\"}}}}\n"
        );
        shared.deliver_line(resp.as_bytes(), &stdin).await;

        assert!(shared.prompt_requests.lock().await.is_empty());
        let notify = |text: &str| ControlBody::Notify {
            method: "session/update".into(),
            params: serde_json::json!({ "text": text }),
        };
        assert_eq!(
            queued(&shared).await,
            vec![
                ControlBody::PromptStarted { prompt_req_id: id },
                notify("first"),
                notify("second"),
                ControlBody::PromptCompleted {
                    prompt_req_id: id,
                    outcome: PromptOutcome::Completed {
                        stop_reason: Some("end_turn".into()),
                    },
                },
            ]
        );
    }

    #[tokio::test]
    async fn prompt_start_precedes_agent_write_and_is_not_replayed() {
        let (shared, stdin, mut child) = shared_with_stdin().await;
        let attachment_id = shared.begin_attachment().await;
        let stdin_guard = stdin.lock().await;
        let prompt =
            shared.agent_prompt(&stdin, attachment_id, serde_json::json!({"sessionId": "s"}));
        tokio::pin!(prompt);
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(prompt.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        let frames = queued(&shared).await;
        let [ControlBody::PromptStarted { prompt_req_id }] = frames.as_slice() else {
            panic!(
                "prompt correlation must be queued while the agent write is blocked: {frames:?}"
            );
        };
        drop(stdin_guard);
        assert_eq!(prompt.await, Some(*prompt_req_id));
        let written = read_agent_stdin(&stdin, &mut child).await;
        let request: serde_json::Value = serde_json::from_str(written.trim()).unwrap();
        assert_eq!(request["id"], *prompt_req_id);
        assert_eq!(request["method"], PROMPT_METHOD);

        shared.disconnect_control(attachment_id, &stdin, "s").await;
        let _new_attachment = shared.begin_attachment().await;
        let response = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{prompt_req_id},\"result\":{{\"stopReason\":\"end_turn\"}}}}\n"
        );
        shared.deliver_line(response.as_bytes(), &stdin).await;
        assert_eq!(
            queued(&shared).await,
            vec![ControlBody::PromptCompleted {
                prompt_req_id: *prompt_req_id,
                outcome: PromptOutcome::Completed {
                    stop_reason: Some("end_turn".into()),
                },
            }],
            "only the durable completion may reach the replacement attachment"
        );
        assert!(shared
            .agent_prompt(&stdin, attachment_id, serde_json::json!({}))
            .await
            .is_none());
        assert!(read_agent_stdin(&stdin, &mut child).await.is_empty());
    }

    #[tokio::test]
    async fn agent_request_becomes_a_server_call() {
        let (shared, stdin, _child) = shared_with_stdin().await;
        let _attachment_id = shared.begin_attachment().await;
        let req = br#"{"jsonrpc":"2.0","id":"req-1","method":"fs/read_text_file","params":{"path":"/tmp/x"}}
"#;
        shared.deliver_line(req, &stdin).await;

        let q = queued(&shared).await;
        let ControlBody::ServerCall {
            call_id,
            method,
            params,
        } = &q[0]
        else {
            panic!("expected a ServerCall, got {q:?}");
        };
        assert_eq!(method, "fs/read_text_file");
        assert_eq!(params, &serde_json::json!({"path": "/tmp/x"}));
        let pending = shared.pending_server_calls.lock().await;
        let entry = pending.get(call_id).expect("call is tracked");
        assert_eq!(entry.agent_id, serde_json::json!("req-1"));
        assert_eq!(entry.method, "fs/read_text_file");
    }

    #[tokio::test]
    async fn control_disconnect_cancels_reverse_calls_rather_than_replaying() {
        for (method, expect_cancelled) in [
            (PERMISSION_METHOD, true),
            ("fs/write_text_file", false),
            ("terminal/create", false),
        ] {
            let (shared, stdin, mut child) = shared_with_stdin().await;
            let attachment_id = shared.begin_attachment().await;
            let req = format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"{method}\",\"params\":{{}}}}\n"
            );
            shared.deliver_line(req.as_bytes(), &stdin).await;
            assert_eq!(shared.pending_server_calls.lock().await.len(), 1);

            shared
                .disconnect_control(attachment_id, &stdin, "s-1")
                .await;

            assert!(
                shared.pending_server_calls.lock().await.is_empty(),
                "{method}: the sweep drains the map"
            );
            assert!(
                !queued(&shared)
                    .await
                    .iter()
                    .any(|frame| matches!(frame, ControlBody::ServerCall { .. })),
                "{method}: the queued call must not survive to be replayed"
            );
            let assert_response = |sent: &serde_json::Value, id: i64| {
                assert_eq!(sent["id"], serde_json::json!(id), "{method}: id is echoed");
                if expect_cancelled {
                    assert_eq!(
                        sent["result"],
                        serde_json::json!({"outcome": {"outcome": "cancelled"}}),
                        "permission gets the semantic cancelled outcome"
                    );
                } else {
                    assert_eq!(
                        sent["error"]["code"],
                        serde_json::json!(control_protocol::DAEMON_GONE),
                        "{method}: gets a method-agnostic error"
                    );
                }
            };
            let written = read_agent_stdin(&stdin, &mut child).await;
            let sent: serde_json::Value =
                serde_json::from_str(written.trim()).expect("a response line was written");
            assert_response(&sent, 42);

            let late = format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":43,\"method\":\"{method}\",\"params\":{{}}}}\n"
            );
            shared.deliver_line(late.as_bytes(), &stdin).await;
            assert!(shared.pending_server_calls.lock().await.is_empty());
            assert!(!queued(&shared)
                .await
                .iter()
                .any(|frame| matches!(frame, ControlBody::ServerCall { .. })));
            let written = read_agent_stdin(&stdin, &mut child).await;
            let sent: serde_json::Value =
                serde_json::from_str(written.trim()).expect("a detached response line was written");
            assert_response(&sent, 43);
        }
    }

    #[tokio::test]
    async fn reverse_call_cap_refuses_rather_than_parking_the_agent() {
        let (shared, stdin, mut child) = shared_with_stdin().await;
        let _attachment_id = shared.begin_attachment().await;
        for id in 0..MAX_OUTSTANDING_REQUESTS {
            let line = format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"fs/read_text_file\",\"params\":{{}}}}\n"
            );
            shared.deliver_line(line.as_bytes(), &stdin).await;
        }
        assert_eq!(
            shared.pending_server_calls.lock().await.len(),
            MAX_OUTSTANDING_REQUESTS
        );

        let extra = br#"{"jsonrpc":"2.0","id":999999,"method":"fs/read_text_file","params":{}}
"#;
        shared.deliver_line(extra, &stdin).await;
        assert_eq!(
            shared.pending_server_calls.lock().await.len(),
            MAX_OUTSTANDING_REQUESTS,
            "the refused call is not tracked"
        );
        let written = read_agent_stdin(&stdin, &mut child).await;
        let last: serde_json::Value = serde_json::from_str(
            written
                .trim()
                .lines()
                .next_back()
                .expect("a refusal was written"),
        )
        .expect("refusal is JSON");
        assert_eq!(last["id"], serde_json::json!(999999));
        assert_eq!(
            last["error"]["code"],
            serde_json::json!(control_protocol::DAEMON_GONE)
        );
    }

    #[test]
    fn queue_budget_sheds_notifications_and_preserves_correlations() {
        let wire = |len| Arc::<[u8]>::from(vec![0; len]);
        let mut channel = ControlChannel::default();
        channel.push(DeliveryScope::Persistent, QueuedKind::AgentReply, wire(16));
        channel.push(DeliveryScope::Persistent, QueuedKind::Notify, wire(8));
        channel.push(DeliveryScope::Persistent, QueuedKind::Notify, wire(8));
        channel.push(
            DeliveryScope::Persistent,
            QueuedKind::PromptCompleted,
            wire(16),
        );

        assert!(channel.make_room(control_protocol::MAX_CONTROL_QUEUE_BYTES - 32));
        assert_eq!(channel.queued_bytes, 32);
        assert_eq!(channel.queue.len(), 2);
        assert!(channel
            .queue
            .iter()
            .all(|frame| frame.kind != QueuedKind::Notify));
        assert!(
            !channel.make_room(control_protocol::MAX_CONTROL_QUEUE_BYTES - 31),
            "correlation frames must apply backpressure rather than be evicted"
        );
    }

    /// The replay barrier follows the updates an agent sent before its session reply,
    /// which itself goes out first; an agent cannot forge it (#4016).
    #[tokio::test]
    async fn established_session_barrier_follows_its_replay() {
        let (shared, stdin, _child) = shared_with_stdin().await;
        let attachment = shared.begin_attachment().await;
        for line in [
            r#"{"jsonrpc":"2.0","method":"session/update","params":{"text":"replayed"}}"#,
            r#"{"jsonrpc":"2.0","method":"_aoe/session_replayed","params":{}}"#,
        ] {
            shared.deliver_line(line.as_bytes(), &stdin).await;
        }
        let ready = ControlBody::SessionReady {
            acp_session_id: "s".into(),
            result: serde_json::json!({}),
        };
        shared
            .enqueue_handshake(attachment, ready.clone(), true)
            .await;

        let mut sent = Vec::new();
        while let Some((id, wire)) = shared.next_outbound(attachment).await {
            sent.push(serde_json::from_slice::<ControlBody>(&wire[4..]).unwrap());
            shared.commit_outbound(attachment, id).await;
        }
        let notify = |method: &str, params| ControlBody::Notify {
            method: method.into(),
            params,
        };
        assert_eq!(
            sent,
            [
                ready,
                notify("session/update", serde_json::json!({"text":"replayed"})),
                notify("_aoe/session_replayed", serde_json::json!({})),
            ]
        );
    }

    #[tokio::test]
    async fn outbound_frame_remains_queued_until_write_commit() {
        let shared = RunnerShared::new(None);
        shared
            .enqueue(
                DeliveryScope::Persistent,
                QueuedKind::PromptCompleted,
                ControlBody::PromptCompleted {
                    prompt_req_id: 7,
                    outcome: PromptOutcome::Aborted,
                },
            )
            .await;
        let first_attachment = shared.begin_attachment().await;
        // A live completion arises in an established, announced session; the detached
        // backlog is otherwise held until SessionReady.
        shared.control.lock().await.session_announced = true;
        let (entry_id, first_wire) = shared
            .next_outbound(first_attachment)
            .await
            .expect("leased frame");

        shared.release_outbound(entry_id).await;
        let second_attachment = shared.begin_attachment().await;
        shared.control.lock().await.session_announced = true;
        let (retried_id, retried_wire) = shared
            .next_outbound(second_attachment)
            .await
            .expect("frame survives cancelled writer");
        assert_eq!(retried_id, entry_id);
        assert_eq!(retried_wire, first_wire);

        shared.commit_outbound(second_attachment, retried_id).await;
        let channel = shared.control.lock().await;
        assert!(channel.queue.is_empty());
        assert_eq!(channel.queued_bytes, 0);
    }

    #[tokio::test]
    async fn disconnect_purges_scoped_frames_and_late_forward_responses() {
        let (shared, stdin, mut child) = shared_with_stdin().await;
        let old_attachment = shared.begin_attachment().await;
        assert!(
            shared
                .enqueue(
                    DeliveryScope::Attachment(old_attachment),
                    QueuedKind::Handshake,
                    ControlBody::Initialized {
                        result: serde_json::json!({}),
                    },
                )
                .await
        );
        shared
            .issue_agent_call(
                &stdin,
                old_attachment,
                9,
                "session/set_mode",
                serde_json::json!({}),
            )
            .await;
        let written = read_agent_stdin(&stdin, &mut child).await;
        let request: serde_json::Value = serde_json::from_str(written.trim()).unwrap();
        let request_id = request["id"].as_i64().unwrap();

        shared.disconnect_control(old_attachment, &stdin, "s").await;
        let _new_attachment = shared.begin_attachment().await;
        assert!(
            !shared
                .enqueue(
                    DeliveryScope::Attachment(old_attachment),
                    QueuedKind::Handshake,
                    ControlBody::Initialized {
                        result: serde_json::json!({"stale": true}),
                    },
                )
                .await
        );
        let response = format!("{{\"jsonrpc\":\"2.0\",\"id\":{request_id},\"result\":{{}}}}\n");
        shared.deliver_line(response.as_bytes(), &stdin).await;

        assert!(shared.pending_agent_calls.lock().await.is_empty());
        assert!(queued(&shared).await.is_empty());
    }

    #[tokio::test]
    async fn detached_reset_response_refreshes_session_without_replaying_result() {
        let (shared, stdin, mut child) = shared_with_stdin().await;
        let old_attachment = shared.begin_attachment().await;
        shared.handshake.lock().await.session = Some((
            "old-session".into(),
            serde_json::json!({"sessionId": "old-session"}),
        ));
        shared
            .issue_agent_call(
                &stdin,
                old_attachment,
                77,
                "session/new",
                serde_json::json!({"cwd": "/tmp"}),
            )
            .await;
        let written = read_agent_stdin(&stdin, &mut child).await;
        let sent: serde_json::Value =
            serde_json::from_str(written.trim()).expect("request written");
        assert_eq!(sent["method"], "session/new");
        let req_id = sent["id"].as_i64().expect("runner allocated a numeric id");

        shared
            .disconnect_control(old_attachment, &stdin, "session")
            .await;
        let _new_attachment = shared.begin_attachment().await;
        let resp = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{req_id},\"result\":{{\"sessionId\":\"new-session\"}}}}\n"
        );
        shared.deliver_line(resp.as_bytes(), &stdin).await;

        assert!(
            queued(&shared).await.is_empty(),
            "the detached daemon's AgentResult must not reach its replacement"
        );
        assert_eq!(
            shared.acp_session_id().await.as_deref(),
            Some("new-session"),
            "the runner cache must follow the completed reset across reattach"
        );
    }

    #[tokio::test]
    async fn forward_call_error_envelope_is_preserved() {
        let (shared, stdin, mut child) = shared_with_stdin().await;
        let attachment_id = shared.begin_attachment().await;
        shared
            .issue_agent_call(
                &stdin,
                attachment_id,
                5,
                "session/set_mode",
                serde_json::json!({}),
            )
            .await;
        let written = read_agent_stdin(&stdin, &mut child).await;
        let sent: serde_json::Value = serde_json::from_str(written.trim()).unwrap();
        let req_id = sent["id"].as_i64().unwrap();

        let resp = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{req_id},\"error\":{{\"code\":-32000,\"message\":\"nope\"}}}}\n"
        );
        shared.deliver_line(resp.as_bytes(), &stdin).await;

        let q = queued(&shared).await;
        match &q[0] {
            ControlBody::AgentError { call_id, error } => {
                assert_eq!(*call_id, 5);
                assert_eq!(error.code, -32000);
                assert_eq!(error.message, "nope");
            }
            other => panic!("expected AgentError, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn duplicate_answer_for_a_resolved_call_is_dropped() {
        let (shared, stdin, mut child) = shared_with_stdin().await;
        let attachment_id = shared.begin_attachment().await;
        let req = br#"{"jsonrpc":"2.0","id":3,"method":"fs/read_text_file","params":{}}
"#;
        shared.deliver_line(req, &stdin).await;
        let call_id = *shared
            .pending_server_calls
            .lock()
            .await
            .keys()
            .next()
            .expect("tracked");

        shared
            .resolve_server_call(
                &stdin,
                attachment_id,
                call_id,
                Ok(serde_json::json!({"content": "x"})),
                "s",
            )
            .await;
        shared
            .resolve_server_call(
                &stdin,
                attachment_id,
                call_id,
                Ok(serde_json::json!({"content": "y"})),
                "s",
            )
            .await;
        let written = read_agent_stdin(&stdin, &mut child).await;
        assert_eq!(
            written.trim().lines().count(),
            1,
            "exactly one response reached the agent: {written}"
        );
    }
}
