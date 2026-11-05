//! Native session admission, selected only by the connection lifecycle.

use agent_client_protocol::schema::v1::{RequestId, SessionId, SessionNotification};
use agent_client_protocol::{
    Agent, ConnectionTo, JsonRpcMessage, JsonRpcNotification, JsonRpcRequest, UntypedMessage,
};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::Mutex as StateMutex;
use tokio::sync::{oneshot, Notify, RwLock, RwLockReadGuard};

use super::connection::ConnectionState;
use super::errors::{acp_internal_error, AcpError};
use super::tool_context::ToolContextCache;
use super::update_events::{AgentMessageDedup, CompactionTracker};
use crate::acp::control_protocol::{
    AdoptedPromptCompletion, NativeChannelLostMarker, PromptCompletedMarker, SessionReplayed,
    MAX_COMPLETION_MARKER_BYTES, MAX_CONTROL_QUEUE_BYTES, MAX_CONTROL_QUEUE_FRAMES,
    MAX_NATIVE_LOSS_FRAME_BYTES,
};
use crate::acp::establishment::NativeRequestContext;
use crate::acp::state::{AuthStatus, AUTH_STATUS_UPDATE_METHOD};

// Cover the producer's ordinary queue, bootstrap slots and retained items.
// Charge original control wire bytes; typed JSON can expand their representation.
// Direct stdio has no detached queue and bounds its typed notification payload.
const MAX_PENDING_UPDATES: usize = MAX_CONTROL_QUEUE_FRAMES
    + crate::acp::control_protocol::CONTROL_BOOTSTRAP_RESERVED_FRAMES
    + crate::acp::control_protocol::CONTROL_REPLAY_RESERVED_FRAMES;
const MAX_PENDING_BYTES: usize = MAX_CONTROL_QUEUE_BYTES;

// Only the daemon control reader writes this synthetic transport field.
pub(super) const CONTROL_FRAME_BYTES_FIELD: &str = "__aoe_control_frame_bytes";

#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ControlIngressMetadata {
    pub(super) wire_bytes: usize,
    pub(super) evidence: crate::acp::control_protocol::NativeNotifyEvidence,
}

#[derive(Debug, serde::Serialize)]
pub(super) struct IngressUpdate {
    pub(super) notification: SessionNotification,
    pub(super) evidence: crate::acp::control_protocol::NativeNotifyEvidence,
    #[serde(skip)]
    pub(super) current_scope: Option<Arc<NativeRequestContext>>,
    /// Post-result metadata retains its original Pending authority, not Applied session permission.
    #[serde(skip)]
    pub(super) metadata_scope: Option<Arc<crate::acp::establishment::NativeConstructorObservation>>,
    #[serde(skip)]
    pub(super) resource_start: Option<Arc<crate::acp::establishment::NativeResourceStartContext>>,
    #[serde(skip)]
    pub(super) history: Option<Arc<OriginalIngressHistory>>,
    #[serde(skip)]
    pub(super) suppress_replay: bool,
}

#[derive(Debug, serde::Serialize)]
pub(super) struct IngressAuth {
    pub(super) status: AuthStatus,
    pub(super) evidence: crate::acp::control_protocol::NativeNotifyEvidence,
    #[serde(skip)]
    pub(super) current_scope: Option<Arc<NativeRequestContext>>,
}

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
    /// Raw params retain trusted frame metadata until transport-aware decode.
    AuthStatus(serde_json::Value),
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
            Self::AuthStatus(params) => UntypedMessage::new(self.method(), params),
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
            return Ok(Self::AuthStatus(serde_json::to_value(params)?));
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
    ) -> agent_client_protocol::Result<(IngressUpdate, Option<usize>)> {
        let invalid = |error: serde_json::Error| {
            agent_client_protocol::Error::invalid_params().data(error.to_string())
        };
        let metadata = take_control_metadata(&mut params, control_transport)?;
        let wire_bytes = metadata.map(|metadata| metadata.wire_bytes);
        Ok((
            IngressUpdate {
                notification: serde_json::from_value(params).map_err(invalid)?,
                evidence: metadata.map_or_else(Default::default, |metadata| metadata.evidence),
                current_scope: None,
                metadata_scope: None,
                resource_start: None,
                history: None,
                suppress_replay: false,
            },
            wire_bytes,
        ))
    }

    pub(super) fn decode_auth(
        mut params: serde_json::Value,
        control_transport: bool,
    ) -> agent_client_protocol::Result<(IngressAuth, Option<usize>)> {
        let metadata = take_control_metadata(&mut params, control_transport)?;
        let parsed: AuthStatusParams = serde_json::from_value(params).map_err(|error| {
            agent_client_protocol::Error::invalid_params().data(error.to_string())
        })?;
        Ok((
            IngressAuth {
                status: parsed.auth_status,
                evidence: metadata.map_or_else(Default::default, |metadata| metadata.evidence),
                current_scope: None,
            },
            metadata.map(|metadata| metadata.wire_bytes),
        ))
    }
}

fn take_control_metadata(
    params: &mut serde_json::Value,
    control_transport: bool,
) -> agent_client_protocol::Result<Option<ControlIngressMetadata>> {
    if !control_transport {
        if let serde_json::Value::Object(fields) = params {
            fields.remove(CONTROL_FRAME_BYTES_FIELD);
        }
        return Ok(None);
    }
    let claim = match params {
        serde_json::Value::Object(fields) => fields.remove(CONTROL_FRAME_BYTES_FIELD),
        serde_json::Value::Array(fields) => fields.pop(),
        _ => None,
    }
    .ok_or_else(|| {
        agent_client_protocol::Error::invalid_params().data("missing control ingress metadata")
    })?;
    serde_json::from_value(claim)
        .map(Some)
        .map_err(|error| agent_client_protocol::Error::invalid_params().data(error.to_string()))
}

#[derive(Default)]
pub(super) struct SessionIngress {
    state: StateMutex<IdentityState>,
    projection: StateMutex<std::sync::Weak<crate::acp::event_sender::ProjectionState>>,
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
    original: Option<Arc<OriginalCallbackGrant>>,
    history: Option<Arc<OriginalIngressHistory>>,
    callbacks_revoked: bool,
}

struct Establishment {
    generation: u64,
    candidate: Option<SessionId>,
    updates: Vec<PendingIngress>,
    encoded_bytes: usize,
}

/// One actual original invocation on this authenticated control connection.
/// It permits existing reverse handlers, not input, adoption, or readiness.
type OriginalCallbackGrant = crate::acp::establishment::NativeCallbackOriginal;

pub(super) struct OriginalIngressHistory {
    original: Arc<OriginalCallbackGrant>,
    pub(super) dedup: StateMutex<AgentMessageDedup>,
    pub(super) compaction: StateMutex<CompactionTracker>,
    pub(super) tool_context: ToolContextCache,
    constructor: std::sync::OnceLock<Arc<crate::acp::establishment::NativeConstructorObservation>>,
}

impl std::fmt::Debug for OriginalIngressHistory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalIngressHistory")
            .field("attempt", &self.original.attempt.id)
            .finish_non_exhaustive()
    }
}

impl OriginalIngressHistory {
    fn accepts_session(&self, session: &str) -> bool {
        self.original.known_session_id() == Some(session)
    }
}

impl OriginalCallbackGrant {
    fn known_session_id(&self) -> Option<&str> {
        if let Some(postimage) = self.postimage.get() {
            return postimage.choice.acp_session_id.as_deref();
        }
        match &self.attempt.target {
            crate::acp::establishment::EstablishmentTarget::Load { session_id, .. } => {
                Some(session_id.as_str())
            }
            _ => None,
        }
    }

    fn same_source(
        &self,
        attempt: &crate::acp::establishment::EstablishmentAttempt,
        owner: crate::acp::runner_lifecycle::RunnerIdentity,
        store: Option<&crate::session::ExecutionBinding>,
    ) -> bool {
        self.attempt.same_source(attempt)
            && self.owner == owner
            && self.store.as_ref() == store
            && self
                .store
                .as_ref()
                .and_then(|binding| binding.exported_default_store.as_ref())
                == store.and_then(|binding| binding.exported_default_store.as_ref())
    }

    fn validate(&self) -> anyhow::Result<()> {
        // No ingress writer or state mutex is retained during native I/O.
        let storage = crate::session::Storage::open_reference(&self.attempt.profile)?;
        let _native = storage.acquire_instance_lifecycle_lock(&self.attempt.session_id)?;
        let row = storage
            .load_instances_complete()?
            .into_iter()
            .find(|row| row.id == self.attempt.session_id)
            .ok_or(crate::session::SessionGone)?;
        anyhow::ensure!(
            self.matches_original(&row, &storage)?,
            "original callback choice changed"
        );
        let registry =
            crate::process::worker_registry::try_registry_lock(&self.attempt.session_id)?
                .ok_or(crate::process::worker_registry::RegistryBusy)?;
        self.check_owner(&registry)?;
        Ok(())
    }
}

/// Captured before native relay delivery, never refreshed after queueing.
#[derive(Debug, Clone)]
pub(super) struct IngressRequestScope {
    generation: u64,
    expected: Option<Arc<crate::acp::establishment::EstablishmentAttempt>>,
}

#[derive(Debug, Clone)]
pub(super) struct CapturedCallback {
    pub(super) ingress: IngressRequestScope,
    pub(super) context: Option<Arc<NativeRequestContext>>,
    pub(super) history: Option<Arc<OriginalIngressHistory>>,
    ingress_source: std::sync::Weak<SessionIngress>,
    pub(super) connection: std::sync::Weak<ConnectionState>,
}

impl CapturedCallback {
    pub(super) fn original_session_id(&self) -> Option<&str> {
        self.ingress
            .expected
            .as_ref()
            .map(|attempt| attempt.session_id.as_str())
    }

    pub(super) fn is_live(&self) -> bool {
        let Some(ingress) = self.ingress_source.upgrade() else {
            return false;
        };
        let state = ingress.state.lock().unwrap();
        !state.failed
            && !state.callbacks_revoked
            && state.generation == self.ingress.generation
            && state.original.as_ref().is_some_and(|original| {
                self.ingress
                    .expected
                    .as_ref()
                    .is_some_and(|attempt| attempt.same_source(&original.attempt))
            })
    }
}

/// Bootstrap updates and private barriers share one ordered, bounded replay.
/// A callback buffers rather than blocking the SDK dispatcher on a commit
/// whose handshake response may still be behind it in the same shim.
#[derive(Debug, serde::Serialize)]
pub(super) enum PendingIngress {
    Update(Box<IngressUpdate>),
    Auth(Box<IngressAuth>),
    PromptCompleted(Box<AdoptedPromptCompletion>),
    NativeLost,
    Replayed(SessionReplayed),
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
    Establishing,
    Active,
}

impl SessionIngress {
    pub(super) fn observed_native_origin(&self) -> Option<crate::acp::state::NativeEventOrigin> {
        let state = self.state.lock().expect("ingress identity");
        let original = state.original.as_ref()?;
        crate::acp::state::NativeEventOrigin::owned_attempt(&original.attempt, Some(original.owner))
    }

    #[cfg(test)]
    pub(super) fn test_pending_footprint(
        &self,
        measure_typed: bool,
    ) -> Option<(bool, usize, usize, usize)> {
        let state = self.state.lock().unwrap();
        let pending = state.pending.as_ref()?;
        let mut count = PendingByteCount {
            written: 0,
            limit: usize::MAX,
        };
        let mut updates = 0;
        for entry in &pending.updates {
            if let PendingIngress::Update(update) = entry {
                updates += 1;
                if measure_typed {
                    serde_json::to_writer(&mut count, &update.notification).unwrap();
                }
            }
        }
        Some((state.failed, updates, pending.encoded_bytes, count.written))
    }

    pub(super) fn new(current: Option<SessionId>) -> Self {
        Self {
            state: StateMutex::new(IdentityState {
                current,
                ..IdentityState::default()
            }),
            ..Self::default()
        }
    }
    pub(super) fn observe_projection(
        &self,
        projection: &Arc<crate::acp::event_sender::ProjectionState>,
    ) {
        *self.projection.lock().expect("ingress projection") = Arc::downgrade(projection);
    }

    fn projection_lost(&self) {
        if let Some(projection) = self
            .projection
            .lock()
            .expect("ingress projection")
            .upgrade()
        {
            projection.lost();
        }
    }

    pub(super) fn is_current_stable(&self, id: &SessionId) -> bool {
        let state = self.state.lock().unwrap();
        !state.failed && state.pending.is_none() && state.current.as_ref() == Some(id)
    }

    pub(super) fn current(&self) -> Option<SessionId> {
        self.state.lock().unwrap().current.clone()
    }

    /// Called under the mutation fence, or before the connection starts.
    pub(super) fn begin(&self) -> u64 {
        let mut state = self.state.lock().unwrap();
        state.generation += 1;
        let generation = state.generation;
        state.original = None;
        state.history = None;
        state.callbacks_revoked = false;
        state.pending = Some(Establishment {
            generation,
            candidate: None,
            updates: Vec::new(),
            encoded_bytes: 0,
        });
        generation
    }

    pub(super) fn authorize_original(
        &self,
        attempt: Arc<crate::acp::establishment::EstablishmentAttempt>,
        owner: crate::acp::runner_lifecycle::RunnerIdentity,
        store: Option<crate::session::ExecutionBinding>,
        postimage: Option<Arc<crate::acp::establishment::EstablishmentPostimage>>,
    ) -> bool {
        let mut state = self.state.lock().unwrap();
        if state.failed
            || state.callbacks_revoked
            || attempt.producer.is_none()
            || owner.birth_fingerprint.is_none()
        {
            return false;
        }
        if state.pending.is_none() && state.current.is_none() {
            state.generation += 1;
            state.pending = Some(Establishment {
                generation: state.generation,
                candidate: None,
                updates: Vec::new(),
                encoded_bytes: 0,
            });
        }
        if let Some(original) = &state.original {
            if !original.same_source(&attempt, owner, store.as_ref()) {
                return false;
            }
        } else {
            let original = Arc::new(OriginalCallbackGrant {
                attempt: Arc::clone(&attempt),
                owner,
                store,
                postimage: std::sync::OnceLock::new(),
                notification_scope: StateMutex::new(None),
            });
            state.history = Some(Arc::new(OriginalIngressHistory {
                original: Arc::clone(&original),
                dedup: StateMutex::default(),
                compaction: StateMutex::default(),
                tool_context: Arc::default(),
                constructor: std::sync::OnceLock::new(),
            }));
            state.original = Some(original);
        }
        let original = state.original.as_ref().expect("authorized original");
        if let Some(postimage) = postimage {
            if let Some(existing) = original.postimage.get() {
                if existing.as_ref() != postimage.as_ref() {
                    return false;
                }
            } else if original.postimage.set(postimage).is_err() {
                return false;
            }
        }
        if let crate::acp::establishment::EstablishmentTarget::Load { session_id, .. } =
            &attempt.target
        {
            if let Some(pending) = state.pending.as_mut() {
                pending.candidate = Some(SessionId::from(session_id.clone()));
            }
        }
        drop(state);
        self.changed.notify_waiters();
        true
    }

    /// Preserve terminal evidence for the owned failed ingress, never permission.
    pub(super) fn failed_original_matches(
        &self,
        attempt: &crate::acp::establishment::EstablishmentAttempt,
        owner: crate::acp::runner_lifecycle::RunnerIdentity,
        store: Option<&crate::session::ExecutionBinding>,
    ) -> bool {
        let state = self.state.lock().unwrap();
        state.failed
            && state
                .original
                .as_ref()
                .is_some_and(|original| original.same_source(attempt, owner, store))
    }

    pub(super) fn authorize_constructor(
        &self,
        attempt: &crate::acp::establishment::EstablishmentAttempt,
        context: crate::acp::state::NativeEventContext,
        fence: Arc<crate::session::NativeInputFence>,
    ) -> bool {
        let state = self.state.lock().expect("ingress identity");
        let Some(history) = state.history.as_ref().filter(|history| {
            history.original.attempt.as_ref() == attempt
                && !state.failed
                && !state.callbacks_revoked
        }) else {
            return false;
        };
        if history.constructor.get().is_some() {
            return true;
        }
        let Ok(constructor) = crate::acp::establishment::NativeConstructorObservation::capture(
            Arc::clone(&history.original),
            context,
            fence,
        ) else {
            return false;
        };
        history.constructor.set(constructor).is_ok()
    }

    pub(super) fn constructor_scope(
        &self,
    ) -> Option<Arc<crate::acp::establishment::NativeConstructorObservation>> {
        let state = self.state.lock().expect("ingress identity");
        if state.failed || state.callbacks_revoked {
            return None;
        }
        state.history.as_ref()?.constructor.get().cloned()
    }

    pub(super) fn capture_native_callback(
        self: &Arc<Self>,
        evidence: crate::acp::control_protocol::NativeNotifyEvidence,
        reported_session: Option<&str>,
        connection: &Arc<ConnectionState>,
    ) -> Option<Arc<CapturedCallback>> {
        let physical = evidence.physical_source?;
        let (generation, original, history) = {
            let state = self.state.lock().unwrap();
            if state.failed || state.callbacks_revoked {
                return None;
            }
            (
                state.generation,
                state.original.clone()?,
                state.history.clone(),
            )
        };
        let expected = crate::acp::state::NativeEventOrigin::owned_attempt(
            &original.attempt,
            Some(original.owner),
        )?;
        if physical.witness != expected {
            return None;
        }
        if let (Some(session), Some(reported)) = (original.known_session_id(), reported_session) {
            if session != reported {
                return None;
            }
        }
        let history = history.filter(|history| {
            reported_session.is_some_and(|session| history.accepts_session(session))
        });
        let ingress = IngressRequestScope {
            generation,
            expected: Some(Arc::clone(&original.attempt)),
        };
        let context = NativeRequestContext::callback(
            original,
            evidence.session_stamp,
            evidence.original_request_stamp,
            reported_session.map(Arc::from),
        )
        .ok()
        .map(Arc::new);
        Some(Arc::new(CapturedCallback {
            ingress,
            context,
            history,
            ingress_source: Arc::downgrade(self),
            connection: Arc::downgrade(connection),
        }))
    }

    pub(super) fn capture_request(&self) -> IngressRequestScope {
        let generation = self.state.lock().unwrap().generation;
        let expected = self
            .projection
            .lock()
            .unwrap()
            .upgrade()
            .and_then(|projection| projection.attempt());
        IngressRequestScope {
            generation,
            expected,
        }
    }

    pub(super) fn revoke_requests(&self) {
        self.state.lock().unwrap().callbacks_revoked = true;
        self.changed.notify_waiters();
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
        drop(state);
        self.changed.notify_waiters();
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
            PendingIngress::Update(update) => {
                Some(&update.notification.session_id) == id.as_ref()
                    || update.evidence.physical_source.is_some()
            }
            PendingIngress::PromptCompleted(_)
            | PendingIngress::NativeLost
            | PendingIngress::Replayed(_)
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
            PendingIngress::Replayed(_) => MAX_NATIVE_LOSS_FRAME_BYTES,
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
            self.projection_lost();
            drop(state);
            self.changed.notify_waiters();
            self.failure.notify_one();
            return Err(Self::overflow_error());
        }
        pending.encoded_bytes += count.written;
        pending.updates.push(marker);
        Ok(None)
    }

    /// Freeze the authenticated observation before admission, gates, or replay.
    /// The cache belongs to the immutable original, never the latest SDK epoch.
    pub(super) fn capture_notification(
        &self,
        mut update: IngressUpdate,
        suppress_replay: bool,
    ) -> IngressUpdate {
        update.history = {
            let state = self.state.lock().expect("ingress identity");
            state
                .history
                .as_ref()
                .filter(|history| {
                    update.evidence.physical_source.is_some_and(|physical| {
                        crate::acp::state::NativeEventOrigin::owned_attempt(
                            &history.original.attempt,
                            Some(history.original.owner),
                        ) == Some(physical.witness)
                    }) && history.accepts_session(update.notification.session_id.0.as_ref())
                })
                .cloned()
        };
        update.suppress_replay = suppress_replay && update.history.is_some();
        update.current_scope = self.notification_scope(update.evidence).filter(|context| {
            context
                .receipt
                .as_ref()
                .and_then(|receipt| receipt.session_id())
                == Some(update.notification.session_id.0.as_ref())
        });
        update.metadata_scope = matches!(
            &update.notification.update,
            agent_client_protocol::schema::v1::SessionUpdate::CurrentModeUpdate(_)
                | agent_client_protocol::schema::v1::SessionUpdate::UsageUpdate(_)
                | agent_client_protocol::schema::v1::SessionUpdate::AvailableCommandsUpdate(_)
                | agent_client_protocol::schema::v1::SessionUpdate::ConfigOptionUpdate(_)
        )
        .then(|| self.original_metadata_scope(&update))
        .flatten();
        update.resource_start = update
            .evidence
            .physical_source
            .zip(update.evidence.resource_start_stamp)
            .and_then(|(physical, stamp)| self.observed_scope(physical, stamp))
            .and_then(|context| {
                crate::acp::establishment::NativeResourceStartContext::from_frame(
                    context,
                    update.notification.session_id.clone(),
                )
                .ok()
            })
            .map(Arc::new);
        update
    }

    fn original_metadata_scope(
        &self,
        update: &IngressUpdate,
    ) -> Option<Arc<crate::acp::establishment::NativeConstructorObservation>> {
        let stamp = update.evidence.original_request_stamp?;
        let physical = update.evidence.physical_source?;
        let history = update.history.as_ref()?;
        let original = &history.original;
        let receipt = self
            .projection
            .lock()
            .expect("ingress projection")
            .upgrade()?
            .receipt_snapshot()?;
        if stamp.attempt_id != original.attempt.id
            || stamp.context != original.attempt.context
            || stamp.generation != original.attempt.generation
            || !original.same_source(&receipt.attempt, receipt.owner, receipt.store.as_ref())
            || crate::acp::state::NativePhysicalSource::from_receipt(&receipt) != Some(physical)
            || !receipt.replay.closed
            || receipt.replay.lost
            || receipt.session_id() != Some(update.notification.session_id.0.as_ref())
            || original.postimage.get()?.as_ref()
                != &receipt.postimage(receipt.attempt.context).ok()?
        {
            return None;
        }
        history.constructor.get().cloned()
    }

    pub(super) fn notification_scope(
        &self,
        evidence: crate::acp::control_protocol::NativeNotifyEvidence,
    ) -> Option<Arc<NativeRequestContext>> {
        self.observed_scope(evidence.physical_source?, evidence.session_stamp?)
    }

    fn observed_scope(
        &self,
        physical: crate::acp::state::NativePhysicalSource,
        stamp: crate::acp::establishment::NativeSessionStamp,
    ) -> Option<Arc<NativeRequestContext>> {
        let origin = physical.witness;
        let original = self.state.lock().unwrap().original.clone()?;
        let receipt = self
            .projection
            .lock()
            .expect("ingress projection")
            .upgrade()?
            .receipt_snapshot()?;
        if crate::acp::state::NativeEventOrigin::owned_attempt(
            &receipt.attempt,
            Some(receipt.owner),
        ) != Some(origin)
            || !original.same_source(&receipt.attempt, receipt.owner, receipt.store.as_ref())
        {
            return None;
        }
        let postimage = original.postimage.get()?.clone();
        let mut cached = original.notification_scope.lock().unwrap();
        if let Some((known, context)) = cached.as_ref() {
            if *known == stamp {
                return Some(Arc::clone(context));
            }
            if known.generation > stamp.generation {
                return None;
            }
        }
        let context =
            Arc::new(NativeRequestContext::session_observation(receipt, postimage, stamp).ok()?);
        *cached = Some((stamp, Arc::clone(&context)));
        Some(context)
    }

    /// Share admission through effects; identity transitions require exclusive access.
    pub(super) async fn notification(
        &self,
        notification: IngressUpdate,
        wire_bytes: Option<usize>,
    ) -> agent_client_protocol::Result<Option<(IngressUpdate, RwLockReadGuard<'_, ()>)>> {
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
                serde_json::to_writer(&mut count, &notification)
                    .ok()
                    .map(|()| count.written)
            };
            let Some(size) = size else {
                pending.updates.clear();
                pending.encoded_bytes = 0;
                state.failed = true;
                self.projection_lost();
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
        let accepted = state.current.as_ref() == Some(&notification.notification.session_id)
            || notification.evidence.physical_source.is_some();
        Ok(accepted.then_some(notification))
    }

    fn request_admission(&self, scope: &IngressRequestScope, id: &SessionId) -> RequestAdmission {
        let state = self.state.lock().unwrap();
        if state.failed || state.callbacks_revoked || scope.generation != state.generation {
            return RequestAdmission::Reject;
        }
        let Some(original) = state.original.as_ref() else {
            return if state.pending.is_some() && scope.expected.is_some() {
                RequestAdmission::AwaitCommit
            } else {
                RequestAdmission::Reject
            };
        };
        if !scope
            .expected
            .as_ref()
            .is_some_and(|expected| expected.same_source(&original.attempt))
        {
            return RequestAdmission::Reject;
        }
        if let Some(pending) = state.pending.as_ref() {
            return if pending
                .candidate
                .as_ref()
                .is_none_or(|candidate| candidate == id)
            {
                RequestAdmission::Establishing
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

    pub(super) fn request_refused() -> agent_client_protocol::Error {
        let mut error = agent_client_protocol::Error::invalid_params();
        error.message = "native callback scope is no longer active".into();
        error
    }

    async fn request(
        &self,
        scope: &IngressRequestScope,
        id: &SessionId,
    ) -> agent_client_protocol::Result<Option<RwLockReadGuard<'_, ()>>> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            match self.request_admission(scope, id) {
                RequestAdmission::Reject => return Err(Self::request_refused()),
                RequestAdmission::AwaitCommit => changed.await,
                RequestAdmission::Establishing => return Ok(None),
                RequestAdmission::Active => {
                    let guard = self.fence.read().await;
                    if self.request_admission(scope, id) == RequestAdmission::Active {
                        return Ok(Some(guard));
                    }
                }
            }
        }
    }

    async fn request_invalid(&self, scope: &IngressRequestScope, id: &SessionId) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.request_admission(scope, id) == RequestAdmission::Reject {
                return;
            }
            changed.await;
        }
    }

    pub(super) async fn run_request<T>(
        &self,
        scope: IngressRequestScope,
        id: &SessionId,
        work: impl std::future::Future<Output = agent_client_protocol::Result<T>>,
    ) -> agent_client_protocol::Result<T> {
        let _guard = self.request(&scope, id).await?;
        let original = self
            .state
            .lock()
            .unwrap()
            .original
            .clone()
            .ok_or_else(Self::request_refused)?;
        tokio::select! {
            biased;
            _ = self.request_invalid(&scope, id) => Err(Self::request_refused()),
            result = async {
                let valid = tokio::task::spawn_blocking(move || original.validate()).await;
                if !matches!(valid, Ok(Ok(())))
                    || matches!(self.request_admission(&scope, id), RequestAdmission::Reject)
                {
                    return Err(Self::request_refused());
                }
                work.await
            } => result,
        }
    }

    pub(super) fn has_failed(&self) -> bool {
        self.state.lock().unwrap().failed
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

pub(super) struct ApplicationOutcome {
    completion: Arc<OrderedRequestCompletion>,
    _duration: super::connection::NativeOperationDuration,
}

impl Drop for ApplicationOutcome {
    fn drop(&mut self) {
        self.completion.finish();
    }
}

pub(super) struct CancelOrderedRequest {
    pub(super) connection: ConnectionTo<Agent>,
    pub(super) id: RequestId,
    completion: Arc<OrderedRequestCompletion>,
    forward: Option<(Arc<super::control::DaemonControlClient>, u64)>,
}

impl Drop for CancelOrderedRequest {
    fn drop(&mut self) {
        if let Some((control, call_id)) = &self.forward {
            control.cancel_forward(&self.id, *call_id);
        }
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

fn register_ordered_request<Req>(
    connection: &ConnectionTo<Agent>,
    state: &Arc<ConnectionState>,
    context: Option<NativeRequestContext>,
    request: Req,
    track_application: bool,
) -> Result<OrderedRequest<Req::Response>, AcpError>
where
    Req: JsonRpcRequest,
    Req::Response: Send + 'static,
{
    let control = match state.control.get() {
        Some(control) => Some(
            control
                .upgrade()
                .ok_or_else(|| AcpError::Protocol("ACP control connection unavailable".into()))?,
        ),
        None => None,
    };
    let context =
        match control.as_ref() {
            Some(_) => Some(context.ok_or_else(|| {
                AcpError::Protocol("native request has no captured context".into())
            })?),
            None => None,
        };
    let completion = Arc::new(OrderedRequestCompletion {
        settled: AtomicBool::new(false),
        application: std::sync::atomic::AtomicU8::new(u8::from(track_application)),
        state: Arc::clone(state),
    });
    let (sent, forward) = match control {
        Some(control) => {
            let (sent, call_id) = control.register_forward(
                context
                    .as_ref()
                    .expect("control request context")
                    .scope
                    .clone(),
                || connection.send_request(request),
            );
            (sent, Some((control, call_id)))
        }
        None => (connection.send_request(request), None),
    };
    let cancel = CancelOrderedRequest {
        connection: connection.clone(),
        id: sent.id().clone(),
        completion: Arc::clone(&completion),
        forward,
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
            OrderedRequestResult::Correlated(result)
        };
        let _ = sender.send(result);
        std::future::ready(Ok(()))
    });
    Ok(OrderedRequest {
        receiver,
        cancel,
        registration,
    })
}

pub(super) fn ordered_native_request<Req>(
    connection: &ConnectionTo<Agent>,
    state: &Arc<ConnectionState>,
    context: Option<NativeRequestContext>,
    request: Req,
) -> Result<OrderedRequest<Req::Response>, AcpError>
where
    Req: JsonRpcRequest,
    Req::Response: Send + 'static,
{
    state.admit(|| register_ordered_request(connection, state, context, request, false))?
}

/// Runner callbacks retain their ticket through terminal Native write settlement.
/// Direct callbacks retain SDK duration; cancellation never invents a decision.
pub(super) fn spawn_native_callback<Handle, Handled>(
    connection: &ConnectionTo<Agent>,
    state: &Arc<ConnectionState>,
    control: Option<&Arc<super::control::DaemonControlClient>>,
    request_id: &RequestId,
    handle: Handle,
) -> agent_client_protocol::Result<()>
where
    Handle: FnOnce(agent_client_protocol::Result<Option<Arc<CapturedCallback>>>) -> Handled
        + Send
        + 'static,
    Handled: Future<Output = agent_client_protocol::Result<()>> + Send + 'static,
{
    use super::control::ReverseCallbackAdmission;
    let admission = match control {
        Some(control) => match control.admit_reverse_callback(request_id) {
            ReverseCallbackAdmission::Admitted(callback) => Some(Ok(Some(callback))),
            ReverseCallbackAdmission::Excluded => Some(Err(SessionIngress::request_refused())),
            ReverseCallbackAdmission::Ended => return Ok(()),
        },
        None => None,
    };
    match state.admit(|| {
        let cancellation = state.clone();
        connection.spawn(async move {
            #[cfg(test)]
            {
                let gate = cancellation.test_callback.lock().unwrap().take();
                if let Some((entered, release)) = gate {
                    let _ = entered.send(());
                    tokio::select! {
                        biased;
                        _ = cancellation.wait_callback_end() => return Ok(()),
                        released = release => if released.is_err() { return Ok(()); },
                    }
                }
            }
            // The relay ticket owns duration through runner write settlement.
            // Taking another fair READ here can deadlock behind Fresh's writer.
            let _duration = if admission.is_none() {
                Some(tokio::select! {
                    biased;
                    _ = cancellation.wait_callback_end() => return Ok(()),
                    guard = cancellation.callback_duration.read() => guard,
                })
            } else {
                None
            };
            tokio::select! {
                biased;
                _ = cancellation.wait_callback_end() => Ok(()),
                result = handle(admission.unwrap_or(Ok(None))) => result,
            }
        })
    }) {
        Ok(result) => result,
        Err(_) => Ok(()),
    }
}

/// Prepare while the caller holds `ConnectionState::admit`.
pub(super) fn prepare_native_request<Req>(
    connection: &ConnectionTo<Agent>,
    state: &Arc<ConnectionState>,
    context: Option<NativeRequestContext>,
    request: Req,
) -> Result<(OrderedRequest<Req::Response>, ApplicationOutcome), AcpError>
where
    Req: JsonRpcRequest,
    Req::Response: Send + 'static,
{
    let duration = state.reserve_native_duration()?;
    let request = register_ordered_request(connection, state, context, request, true)?;
    let outcome = ApplicationOutcome {
        completion: Arc::clone(&request.cancel.completion),
        _duration: duration,
    };
    Ok((request, outcome))
}

pub(super) fn spawn_native_request<Req, Handle, Handled>(
    connection: &ConnectionTo<Agent>,
    state: &Arc<ConnectionState>,
    context: Option<NativeRequestContext>,
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
        let (request, outcome) = prepare_native_request(connection, state, context, request)?;
        connection
            .spawn(async move {
                let _outcome = outcome;
                handle(request).await;
                Ok(())
            })
            .map_err(|error| AcpError::Protocol(super::errors::public_error_message(&error)))
    })?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::acp_client::test_helpers::text_chunk;
    use crate::acp::state::AuthStatusKind;

    fn notif(id: &str) -> IngressUpdate {
        SessionIngressNotification::decode_update(
            serde_json::to_value(SessionNotification::new(
                id.to_owned(),
                text_chunk("x", None),
            ))
            .unwrap(),
            false,
        )
        .unwrap()
        .0
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
            let (auth, _) = SessionIngressNotification::decode_auth(status, false).unwrap();
            assert_eq!(auth.status.kind, kind, "{name}");
            assert_eq!(auth.status.label, label, "{name}");
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
            for _ in 0..MAX_PENDING_UPDATES {
                ingress
                    .route(notif("s"), None)
                    .expect("a full detach-queue backlog fits");
            }
            let replay = ingress
                .finish(Some(SessionId::from("s")))
                .expect("commit succeeds");
            assert_eq!(replay.len(), MAX_PENDING_UPDATES);

            // Past that contract the buffer stays bounded: an overflow fails the
            // attach rather than growing without limit.
            {
                let ingress = SessionIngress::default();
                ingress.begin();
                for _ in 0..MAX_PENDING_UPDATES {
                    ingress.route(notif("s"), None).expect("under the cap");
                }
                assert!(ingress.route(notif("s"), None).is_err());
            }
        }
    }
}
