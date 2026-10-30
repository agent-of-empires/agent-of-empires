//! Native session admission, selected only by the connection lifecycle.

use agent_client_protocol::schema::v1::{RequestId, SessionId, SessionNotification};
use agent_client_protocol::{
    Agent, ConnectionTo, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, UntypedMessage,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex as StateMutex;
use tokio::sync::{oneshot, Notify, RwLock, RwLockReadGuard};

use super::connection::ConnectionState;
use super::errors::{acp_internal_error, AcpError};
use crate::acp::control_protocol::{
    AdoptedPromptCompletion, NativeChannelLostMarker, PromptCompletedMarker, SessionReplayed,
    MAX_COMPLETION_MARKER_BYTES, MAX_CONTROL_QUEUE_BYTES, MAX_CONTROL_QUEUE_FRAMES,
    MAX_NATIVE_LOSS_FRAME_BYTES,
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
    /// Daemon-minted barrier committing a local or adopted prompt outcome.
    PromptCompleted(PromptCompletedMarker),
    NativeLost(NativeChannelLostMarker),
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
            || NativeChannelLostMarker::matches_method(method)
            || method == AUTH_STATUS_UPDATE_METHOD
    }

    fn method(&self) -> &str {
        match self {
            Self::Update(_) => "session/update",
            Self::Replayed(marker) => marker.method(),
            Self::PromptCompleted(marker) => marker.method(),
            Self::NativeLost(marker) => marker.method(),
            Self::AuthStatus(_) => AUTH_STATUS_UPDATE_METHOD,
        }
    }

    fn to_untyped_message(&self) -> agent_client_protocol::Result<UntypedMessage> {
        match self {
            Self::Update(params) => UntypedMessage::new(self.method(), params),
            Self::Replayed(marker) => marker.to_untyped_message(),
            Self::PromptCompleted(marker) => marker.to_untyped_message(),
            Self::NativeLost(marker) => marker.to_untyped_message(),
            Self::AuthStatus(status) => {
                UntypedMessage::new(self.method(), serde_json::json!({ "authStatus": status }))
            }
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
        if NativeChannelLostMarker::matches_method(method) {
            return Ok(Self::NativeLost(NativeChannelLostMarker::parse_message(
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
        Ok(Self::Update(serde_json::to_value(params)?))
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
    pub(super) fence: RwLock<()>,
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
    updates: Vec<PendingIngress>,
    encoded_bytes: usize,
}

/// Bootstrap updates and private barriers share one ordered, bounded replay.
/// A callback buffers rather than blocking the SDK dispatcher on a commit
/// whose handshake response may still be behind it in the same shim.
#[derive(Debug, serde::Serialize)]
pub(super) enum PendingIngress {
    Update(Box<SessionNotification>),
    Auth(AuthStatus),
    PromptCompleted(Box<AdoptedPromptCompletion>),
    NativeLost,
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
    ) -> agent_client_protocol::Result<Vec<PendingIngress>> {
        let mut state = self.state.lock().unwrap();
        if state.failed {
            return Err(Self::overflow_error());
        }
        let mut updates = state
            .pending
            .take()
            .map(|pending| pending.updates)
            .unwrap_or_default();
        updates.retain(|entry| match entry {
            PendingIngress::Update(notification) => Some(&notification.session_id) == id.as_ref(),
            PendingIngress::PromptCompleted(_)
            | PendingIngress::NativeLost
            | PendingIngress::Auth(_) => true,
        });
        state.current = id;
        drop(state);
        self.changed.notify_waiters();
        Ok(updates)
    }

    /// Return a marker only when the initial identity is already committed.
    /// Synthetic payloads are charged too, within the producer's shared replay
    /// reservation; overflow retains no partially applicable replay.
    pub(super) fn marker(
        &self,
        marker: PendingIngress,
    ) -> agent_client_protocol::Result<Option<PendingIngress>> {
        let mut state = self.state.lock().unwrap();
        if state.failed {
            return Err(Self::overflow_error());
        }
        let Some(pending) = state.pending.as_mut() else {
            return Ok(Some(marker));
        };
        let marker_limit = match &marker {
            PendingIngress::PromptCompleted(_) => MAX_COMPLETION_MARKER_BYTES,
            PendingIngress::NativeLost => MAX_NATIVE_LOSS_FRAME_BYTES,
            PendingIngress::Auth(_) => MAX_PENDING_BYTES,
            PendingIngress::Update(_) => unreachable!("updates use scoped admission"),
        };
        let mut count = PendingByteCount {
            written: 0,
            limit: marker_limit.min(MAX_PENDING_BYTES - pending.encoded_bytes),
        };
        if pending.updates.len() >= MAX_PENDING_UPDATES
            || serde_json::to_writer(&mut count, &marker).is_err()
        {
            pending.updates.clear();
            pending.encoded_bytes = 0;
            state.failed = true;
            drop(state);
            self.changed.notify_waiters();
            self.failure.notify_one();
            return Err(Self::overflow_error());
        }
        pending.encoded_bytes += count.written;
        pending.updates.push(marker);
        Ok(None)
    }

    /// Share admission through effects; identity transitions require exclusive access.
    pub(super) async fn notification(
        &self,
        notification: SessionNotification,
        wire_bytes: Option<usize>,
    ) -> agent_client_protocol::Result<Option<(SessionNotification, RwLockReadGuard<'_, ()>)>> {
        let Some(notification) = self.route(notification, wire_bytes)? else {
            return Ok(None);
        };
        let guard = self.fence.read().await;
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
            pending
                .updates
                .push(PendingIngress::Update(Box::new(notification)));
            return Ok(None);
        }
        Ok((state.current.as_ref() == Some(&notification.session_id)).then_some(notification))
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
    ) -> agent_client_protocol::Result<RwLockReadGuard<'_, ()>> {
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
                    let guard = self.fence.read().await;
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

struct OrderedRequestCompletion {
    settled: AtomicBool,
    // 0: untracked/finished, 1: waiting, 2: correlated application outcome.
    application: std::sync::atomic::AtomicU8,
    state: Arc<ConnectionState>,
}

impl OrderedRequestCompletion {
    fn correlated(&self) {
        if self.application.load(Ordering::Acquire) != 1 {
            return;
        }
        // Increment before publishing phase 2 so a racing task drop cannot
        // decrement a counter whose increment is still outstanding.
        self.state.begin_completed_outcome();
        if self
            .application
            .compare_exchange(1, 2, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            self.state.finish_completed_outcome();
        }
    }

    fn finish(&self) {
        if self.application.swap(0, Ordering::AcqRel) == 2 {
            self.state.finish_completed_outcome();
        }
    }
}

struct ApplicationOutcome(Arc<OrderedRequestCompletion>);

impl Drop for ApplicationOutcome {
    fn drop(&mut self) {
        self.0.finish();
    }
}

pub(super) struct CancelOrderedRequest {
    pub(super) connection: ConnectionTo<Agent>,
    pub(super) id: RequestId,
    completion: Arc<OrderedRequestCompletion>,
}

impl Drop for CancelOrderedRequest {
    fn drop(&mut self) {
        if !self.completion.settled.swap(true, Ordering::AcqRel) {
            let _ = self.connection.send_cancel_request(self.id.clone());
        }
    }
}

pub(super) enum OrderedRequestResult<T> {
    Correlated(agent_client_protocol::Result<T>),
    Refused(agent_client_protocol::Error),
    Unavailable(crate::acp::state::AcpConnectionUnavailability),
    Abandoned,
}

impl<T> OrderedRequestResult<T> {
    pub(super) fn into_result(self) -> agent_client_protocol::Result<T> {
        match self {
            Self::Correlated(result) => result,
            Self::Refused(error) => Err(error),
            Self::Unavailable(_) => Err(acp_internal_error("ACP connection unavailable".into())),
            Self::Abandoned => Err(acp_internal_error(
                "Request response was not observed".into(),
            )),
        }
    }
}

pub(super) struct OrderedRequest<T> {
    receiver: oneshot::Receiver<OrderedRequestResult<T>>,
    cancel: CancelOrderedRequest,
    registration: agent_client_protocol::Result<()>,
}

impl<T> OrderedRequest<T> {
    pub(super) async fn wait(self) -> OrderedRequestResult<T> {
        let _cancel = self.cancel;
        if let Err(error) = self.registration {
            return OrderedRequestResult::Refused(error);
        }
        tokio::select! {
            biased;
            result = self.receiver => result.unwrap_or(OrderedRequestResult::Abandoned),
            observation = _cancel.completion.state.wait_ordered_unavailable() => {
                OrderedRequestResult::Unavailable(observation)
            }
        }
    }
}

fn register_ordered_request<Req, Before>(
    connection: &ConnectionTo<Agent>,
    state: &Arc<ConnectionState>,
    request: Req,
    before_result: Before,
    track_application: bool,
) -> OrderedRequest<Req::Response>
where
    Req: JsonRpcRequest,
    Req::Response: Send + 'static,
    Before: FnOnce(&agent_client_protocol::Result<Req::Response>) + Send + 'static,
{
    let completion = Arc::new(OrderedRequestCompletion {
        settled: AtomicBool::new(false),
        application: std::sync::atomic::AtomicU8::new(u8::from(track_application)),
        state: Arc::clone(state),
    });
    let sent = connection.send_request(request);
    let cancel = CancelOrderedRequest {
        connection: connection.clone(),
        id: sent.id().clone(),
        completion: Arc::clone(&completion),
    };
    let (sender, receiver) = oneshot::channel();
    let registration = sent.on_receiving_result(move |result| {
        completion.settled.store(true, Ordering::Release);
        let result = if completion.state.sdk_is_closing() {
            match completion.state.loss() {
                Some(observation) => OrderedRequestResult::Unavailable(observation),
                None => OrderedRequestResult::Abandoned,
            }
        } else {
            completion.correlated();
            before_result(&result);
            OrderedRequestResult::Correlated(result)
        };
        let _ = sender.send(result);
        std::future::ready(Ok(()))
    });
    OrderedRequest {
        receiver,
        cancel,
        registration,
    }
}

pub(super) fn ordered_native_request<Req>(
    connection: &ConnectionTo<Agent>,
    state: &Arc<ConnectionState>,
    request: Req,
) -> Result<OrderedRequest<Req::Response>, AcpError>
where
    Req: JsonRpcRequest,
    Req::Response: Send + 'static,
{
    state.admit(|| register_ordered_request(connection, state, request, |_| {}, false))
}

/// Reverse callbacks belong to the SDK connection and cancel on actual loss.
/// A refused callback makes no peer decision: the closed-admission shim drops
/// any SDK cleanup response from its responder.
pub(super) fn spawn_native_callback<Handled>(
    connection: &ConnectionTo<Agent>,
    state: &Arc<ConnectionState>,
    handled: Handled,
) -> agent_client_protocol::Result<()>
where
    Handled: std::future::Future<Output = agent_client_protocol::Result<()>> + Send + 'static,
{
    match state.admit(|| {
        let cancellation = state.clone();
        connection.spawn(async move {
            tokio::select! {
                biased;
                _ = cancellation.wait_unavailable() => Ok(()),
                result = handled => result,
            }
        })
    }) {
        Ok(result) => result,
        Err(_) => Ok(()),
    }
}

pub(super) fn spawn_native_request<Req, Handle, Handled>(
    connection: &ConnectionTo<Agent>,
    state: &Arc<ConnectionState>,
    request: Req,
    handle: Handle,
) -> Result<(), AcpError>
where
    Req: JsonRpcRequest,
    Req::Response: Send + 'static,
    Handle: FnOnce(OrderedRequest<Req::Response>) -> Handled + Send + 'static,
    Handled: std::future::Future<Output = ()> + Send + 'static,
{
    state.admit(|| {
        let request = register_ordered_request(connection, state, request, |_| {}, true);
        let outcome = ApplicationOutcome(Arc::clone(&request.cancel.completion));
        connection
            .spawn(async move {
                let _outcome = outcome;
                handle(request).await;
                Ok(())
            })
            .map_err(|error| AcpError::Protocol(super::errors::public_error_message(&error)))
    })?
}

/// Preserve response-before-callback ordering and the foreground waiter's cancellation.
pub(super) async fn ordered_session_request<Req>(
    connection: &ConnectionTo<Agent>,
    ingress: &Arc<SessionIngress>,
    state: &Arc<ConnectionState>,
    generation: u64,
    request: Req,
    native_id: fn(&Req::Response) -> SessionId,
) -> agent_client_protocol::Result<Req::Response>
where
    Req: JsonRpcRequest,
    Req::Response: Send + 'static,
{
    let ingress = ingress.clone();
    let request = state
        .admit(|| {
            register_ordered_request(
                connection,
                state,
                request,
                move |result| {
                    if let Ok(response) = result {
                        ingress.resolve(Some(generation), native_id(response));
                    }
                },
                false,
            )
        })
        .map_err(|_| acp_internal_error("ACP connection unavailable".into()))?;
    match request.wait().await {
        OrderedRequestResult::Correlated(result) => result,
        OrderedRequestResult::Refused(error) => Err(error),
        OrderedRequestResult::Unavailable(_) => {
            Err(acp_internal_error("ACP connection unavailable".into()))
        }
        OrderedRequestResult::Abandoned => {
            Err(acp_internal_error("session response waiter closed".into()))
        }
    }
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

    #[tokio::test]
    async fn pending_replay_and_wire_budget_are_bounded() {
        let ingress = SessionIngress::new(Some(SessionId::from("s")));
        let guard = ingress.fence.write().await;
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

    #[tokio::test]
    async fn admitted_callbacks_share_progress_but_exclude_identity_writes() {
        use futures_util::FutureExt as _;
        let ingress = SessionIngress::new(Some(SessionId::from("old")));
        let old = SessionId::from("old");
        let request = ingress.request(&old).await.unwrap();
        let notification = ingress
            .notification(notif("old"), None)
            .now_or_never()
            .expect("progress cannot wait for a human response")
            .unwrap()
            .unwrap();
        drop(notification);
        let second = ingress
            .request(&old)
            .now_or_never()
            .expect("another admitted callback shares the session")
            .unwrap();
        drop(second);
        let mut transition = std::pin::pin!(ingress.fence.write());
        assert!(futures_util::poll!(transition.as_mut()).is_pending());
        let mut stale = std::pin::pin!(ingress.request(&old));
        assert!(
            futures_util::poll!(stale.as_mut()).is_pending(),
            "queued writer excludes later readers"
        );
        drop(request);
        let transition = transition.await;
        ingress.begin();
        ingress.finish(Some(SessionId::from("new"))).unwrap();
        drop(transition);
        assert!(
            stale.await.is_err(),
            "old admission must be rechecked after the write"
        );
    }

    #[tokio::test]
    async fn candidate_callback_waits_without_blocking_commit_write() {
        let ingress = SessionIngress::default();
        let generation = {
            let _guard = ingress.fence.write().await;
            ingress.begin()
        };
        let id = SessionId::from("new");
        ingress.resolve(Some(generation), id.clone());
        let mut request = std::pin::pin!(ingress.request(&id));
        assert!(futures_util::poll!(request.as_mut()).is_pending());
        let commit = ingress
            .fence
            .try_write()
            .expect("AwaitCommit holds no read guard");
        ingress.finish(Some(id.clone())).unwrap();
        assert!(
            futures_util::poll!(request.as_mut()).is_pending(),
            "callback cannot overtake commit replay"
        );
        drop(commit);
        let admitted = request.await.unwrap();
        drop(admitted);
    }
}
