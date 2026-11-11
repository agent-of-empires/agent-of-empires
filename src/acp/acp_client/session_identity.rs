//! Native session admission, selected only by the connection lifecycle.

use agent_client_protocol::schema::MaybeUndefined;
use agent_client_protocol::v1::schema::{
    RequestId, SessionId, SessionNotification, SessionUpdate, SubagentUpdate,
};
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::{
    Agent, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, UntypedMessage,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex as StateMutex;
use tokio::sync::{oneshot, Mutex, MutexGuard, Notify};

use super::errors::acp_internal_error;
use crate::acp::control_protocol::{
    PromptCompletedMarker, SessionReplayed, MAX_CONTROL_QUEUE_BYTES, MAX_CONTROL_QUEUE_FRAMES,
};
use crate::acp::state::{AuthStatus, AUTH_STATUS_UPDATE_METHOD};

// The replayed backlog a reattach flushes is exactly the runner's detached
// control queue, so this buffer is sized against the same contract: a
// legitimate reattach must accept the whole flushed queue rather than be
// killed for a backlog the runner was entitled to hold. Charge original control
// frame bytes: typed schema serialization can expand the native representation.
// Direct stdio has no detached queue and bounds its typed notification payload.
const MAX_PENDING_UPDATES: usize = MAX_CONTROL_QUEUE_FRAMES;
const MAX_PENDING_BYTES: usize = MAX_CONTROL_QUEUE_BYTES;

// Bounds the child routes one connection can admit.
const MAX_CHILDREN: usize = 256;

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
        }
    }

    fn parse_message(
        method: &str,
        params: &serde_json::Value,
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
            let parsed: AuthStatusParams = serde_json::from_value(params.clone())?;
            return Ok(Self::AuthStatus(parsed.auth_status));
        }
        if !SessionNotification::matches_method(method) {
            return Err(agent_client_protocol::Error::method_not_found());
        }
        Ok(Self::Update(params.clone()))
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
    /// Requests awaiting the human with the fence released.
    parked: AtomicUsize,
    changed: Notify,
    failure: Notify,
}

#[derive(Default)]
struct IdentityState {
    current: Option<SessionId>,
    /// Subagent sessions announced by an admitted session. Routing metadata
    /// only; titles and state live in the reduced event state.
    children: HashMap<SessionId, ChildRoute>,
    pending: Option<Establishment>,
    generation: u64,
    failed: bool,
    /// Adapter frames dropped for another session id. They still hold a
    /// position in the proxy's connection-wide numbering.
    discarded_adapter_frames: u64,
}

fn is_adapter_frame(notification: &SessionNotification) -> bool {
    !serde_json::to_value(&notification.update)
        .is_ok_and(|update| crate::acp::aoe_meta::is_anchored(&update))
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ChildRoute {
    pub(super) parent: SessionId,
    pub(super) cancel_capable: bool,
}

/// Which sessions a request handler may act for.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum RequestPolicy {
    /// Handlers that resolve paths or processes against the root session's
    /// own workspace.
    ParentOnly,
    ParentOrChild,
}

/// An admitted session's `subagent_update` admits the child it names, so a
/// child cannot be routable before its parent announced it, and an announcement
/// cannot re-parent or loop the tree.
fn register_child(
    children: &mut HashMap<SessionId, ChildRoute>,
    root: Option<&SessionId>,
    notification: &SessionNotification,
) {
    let SessionUpdate::SubagentUpdate(update) = &notification.update else {
        return;
    };
    let parent = &notification.session_id;
    let child = &update.session_id;
    if Some(parent) != root && !children.contains_key(parent) {
        return;
    }
    if child == parent || Some(child) == root {
        return;
    }
    match children.get(child) {
        Some(existing) if existing.parent != *parent => return,
        None if children.len() >= MAX_CHILDREN => {
            tracing::warn!(
                target: "acp.protocol",
                child = %child.0,
                limit = MAX_CHILDREN,
                "subagent route limit reached; ignoring announcement"
            );
            return;
        }
        _ => {}
    }
    let route = children.entry(child.clone()).or_insert_with(|| ChildRoute {
        parent: parent.clone(),
        cancel_capable: false,
    });
    route.cancel_capable = cancel_capable(update, route.cancel_capable);
}

fn cancel_capable(update: &SubagentUpdate, current: bool) -> bool {
    match &update.capabilities {
        MaybeUndefined::Undefined => current,
        MaybeUndefined::Null => false,
        MaybeUndefined::Value(capabilities) => capabilities.cancel.is_some(),
    }
}

struct Establishment {
    generation: u64,
    candidate: Option<SessionId>,
    updates: Vec<SessionNotification>,
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
    /// A registered child asked for something only the root session may do.
    RootOnly,
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
    ) -> agent_client_protocol::Result<Vec<SessionNotification>> {
        let mut state = self.state.lock().unwrap();
        if state.failed {
            return Err(Self::overflow_error());
        }
        let pending = state
            .pending
            .take()
            .map(|pending| pending.updates)
            .unwrap_or_default();
        // A failed reset keeps its session, so its children stay routable.
        let mut children = if state.current == id {
            std::mem::take(&mut state.children)
        } else {
            HashMap::new()
        };
        // Wire order: an announcement admits the child frames after it.
        let mut updates = Vec::with_capacity(pending.len());
        let mut discarded = 0;
        for notification in pending {
            if Some(&notification.session_id) == id.as_ref()
                || children.contains_key(&notification.session_id)
            {
                register_child(&mut children, id.as_ref(), &notification);
                updates.push(notification);
            } else if is_adapter_frame(&notification) {
                discarded += 1;
            }
        }
        state.discarded_adapter_frames += discarded;
        state.current = id;
        state.children = children;
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
        let Some(notification) = self.route(notification, wire_bytes)? else {
            return Ok(None);
        };
        let guard = self.fence.lock().await;
        Ok(self
            .route(notification, wire_bytes)?
            .map(|notification| (notification, guard)))
    }

    fn route(
        &self,
        notification: SessionNotification,
        wire_bytes: Option<usize>,
    ) -> agent_client_protocol::Result<Option<SessionNotification>> {
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
                serde_json::to_writer(&mut count, &notification)
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
        if state.current.as_ref() != Some(&notification.session_id)
            && !state.children.contains_key(&notification.session_id)
        {
            if is_adapter_frame(&notification) {
                state.discarded_adapter_frames += 1;
            }
            return Ok(None);
        }
        // Registered under the state lock so the next child frame cannot
        // reach the fence before its route exists.
        let IdentityState {
            current, children, ..
        } = &mut *state;
        register_child(children, current.as_ref(), &notification);
        Ok(Some(notification))
    }

    /// The adapter frames dropped since the last call.
    pub(super) fn take_discarded_adapter_frames(&self) -> u64 {
        std::mem::take(&mut self.state.lock().unwrap().discarded_adapter_frames)
    }

    /// Route of a subagent session, valid while the caller holds the fence.
    pub(super) fn child_route(&self, id: &SessionId) -> Option<ChildRoute> {
        self.state.lock().unwrap().children.get(id).cloned()
    }

    fn request_admission(&self, id: &SessionId, policy: RequestPolicy) -> RequestAdmission {
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
        } else if state.children.contains_key(id) {
            match policy {
                RequestPolicy::ParentOrChild => RequestAdmission::Active,
                RequestPolicy::ParentOnly => RequestAdmission::RootOnly,
            }
        } else {
            RequestAdmission::Reject
        }
    }

    /// Mark a request as waiting on the human. Call while holding the fence,
    /// so a transition that takes the fence next sees it.
    pub(super) fn park(&self) -> ParkedRequest<'_> {
        self.parked.fetch_add(1, Ordering::AcqRel);
        ParkedRequest(self)
    }

    pub(super) fn has_parked(&self) -> bool {
        self.parked.load(Ordering::Acquire) > 0
    }

    /// Whether `id` is still admitted, for a handler that released the fence
    /// across a wait and must not answer for a replaced session.
    pub(super) fn still_admitted(&self, id: &SessionId, policy: RequestPolicy) -> bool {
        self.request_admission(id, policy) == RequestAdmission::Active
    }

    pub(super) async fn request(
        &self,
        id: &SessionId,
        policy: RequestPolicy,
    ) -> agent_client_protocol::Result<MutexGuard<'_, ()>> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            match self.request_admission(id, policy) {
                RequestAdmission::Reject => {
                    let mut error = agent_client_protocol::Error::invalid_params();
                    error.message = "sessionId does not identify the active native session".into();
                    return Err(error);
                }
                RequestAdmission::RootOnly => {
                    let mut error = agent_client_protocol::Error::invalid_params();
                    error.message = "subagent sessions cannot make this request".into();
                    return Err(error);
                }
                RequestAdmission::AwaitCommit => changed.await,
                RequestAdmission::Active => {
                    let guard = self.fence.lock().await;
                    if self.request_admission(id, policy) == RequestAdmission::Active {
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

pub(super) struct ParkedRequest<'a>(&'a SessionIngress);

impl Drop for ParkedRequest<'_> {
    fn drop(&mut self) {
        self.0.parked.fetch_sub(1, Ordering::AcqRel);
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
    fn cancel_capable_follows_the_capabilities_field() {
        // Table: absent keeps the current value, null clears it, an object
        // is capable only when it carries `cancel`.
        let cases = [
            ("undefined keeps true", None, true, true),
            ("undefined keeps false", None, false, false),
            (
                "null clears true",
                Some(serde_json::Value::Null),
                true,
                false,
            ),
            (
                "null keeps false",
                Some(serde_json::Value::Null),
                false,
                false,
            ),
            (
                "cancel sets from false",
                Some(serde_json::json!({"cancel": {}})),
                false,
                true,
            ),
            (
                "cancel keeps true",
                Some(serde_json::json!({"cancel": {}})),
                true,
                true,
            ),
            (
                "no cancel clears true",
                Some(serde_json::json!({})),
                true,
                false,
            ),
            (
                "no cancel keeps false",
                Some(serde_json::json!({})),
                false,
                false,
            ),
        ];
        for (name, capabilities, current, expected) in cases {
            let mut update = serde_json::json!({"sessionId": "child"});
            if let Some(capabilities) = capabilities {
                update["capabilities"] = capabilities;
            }
            let update: SubagentUpdate = serde_json::from_value(update).unwrap();
            assert_eq!(cancel_capable(&update, current), expected, "{name}");
        }
    }

    #[test]
    fn dropped_old_session_frames_keep_their_adapter_position() {
        let ingress = SessionIngress::default();
        ingress
            .finish(Some(SessionId::from("old".to_string())))
            .unwrap();
        ingress.begin();
        // Buffered during the reset, then dropped at the commit.
        assert!(ingress.route(notif("old"), None).unwrap().is_none());
        let mut tailer = notif("old");
        let anchored =
            serde_json::json!({"aoe": {"v": 1, "key": "t", "after_adapter_sequence": 2}});
        if let agent_client_protocol::v1::schema::SessionUpdate::AgentMessageChunk(chunk) =
            &mut tailer.update
        {
            chunk.meta = anchored.as_object().cloned();
        }
        assert!(ingress.route(tailer, None).unwrap().is_none());
        ingress
            .finish(Some(SessionId::from("new".to_string())))
            .unwrap();
        assert_eq!(
            ingress.take_discarded_adapter_frames(),
            1,
            "tailer frames take no number"
        );
        // A late old-session frame is dropped as well.
        assert!(ingress.route(notif("old"), None).unwrap().is_none());
        assert_eq!(ingress.take_discarded_adapter_frames(), 1);
        assert_eq!(ingress.take_discarded_adapter_frames(), 0, "drained");
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
                    .route(notif("s"), Some(MAX_PENDING_BYTES / 2))
                    .unwrap();
            }
            assert_eq!(ingress.finish(Some(SessionId::from("s"))).unwrap().len(), 2);
        }

        {
            let ingress = SessionIngress::default();
            ingress.begin();
            for _ in 0..MAX_CONTROL_QUEUE_FRAMES {
                ingress
                    .route(notif("s"), None)
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
                    ingress.route(notif("s"), None).expect("under the cap");
                }
                assert!(ingress.route(notif("s"), None).is_err());
            }
        }
    }

    fn announce(parent: &str, child: &str) -> SessionNotification {
        serde_json::from_value(serde_json::json!({
            "sessionId": parent,
            "update": { "sessionUpdate": "subagent_update", "sessionId": child },
        }))
        .expect("subagent_update")
    }

    async fn admitted(ingress: &SessionIngress, n: SessionNotification) -> bool {
        ingress.notification(n, None).await.unwrap().is_some()
    }

    #[tokio::test]
    async fn an_announced_child_is_routed_and_an_unannounced_one_is_not() {
        let ingress = SessionIngress::new(Some(SessionId::from("root")));
        assert!(!admitted(&ingress, notif("c1")).await);
        assert!(admitted(&ingress, announce("root", "c1")).await);
        assert!(admitted(&ingress, notif("c1")).await);
        // Grandchildren hang off an admitted child; a stranger cannot adopt one.
        assert!(admitted(&ingress, announce("c1", "c2")).await);
        assert!(admitted(&ingress, notif("c2")).await);
        assert!(!admitted(&ingress, announce("stranger", "c3")).await);
        assert!(!admitted(&ingress, notif("c3")).await);
        // No re-parenting, self-parenting, or adopting the root.
        for (parent, child) in [("root", "c2"), ("c1", "c1"), ("c1", "root")] {
            admitted(&ingress, announce(parent, child)).await;
        }
        for (id, expected) in [("c2", Some("c1")), ("c1", Some("root")), ("root", None)] {
            let route = ingress.child_route(&SessionId::from(id));
            assert_eq!(
                route.map(|r| r.parent),
                expected.map(SessionId::from),
                "{id}"
            );
        }
    }

    #[tokio::test]
    async fn requests_from_a_child_obey_the_policy() {
        let ingress = SessionIngress::new(Some(SessionId::from("root")));
        let child = SessionId::from("c1");
        let err = |e: agent_client_protocol::Error| e.message;
        assert!(ingress
            .request(&child, RequestPolicy::ParentOrChild)
            .await
            .is_err());
        admitted(&ingress, announce("root", "c1")).await;
        assert!(ingress
            .request(&child, RequestPolicy::ParentOrChild)
            .await
            .is_ok());
        let refused = ingress
            .request(&child, RequestPolicy::ParentOnly)
            .await
            .map(|_| ())
            .unwrap_err();
        assert_eq!(err(refused), "subagent sessions cannot make this request");
        assert!(ingress
            .request(&SessionId::from("root"), RequestPolicy::ParentOnly)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn children_survive_a_failed_reset_and_not_a_successful_one() {
        let root = SessionId::from("root");
        let ingress = SessionIngress::new(Some(root.clone()));
        admitted(&ingress, announce("root", "c1")).await;

        // Wire order: a frame before its announcement is dropped, one after
        // it is replayed.
        ingress.begin();
        for n in [
            notif("c2"),
            announce("root", "c2"),
            notif("c2"),
            notif("c1"),
        ] {
            assert!(!admitted(&ingress, n).await);
        }
        let replay = ingress.finish(Some(root.clone())).unwrap();
        let ids: Vec<_> = replay.iter().map(|n| n.session_id.0.to_string()).collect();
        assert_eq!(ids, ["root", "c2", "c1"]);
        assert!(admitted(&ingress, notif("c1")).await);
        assert!(admitted(&ingress, notif("c2")).await);

        ingress.begin();
        ingress.finish(Some(SessionId::from("fresh"))).unwrap();
        assert!(!admitted(&ingress, notif("c1")).await);
    }
}
