//! Native session admission, selected only by the connection lifecycle.

use agent_client_protocol::schema::v1::{RequestId, SessionId, SessionNotification};
use agent_client_protocol::{
    Agent, ConnectionTo, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, UntypedMessage,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex as StateMutex;
use tokio::sync::{oneshot, Mutex, MutexGuard, Notify};

use super::errors::acp_internal_error;
use crate::acp::control_protocol::{
    PromptCompletedMarker, SessionReplayed, MAX_CONTROL_QUEUE_BYTES, MAX_CONTROL_QUEUE_FRAMES,
};
use crate::acp::state::{AuthStatus, Event, AUTH_STATUS_UPDATE_METHOD};

// The replayed backlog a reattach flushes is exactly the runner's detached
// control queue, so this buffer is sized against the same contract: a
// legitimate reattach must accept the whole flushed queue rather than be
// killed for a backlog the runner was entitled to hold. Charge original control
// frame bytes: typed schema serialization can expand the native representation.
// Direct stdio has no detached queue and bounds its typed notification payload.
const MAX_PENDING_UPDATES: usize = MAX_CONTROL_QUEUE_FRAMES;
const MAX_PENDING_BYTES: usize = MAX_CONTROL_QUEUE_BYTES;

// Only the daemon control reader writes this synthetic transport field.
pub(super) const CONTROL_FRAME_BYTES_FIELD: &str = "__aoe_control_frame_bytes";

/// Every agent-side notification the connection admits, on one handler.
///
/// The SDK renders its whole handler chain per inbound message
/// (`trace!(handler = ?handler.describe_chain())`), and each link nests the
/// links below it through `{:?}`, so the rendered description roughly doubles
/// per added link. One more `on_receive_notification` therefore doubles the
/// per-notification cost of a replay, so new agent-side notifications join
/// this type rather than extending the chain.
#[derive(Clone, Debug)]
pub(super) enum SessionIngressNotification {
    Update(serde_json::Value),
    /// Runner-minted barrier marking the end of a session/load replay (#4016).
    Replayed(SessionReplayed),
    /// Daemon-minted barrier releasing a local prompt's outcome.
    PromptCompleted(PromptCompletedMarker),
    /// `_auth/status_update`: the agent's own identity. Connection-scoped, so
    /// it carries no session id and never passes the ingress fence (#4241).
    AuthStatus(AuthStatus),
    /// An `async_task_*` `session/update` of the adapter's `asyncTasks`
    /// extension, which the typed `SessionUpdate` enum has no variant for.
    AsyncTask(AsyncTaskNotification),
}

/// A session-scoped `async_task_*` update, reduced to what aoe tracks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct AsyncTaskNotification {
    pub(super) session_id: SessionId,
    update: AsyncTaskUpdate,
    /// The frame as received, for re-serialization.
    raw: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AsyncTaskUpdate {
    Spawned {
        task_id: String,
        tool_call_id: Option<String>,
        description: String,
    },
    Finished {
        task_id: String,
    },
    /// Progress or a non-terminal state: nothing to track.
    Ignored,
}

/// A session-scoped update that waits for native identity: an ordinary typed
/// update or an `async_task_*` frame.
#[derive(Clone, Debug)]
pub(super) enum IngressUpdate {
    Session(Box<SessionNotification>),
    Task(AsyncTaskNotification),
}

impl IngressUpdate {
    fn session_id(&self) -> &SessionId {
        match self {
            Self::Session(notification) => &notification.session_id,
            Self::Task(task) => &task.session_id,
        }
    }

    fn serialize_into(&self, writer: impl std::io::Write) -> serde_json::Result<()> {
        match self {
            Self::Session(notification) => serde_json::to_writer(writer, notification),
            Self::Task(task) => serde_json::to_writer(writer, &task.raw),
        }
    }
}

impl From<SessionNotification> for IngressUpdate {
    fn from(notification: SessionNotification) -> Self {
        Self::Session(Box::new(notification))
    }
}

impl AsyncTaskNotification {
    /// The daemon control reader's byte count for this frame, absent on stdio.
    pub(super) fn wire_bytes(&self) -> Option<usize> {
        self.raw
            .get(CONTROL_FRAME_BYTES_FIELD)
            .and_then(serde_json::Value::as_u64)
            .and_then(|bytes| usize::try_from(bytes).ok())
    }

    pub(super) fn into_event(self) -> Option<Event> {
        match self.update {
            AsyncTaskUpdate::Spawned {
                task_id,
                tool_call_id,
                description,
            } => Some(Event::BackgroundTaskStarted {
                task_id,
                tool_call_id,
                description,
            }),
            AsyncTaskUpdate::Finished { task_id } => {
                Some(Event::BackgroundTaskFinished { task_id })
            }
            AsyncTaskUpdate::Ignored => None,
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AsyncTaskParams {
    session_id: SessionId,
    update: AsyncTaskWire,
}

#[derive(serde::Deserialize)]
#[serde(tag = "sessionUpdate", rename_all_fields = "camelCase")]
enum AsyncTaskWire {
    #[serde(rename = "async_task_spawned")]
    Spawned {
        async_task_id: String,
        #[serde(default)]
        tool_call_id: Option<String>,
        #[serde(default)]
        description: Option<String>,
        #[serde(default)]
        name: Option<String>,
    },
    #[serde(rename = "async_task_progress")]
    Progress {},
    #[serde(rename = "async_task_state_update")]
    StateUpdate {
        async_task_id: String,
        state: String,
    },
}

const ASYNC_TASK_UPDATE_PREFIX: &str = "async_task_";

/// `session/update` params whose update is an `async_task_*` frame. The
/// daemon's control relay adds its byte-count field beside `update`, which
/// the typed parse below ignores.
fn is_async_task_update(params: &serde_json::Value) -> bool {
    params
        .get("update")
        .and_then(|update| update.get("sessionUpdate"))
        .and_then(serde_json::Value::as_str)
        .is_some_and(|kind| kind.starts_with(ASYNC_TASK_UPDATE_PREFIX))
}

fn parse_async_task(params: serde_json::Value) -> serde_json::Result<AsyncTaskNotification> {
    let parsed: AsyncTaskParams = serde_json::from_value(params.clone())?;
    let update = match parsed.update {
        AsyncTaskWire::Spawned {
            async_task_id,
            tool_call_id,
            description,
            name,
        } => AsyncTaskUpdate::Spawned {
            description: description.or(name).unwrap_or_default(),
            task_id: async_task_id,
            tool_call_id,
        },
        AsyncTaskWire::StateUpdate {
            async_task_id,
            state,
        } if !matches!(state.as_str(), "running" | "paused") => AsyncTaskUpdate::Finished {
            task_id: async_task_id,
        },
        AsyncTaskWire::StateUpdate { .. } | AsyncTaskWire::Progress {} => AsyncTaskUpdate::Ignored,
    };
    Ok(AsyncTaskNotification {
        session_id: parsed.session_id,
        update,
        raw: params,
    })
}

/// Params of `_auth/status_update`. Unknown fields, including the upstream
/// `vendor` bag, are dropped here rather than persisted.
#[derive(serde::Deserialize)]
struct AuthStatusParams {
    #[serde(rename = "authStatus")]
    auth_status: AuthStatus,
}

impl JsonRpcMessage for SessionIngressNotification {
    fn matches_method(method: &str) -> bool {
        SessionNotification::matches_method(method)
            || SessionReplayed::matches_method(method)
            || PromptCompletedMarker::matches_method(method)
            || method == AUTH_STATUS_UPDATE_METHOD
    }

    fn method(&self) -> &str {
        match self {
            Self::Update(_) => "session/update",
            Self::Replayed(marker) => marker.method(),
            Self::PromptCompleted(marker) => marker.method(),
            Self::AuthStatus(_) => AUTH_STATUS_UPDATE_METHOD,
            Self::AsyncTask(_) => "session/update",
        }
    }

    fn to_untyped_message(&self) -> agent_client_protocol::Result<UntypedMessage> {
        match self {
            Self::Update(params) => UntypedMessage::new(self.method(), params),
            Self::Replayed(marker) => marker.to_untyped_message(),
            Self::PromptCompleted(marker) => marker.to_untyped_message(),
            Self::AuthStatus(status) => {
                UntypedMessage::new(self.method(), serde_json::json!({ "authStatus": status }))
            }
            Self::AsyncTask(task) => UntypedMessage::new(self.method(), &task.raw),
        }
    }

    fn parse_message(
        method: &str,
        params: &impl serde::Serialize,
    ) -> agent_client_protocol::Result<Self> {
        if SessionReplayed::matches_method(method) {
            return Ok(Self::Replayed(SessionReplayed::parse_message(
                method, params,
            )?));
        }
        if PromptCompletedMarker::matches_method(method) {
            return Ok(Self::PromptCompleted(PromptCompletedMarker::parse_message(
                method, params,
            )?));
        }
        if method == AUTH_STATUS_UPDATE_METHOD {
            let parsed: AuthStatusParams = serde_json::from_value(serde_json::to_value(params)?)?;
            return Ok(Self::AuthStatus(parsed.auth_status));
        }
        if !SessionNotification::matches_method(method) {
            return Err(agent_client_protocol::Error::method_not_found());
        }
        let params = serde_json::to_value(params)?;
        if is_async_task_update(&params) {
            return Ok(Self::AsyncTask(parse_async_task(params)?));
        }
        Ok(Self::Update(params))
    }
}

impl JsonRpcNotification for SessionIngressNotification {}

impl SessionIngressNotification {
    /// Decode a `session/update` payload, splitting off the daemon reader's
    /// control frame byte count.
    pub(super) fn decode_update(
        mut params: serde_json::Value,
        control_transport: bool,
    ) -> agent_client_protocol::Result<(SessionNotification, Option<usize>)> {
        let invalid = |error: serde_json::Error| {
            agent_client_protocol::Error::invalid_params().data(error.to_string())
        };
        let wire_bytes = if control_transport {
            let claim = match &mut params {
                serde_json::Value::Object(fields) => fields.remove(CONTROL_FRAME_BYTES_FIELD),
                serde_json::Value::Array(fields) => fields.pop(),
                _ => None,
            }
            .ok_or_else(|| {
                agent_client_protocol::Error::invalid_params()
                    .data("missing control frame byte count")
            })?;
            Some(serde_json::from_value(claim).map_err(invalid)?)
        } else {
            None
        };
        Ok((serde_json::from_value(params).map_err(invalid)?, wire_bytes))
    }
}

#[derive(Default)]
pub(super) struct SessionIngress {
    state: StateMutex<IdentityState>,
    pub(super) fence: Mutex<()>,
    changed: Notify,
    failure: Notify,
}

#[derive(Default)]
struct IdentityState {
    current: Option<SessionId>,
    pending: Option<Establishment>,
    generation: u64,
    failed: bool,
}

struct Establishment {
    generation: u64,
    candidate: Option<SessionId>,
    updates: Vec<IngressUpdate>,
    encoded_bytes: usize,
}

/// Count encoded pending payload without retaining a second copy.
struct PendingByteCount {
    written: usize,
    limit: usize,
}

impl std::io::Write for PendingByteCount {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.written = self
            .written
            .checked_add(bytes.len())
            .filter(|written| *written <= self.limit)
            .ok_or(std::io::ErrorKind::WriteZero)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(PartialEq)]
enum RequestAdmission {
    Reject,
    AwaitCommit,
    Active,
}

impl SessionIngress {
    pub(super) fn new(current: Option<SessionId>) -> Self {
        Self {
            state: StateMutex::new(IdentityState {
                current,
                ..IdentityState::default()
            }),
            ..Self::default()
        }
    }

    pub(super) fn current(&self) -> Option<SessionId> {
        self.state.lock().unwrap().current.clone()
    }

    /// Called under the mutation fence, or before the connection starts.
    pub(super) fn begin(&self) -> u64 {
        let mut state = self.state.lock().unwrap();
        state.generation += 1;
        let generation = state.generation;
        state.pending = Some(Establishment {
            generation,
            candidate: None,
            updates: Vec::new(),
            encoded_bytes: 0,
        });
        generation
    }

    /// Publish a response fact under the wire's ordering barrier, not an update's ID.
    pub(super) fn resolve(&self, generation: Option<u64>, id: SessionId) {
        let mut state = self.state.lock().unwrap();
        if !state.failed {
            if let Some(pending) = state.pending.as_mut() {
                if generation.is_none_or(|generation| generation == pending.generation) {
                    pending.candidate = Some(id);
                }
            }
        }
    }

    /// Caller retains the fence through boundary publication and returned replay.
    pub(super) fn finish(
        &self,
        id: Option<SessionId>,
    ) -> agent_client_protocol::Result<Vec<IngressUpdate>> {
        let mut state = self.state.lock().unwrap();
        if state.failed {
            return Err(Self::overflow_error());
        }
        let mut updates = state
            .pending
            .take()
            .map(|pending| pending.updates)
            .unwrap_or_default();
        updates.retain(|update| Some(update.session_id()) == id.as_ref());
        state.current = id;
        drop(state);
        self.changed.notify_waiters();
        Ok(updates)
    }

    /// Hold the admission fence through effects, retaining bytes across both checks.
    pub(super) async fn notification(
        &self,
        notification: SessionNotification,
        wire_bytes: Option<usize>,
    ) -> agent_client_protocol::Result<Option<(SessionNotification, MutexGuard<'_, ()>)>> {
        Ok(self
            .admit(notification.into(), wire_bytes)
            .await?
            .and_then(|(update, guard)| match update {
                IngressUpdate::Session(notification) => Some((*notification, guard)),
                IngressUpdate::Task(_) => None,
            }))
    }

    /// An `async_task_*` frame takes the same pending buffer and identity
    /// fence as an ordinary update, so one sent between `SessionReady` and the
    /// identity commit is replayed at the commit instead of dropped.
    pub(super) async fn task(
        &self,
        task: AsyncTaskNotification,
        control_transport: bool,
    ) -> agent_client_protocol::Result<Option<(AsyncTaskNotification, MutexGuard<'_, ()>)>> {
        // Only the daemon's control reader writes the byte count.
        let wire_bytes = control_transport.then(|| task.wire_bytes()).flatten();
        Ok(self
            .admit(IngressUpdate::Task(task), wire_bytes)
            .await?
            .and_then(|(update, guard)| match update {
                IngressUpdate::Task(task) => Some((task, guard)),
                IngressUpdate::Session(_) => None,
            }))
    }

    async fn admit(
        &self,
        update: IngressUpdate,
        wire_bytes: Option<usize>,
    ) -> agent_client_protocol::Result<Option<(IngressUpdate, MutexGuard<'_, ()>)>> {
        let Some(update) = self.route(update, wire_bytes)? else {
            return Ok(None);
        };
        let guard = self.fence.lock().await;
        Ok(self
            .route(update, wire_bytes)?
            .map(|update| (update, guard)))
    }

    fn route(
        &self,
        notification: IngressUpdate,
        wire_bytes: Option<usize>,
    ) -> agent_client_protocol::Result<Option<IngressUpdate>> {
        let mut state = self.state.lock().unwrap();
        if state.failed {
            return Ok(None);
        }
        if let Some(pending) = state.pending.as_mut() {
            // No encoded-size work on the established per-token path.
            let size = if pending.updates.len() >= MAX_PENDING_UPDATES {
                None
            } else if let Some(size) = wire_bytes {
                (size <= MAX_PENDING_BYTES - pending.encoded_bytes).then_some(size)
            } else {
                // Stdio has no detached control queue; bound its typed payload.
                let mut count = PendingByteCount {
                    written: 0,
                    limit: MAX_PENDING_BYTES - pending.encoded_bytes,
                };
                notification
                    .serialize_into(&mut count)
                    .ok()
                    .map(|()| count.written)
            };
            let Some(size) = size else {
                pending.updates.clear();
                pending.encoded_bytes = 0;
                state.failed = true;
                drop(state);
                self.changed.notify_waiters();
                self.failure.notify_one();
                return Err(Self::overflow_error());
            };
            pending.encoded_bytes += size;
            pending.updates.push(notification);
            return Ok(None);
        }
        Ok((state.current.as_ref() == Some(notification.session_id())).then_some(notification))
    }

    fn request_admission(&self, id: &SessionId) -> RequestAdmission {
        let state = self.state.lock().unwrap();
        if state.failed {
            return RequestAdmission::Reject;
        }
        if let Some(pending) = state.pending.as_ref() {
            return if pending.candidate.as_ref() == Some(id) {
                RequestAdmission::AwaitCommit
            } else {
                RequestAdmission::Reject
            };
        }
        if state.current.as_ref() == Some(id) {
            RequestAdmission::Active
        } else {
            RequestAdmission::Reject
        }
    }

    pub(super) async fn request(
        &self,
        id: &SessionId,
    ) -> agent_client_protocol::Result<MutexGuard<'_, ()>> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            match self.request_admission(id) {
                RequestAdmission::Reject => {
                    let mut error = agent_client_protocol::Error::invalid_params();
                    error.message = "sessionId does not identify the active native session".into();
                    return Err(error);
                }
                RequestAdmission::AwaitCommit => changed.await,
                RequestAdmission::Active => {
                    let guard = self.fence.lock().await;
                    if self.request_admission(id) == RequestAdmission::Active {
                        return Ok(guard);
                    }
                    drop(guard);
                }
            }
        }
    }

    pub(super) async fn failed(&self) -> agent_client_protocol::Error {
        loop {
            let failed = self.failure.notified();
            tokio::pin!(failed);
            failed.as_mut().enable();
            if self.state.lock().unwrap().failed {
                return Self::overflow_error();
            }
            failed.await;
        }
    }

    fn overflow_error() -> agent_client_protocol::Error {
        acp_internal_error("pending native session updates exceed replay capacity".into())
    }
}

struct CancelOrderedRequest {
    connection: ConnectionTo<Agent>,
    id: RequestId,
    settled: Arc<AtomicBool>,
}

impl Drop for CancelOrderedRequest {
    fn drop(&mut self) {
        if !self.settled.swap(true, Ordering::AcqRel) {
            let _ = self.connection.send_cancel_request(self.id.clone());
        }
    }
}

/// Preserve response-before-callback ordering and the foreground waiter's cancellation.
pub(super) async fn ordered_session_request<Req>(
    connection: &ConnectionTo<Agent>,
    ingress: &Arc<SessionIngress>,
    generation: u64,
    request: Req,
    native_id: fn(&Req::Response) -> SessionId,
) -> agent_client_protocol::Result<Req::Response>
where
    Req: JsonRpcRequest,
    Req::Response: Send + 'static,
{
    let sent = connection.send_request(request);
    let settled = Arc::new(AtomicBool::new(false));
    let _cancel = CancelOrderedRequest {
        connection: connection.clone(),
        id: sent.id().clone(),
        settled: settled.clone(),
    };
    let ingress = ingress.clone();
    let (tx, rx) = oneshot::channel();
    sent.on_receiving_result(move |result| async move {
        settled.store(true, Ordering::Release);
        if let Ok(response) = result.as_ref() {
            ingress.resolve(Some(generation), native_id(response));
        }
        let _ = tx.send(result);
        Ok(())
    })?;
    rx.await
        .map_err(|_| acp_internal_error("session response waiter closed".into()))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::acp_client::test_helpers::text_chunk;
    use crate::acp::state::AuthStatusKind;

    fn notif(id: &str) -> SessionNotification {
        SessionNotification::new(id.to_string(), text_chunk("x", None))
    }

    #[test]
    fn auth_status_update_parses_without_a_session_id() {
        // Table: the wire payloads the extension can send, including a kind
        // added after this code was written and the vendor bag it drops.
        let cases = [
            (
                "subscription",
                serde_json::json!({"authStatus": {
                    "kind": "account",
                    "label": "Claude Max",
                    "account": {"email": "a@b.co", "organization": "Acme", "plan": "max"},
                }}),
                AuthStatusKind::Account,
                "Claude Max",
            ),
            (
                "api key, vendor bag dropped",
                serde_json::json!({"authStatus": {
                    "kind": "api_key",
                    "label": "Anthropic API key",
                    "detail": "apiKeyHelper",
                    "vendor": {"claudeCode": {"anything": [1, 2, 3]}},
                }}),
                AuthStatusKind::ApiKey,
                "Anthropic API key",
            ),
            (
                "logged out",
                serde_json::json!({"authStatus": {"kind": "none", "label": "Not logged in"}}),
                AuthStatusKind::None,
                "Not logged in",
            ),
            (
                "kind added upstream later",
                serde_json::json!({"authStatus": {"kind": "quantum", "label": "Future Auth"}}),
                AuthStatusKind::Unknown,
                "Future Auth",
            ),
        ];
        for (name, params, kind, label) in cases {
            let parsed =
                SessionIngressNotification::parse_message(AUTH_STATUS_UPDATE_METHOD, &params)
                    .unwrap_or_else(|e| panic!("{name}: {e}"));
            let SessionIngressNotification::AuthStatus(status) = parsed else {
                panic!("{name}: expected AuthStatus");
            };
            assert_eq!(status.kind, kind, "{name}");
            assert_eq!(status.label, label, "{name}");
        }
        assert!(SessionIngressNotification::matches_method(
            AUTH_STATUS_UPDATE_METHOD
        ));
    }

    #[test]
    fn async_task_updates_parse_into_task_events() {
        let frame = |update: serde_json::Value| serde_json::json!({"sessionId": "s-1", "update": update, CONTROL_FRAME_BYTES_FIELD: 120});
        let cases = [
            (
                "spawned",
                frame(serde_json::json!({
                    "sessionUpdate": "async_task_spawned",
                    "asyncTaskId": "t-1",
                    "name": "Run delayed echo",
                    "taskType": "shell",
                    "description": "Run delayed echo in background",
                    "showInTranscript": false,
                    "canStop": true,
                    "toolCallId": "toolu_1",
                })),
                Some(Event::BackgroundTaskStarted {
                    task_id: "t-1".into(),
                    tool_call_id: Some("toolu_1".into()),
                    description: "Run delayed echo in background".into(),
                }),
            ),
            (
                "progress",
                frame(serde_json::json!({
                    "sessionUpdate": "async_task_progress",
                    "asyncTaskId": "t-1",
                    "outputFilePath": "/tmp/t-1.output",
                })),
                None,
            ),
            (
                "still running",
                frame(serde_json::json!({
                    "sessionUpdate": "async_task_state_update",
                    "asyncTaskId": "t-1",
                    "state": "running",
                })),
                None,
            ),
            (
                "completed",
                frame(serde_json::json!({
                    "sessionUpdate": "async_task_state_update",
                    "asyncTaskId": "t-1",
                    "state": "completed",
                })),
                Some(Event::BackgroundTaskFinished {
                    task_id: "t-1".into(),
                }),
            ),
            (
                "failed",
                frame(serde_json::json!({
                    "sessionUpdate": "async_task_state_update",
                    "asyncTaskId": "t-1",
                    "state": "failed",
                    "summary": "exit 1",
                })),
                Some(Event::BackgroundTaskFinished {
                    task_id: "t-1".into(),
                }),
            ),
        ];
        for (name, params, want) in cases {
            let parsed = SessionIngressNotification::parse_message("session/update", &params)
                .unwrap_or_else(|e| panic!("{name}: {e}"));
            let SessionIngressNotification::AsyncTask(task) = parsed else {
                panic!("{name}: expected AsyncTask, got {parsed:?}");
            };
            assert_eq!(task.session_id, SessionId::from("s-1"), "{name}");
            assert_eq!(
                task.into_event().map(|e| serde_json::to_value(e).unwrap()),
                want.map(|e| serde_json::to_value(e).unwrap()),
                "{name}"
            );
        }
    }

    fn task_frame(session: &str, update: serde_json::Value) -> AsyncTaskNotification {
        parse_async_task(serde_json::json!({"sessionId": session, "update": update}))
            .expect("task frame parses")
    }

    fn spawned(session: &str, id: &str) -> AsyncTaskNotification {
        task_frame(
            session,
            serde_json::json!({"sessionUpdate": "async_task_spawned", "asyncTaskId": id}),
        )
    }

    fn completed(session: &str, id: &str) -> AsyncTaskNotification {
        task_frame(
            session,
            serde_json::json!({
                "sessionUpdate": "async_task_state_update",
                "asyncTaskId": id,
                "state": "completed",
            }),
        )
    }

    /// A runner sends `SessionReady` ahead of the identity commit, so a task
    /// start or finish arriving in between must wait for the commit.
    #[test]
    fn task_frames_before_the_identity_commit_replay_in_order() {
        let ingress = SessionIngress::default();
        ingress.begin();
        ingress.resolve(None, SessionId::from("s"));
        for task in [
            spawned("s", "t-1"),
            spawned("other", "t-9"),
            completed("s", "t-1"),
        ] {
            assert!(ingress
                .route(IngressUpdate::Task(task), None)
                .unwrap()
                .is_none());
        }

        let replay = ingress.finish(Some(SessionId::from("s"))).unwrap();
        let events: Vec<serde_json::Value> = replay
            .into_iter()
            .map(|update| match update {
                IngressUpdate::Task(task) => {
                    serde_json::to_value(task.into_event().unwrap()).unwrap()
                }
                IngressUpdate::Session(_) => panic!("only task frames were sent"),
            })
            .collect();
        assert_eq!(
            events,
            [
                serde_json::json!({"BackgroundTaskStarted": {"task_id": "t-1", "description": ""}}),
                serde_json::json!({"BackgroundTaskFinished": {"task_id": "t-1"}}),
            ]
        );
    }

    #[tokio::test]
    async fn task_frames_after_the_commit_pass_only_for_the_current_session() {
        let ingress = SessionIngress::new(Some(SessionId::from("s")));
        assert!(ingress
            .task(spawned("s", "t-1"), false)
            .await
            .unwrap()
            .is_some());
        assert!(ingress
            .task(spawned("other", "t-2"), false)
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn pending_replay_and_wire_budget_are_bounded() {
        let ingress = SessionIngress::new(Some(SessionId::from("s")));
        let guard = ingress.fence.lock().await;
        let mut admission =
            std::pin::pin!(ingress.notification(notif("s"), Some(MAX_PENDING_BYTES / 2)));
        assert!(futures_util::poll!(admission.as_mut()).is_pending());
        ingress.begin();
        drop(guard);
        assert!(admission.await.unwrap().is_none());
        assert!(ingress
            .notification(notif("s"), Some(MAX_PENDING_BYTES / 2))
            .await
            .unwrap()
            .is_none());
        assert!(ingress.notification(notif("s"), Some(1)).await.is_err());
        assert!(futures_util::poll!(std::pin::pin!(ingress.failed())).is_ready());
        assert!(ingress.finish(Some(SessionId::from("s"))).is_err());

        // The budget accepts its exact ceiling.
        {
            let ingress = SessionIngress::default();
            ingress.begin();
            for _ in 0..2 {
                ingress
                    .route(notif("s").into(), Some(MAX_PENDING_BYTES / 2))
                    .unwrap();
            }
            assert_eq!(ingress.finish(Some(SessionId::from("s"))).unwrap().len(), 2);
        }

        {
            let ingress = SessionIngress::default();
            ingress.begin();
            for _ in 0..MAX_CONTROL_QUEUE_FRAMES {
                ingress
                    .route(notif("s").into(), None)
                    .expect("a full detach-queue backlog fits");
            }
            let replay = ingress
                .finish(Some(SessionId::from("s")))
                .expect("commit succeeds");
            assert_eq!(replay.len(), MAX_CONTROL_QUEUE_FRAMES);

            // Past that contract the buffer stays bounded: an overflow fails the
            // attach rather than growing without limit.
            {
                let ingress = SessionIngress::default();
                ingress.begin();
                for _ in 0..MAX_CONTROL_QUEUE_FRAMES {
                    ingress
                        .route(notif("s").into(), None)
                        .expect("under the cap");
                }
                assert!(ingress.route(notif("s").into(), None).is_err());
            }
        }
    }
}
