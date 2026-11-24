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
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

// Bounds the child routes one connection can admit.
const MAX_CHILDREN: usize = 256;

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
    pub(super) original_source: Option<crate::acp::state::NativeEventOrigin>,
    #[serde(skip)]
    pub(super) child_route: Option<(SessionId, ChildRoute)>,
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
    Autonomous(
        crate::acp::control_protocol::NativeAutonomousPhase,
        serde_json::Value,
    ),
    AutonomousSnapshot(serde_json::Value),
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
            || crate::acp::control_protocol::NativeAutonomousPhase::from_method(method).is_some()
            || method == crate::acp::control_protocol::NATIVE_AUTONOMOUS_SNAPSHOT_METHOD
    }

    fn method(&self) -> &str {
        match self {
            Self::Update(_) => "session/update",
            Self::Replayed(marker) => marker.method(),
            Self::PromptCompleted(marker) => marker.method(),
            Self::NativeLost(marker) => marker.method(),
            Self::AuthStatus(_) => AUTH_STATUS_UPDATE_METHOD,
            Self::Autonomous(phase, _) => phase.method(),
            Self::AutonomousSnapshot(_) => {
                crate::acp::control_protocol::NATIVE_AUTONOMOUS_SNAPSHOT_METHOD
            }
        }
    }

    fn to_untyped_message(&self) -> agent_client_protocol::Result<UntypedMessage> {
        match self {
            Self::Update(params) => UntypedMessage::new(self.method(), params),
            Self::Replayed(marker) => marker.to_untyped_message(),
            Self::PromptCompleted(marker) => marker.to_untyped_message(),
            Self::NativeLost(marker) => marker.to_untyped_message(),
            Self::AuthStatus(params) => UntypedMessage::new(self.method(), params),
            Self::Autonomous(_, params) => UntypedMessage::new(self.method(), params),
            Self::AutonomousSnapshot(params) => UntypedMessage::new(self.method(), params),
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
        if NativeChannelLostMarker::matches_method(method) {
            return Ok(Self::NativeLost(NativeChannelLostMarker::parse_message(
                method, params,
            )?));
        }
        if method == AUTH_STATUS_UPDATE_METHOD {
            return Ok(Self::AuthStatus(params.clone()));
        }
        if method == crate::acp::control_protocol::NATIVE_AUTONOMOUS_SNAPSHOT_METHOD {
            return Ok(Self::AutonomousSnapshot(params.clone()));
        }
        if let Some(phase) =
            crate::acp::control_protocol::NativeAutonomousPhase::from_method(method)
        {
            return Ok(Self::Autonomous(phase, params.clone()));
        }
        if !SessionNotification::matches_method(method) {
            return Err(agent_client_protocol::Error::method_not_found());
        }
        Ok(Self::Update(params.clone()))
    }
}

impl JsonRpcNotification for SessionIngressNotification {}

impl SessionIngressNotification {
    pub(super) fn decode_autonomous(
        phase: crate::acp::control_protocol::NativeAutonomousPhase,
        mut params: serde_json::Value,
        control_transport: bool,
    ) -> agent_client_protocol::Result<(
        crate::acp::control_protocol::NativeAutonomousNotification,
        crate::acp::control_protocol::NativeNotifyEvidence,
        Option<usize>,
    )> {
        let metadata = take_control_metadata(&mut params, control_transport)?;
        let notification: crate::acp::control_protocol::NativeAutonomousNotification =
            serde_json::from_value(params)
                .map_err(|_| agent_client_protocol::Error::invalid_params())?;
        notification
            .validate(phase)
            .map_err(|_| agent_client_protocol::Error::invalid_params())?;
        Ok((
            notification,
            metadata
                .as_ref()
                .map(|value| value.evidence)
                .unwrap_or_default(),
            metadata.map(|value| value.wire_bytes),
        ))
    }

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
                original_source: None,
                child_route: None,
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
    original_quota: Arc<StateMutex<OriginalQuotaCaptures>>,
    projection: StateMutex<std::sync::Weak<crate::acp::event_sender::ProjectionState>>,
    pub(super) fence: RwLock<()>,
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
    original: Option<Arc<OriginalCallbackGrant>>,
    history: Option<Arc<OriginalIngressHistory>>,
    callbacks_revoked: bool,
    /// Adapter frames dropped for another session id retain proxy numbering.
    discarded_adapter_frames: u64,
    native_autonomous: bool,
}

fn is_adapter_frame(notification: &SessionNotification) -> bool {
    !serde_json::to_value(&notification.update)
        .is_ok_and(|update| crate::acp::aoe_meta::is_anchored(&update))
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct ChildRoute {
    pub(super) parent: SessionId,
    pub(super) cancel_capable: bool,
    pub(super) source: Option<crate::acp::state::NativeEventOrigin>,
    pub(super) provenance: Option<Arc<crate::acp::event_sender::NativeChildProvenance>>,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum RequestPolicy {
    ParentOnly,
    ParentOrChild,
}

/// Only an admitted parent can announce a child; routes cannot re-parent or loop.
fn register_child(
    children: &mut HashMap<SessionId, ChildRoute>,
    root: Option<&SessionId>,
    notification: &SessionNotification,
    source: Option<crate::acp::state::NativeEventOrigin>,
) {
    let SessionUpdate::SubagentUpdate(update) = &notification.update else {
        return;
    };
    let parent = &notification.session_id;
    let child = &update.session_id;
    if (Some(parent) != root
        && !children
            .get(parent)
            .is_some_and(|route| route.source == source))
        || child == parent
        || Some(child) == root
    {
        return;
    }
    match children.get(child) {
        Some(existing) if existing.parent != *parent => return,
        None if children.len() >= MAX_CHILDREN => {
            tracing::warn!(target: "acp.protocol", child = %child.0, limit = MAX_CHILDREN,
                "subagent route limit reached; ignoring announcement");
            return;
        }
        _ => {}
    }
    let provenance = children
        .get(child)
        .filter(|route| route.source == source)
        .and_then(|route| route.provenance.clone())
        .or_else(|| {
            let origin = source?;
            let mut ancestry = if Some(parent) == root {
                vec![parent.clone()]
            } else {
                children.get(parent)?.provenance.as_ref()?.ancestry.to_vec()
            };
            ancestry.push(child.clone());
            Some(Arc::new(crate::acp::event_sender::NativeChildProvenance {
                origin,
                ancestry: ancestry.into(),
            }))
        });
    if source.is_some() && provenance.is_none() {
        return;
    }
    let route = children.entry(child.clone()).or_insert_with(|| ChildRoute {
        parent: parent.clone(),
        cancel_capable: false,
        source,
        provenance: None,
    });
    if route.source != source {
        route.cancel_capable = false;
        route.source = source;
    }
    route.cancel_capable = cancel_capable(update, route.cancel_capable);
    route.provenance = provenance;
}

/// Freeze the specific direct association, not a future lookup by session string.
fn freeze_child_route(
    children: &HashMap<SessionId, ChildRoute>,
    notification: &SessionNotification,
    source: Option<crate::acp::state::NativeEventOrigin>,
) -> Option<(SessionId, ChildRoute)> {
    let addressed = &notification.session_id;
    let child = match &notification.update {
        SessionUpdate::SubagentUpdate(update) => {
            let route = children.get(&update.session_id)?;
            (route.parent == *addressed).then_some(&update.session_id)?
        }
        SessionUpdate::SessionMessage(message) => {
            let (MaybeUndefined::Value(sender), MaybeUndefined::Value(recipient)) =
                (&message.sender_session_id, &message.recipient_session_id)
            else {
                return None;
            };
            let child = if sender == addressed {
                recipient
            } else if recipient == addressed {
                sender
            } else {
                return None;
            };
            (children.get(child)?.parent == *addressed).then_some(child)?
        }
        _ => addressed,
    };
    let route = children.get(child)?;
    (route.source == source && (source.is_none() || route.provenance.is_some()))
        .then(|| (child.clone(), route.clone()))
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
    updates: Vec<PendingIngress>,
    encoded_bytes: usize,
}

/// One actual original invocation on this authenticated control connection.
/// It permits existing reverse handlers, not input, adoption, or readiness.
type OriginalCallbackGrant = crate::acp::establishment::NativeCallbackOriginal;

pub(super) struct OriginalIngressHistory {
    /// Correlated result identity certifies history only, including signed Pending.
    result_session_id: std::sync::OnceLock<SessionId>,
    original: Arc<OriginalCallbackGrant>,
    pub(super) dedup: StateMutex<AgentMessageDedup>,
    pub(super) compaction: StateMutex<CompactionTracker>,
    pub(super) tool_context: ToolContextCache,
    constructor: std::sync::OnceLock<Arc<crate::acp::establishment::NativeConstructorObservation>>,
}

#[derive(Default)]
pub(super) struct OriginalQuotaCaptures {
    windows: Vec<OriginalWindowQuota>,
    reports: usize,
    name_bytes: usize,
}

struct OriginalWindowQuota {
    history: Arc<OriginalIngressHistory>,
    window_id: crate::acp::protocol::AcpPromptWindowId,
    rejections: HashMap<String, i64>,
}

impl OriginalQuotaCaptures {
    fn record(
        &mut self,
        history: &Arc<OriginalIngressHistory>,
        window_id: crate::acp::protocol::AcpPromptWindowId,
        meta: &Option<agent_client_protocol::v1::schema::Meta>,
    ) -> Result<(), ()> {
        use super::rate_limit::{rate_limit_rejection_from_meta, recovered_rate_limit_window};
        let index = self.windows.iter().position(|window| {
            window.window_id == window_id
                && window.history.original.same_source(
                    &history.original.attempt,
                    history.original.owner,
                    history.original.store.as_ref(),
                )
        });
        if let Some(rejection) = rate_limit_rejection_from_meta(meta) {
            let exists = index.is_some_and(|index| {
                self.windows[index]
                    .rejections
                    .contains_key(&rejection.window)
            });
            if !exists
                && (self.reports >= MAX_PENDING_UPDATES
                    || rejection.window.len() > MAX_PENDING_BYTES - self.name_bytes)
            {
                return Err(());
            }
            let index = match index {
                Some(index) => index,
                None => {
                    self.windows.push(OriginalWindowQuota {
                        history: Arc::clone(history),
                        window_id,
                        rejections: HashMap::new(),
                    });
                    self.windows.len() - 1
                }
            };
            if !exists {
                self.reports += 1;
                self.name_bytes += rejection.window.len();
            }
            self.windows[index]
                .rejections
                .insert(rejection.window, rejection.resets_at_secs);
            return Ok(());
        }
        if let (Some(index), Some(quota_window)) = (index, recovered_rate_limit_window(meta)) {
            if let Some((name, _)) = self.windows[index].rejections.remove_entry(quota_window) {
                self.reports -= 1;
                self.name_bytes -= name.len();
            }
            if self.windows[index].rejections.is_empty() {
                self.windows.swap_remove(index);
            }
        }
        Ok(())
    }

    fn take(
        &mut self,
        evidence: &crate::acp::control_protocol::NativePromptCompletionEvidence,
        now: crate::time::Timestamp,
    ) -> Option<super::rate_limit::CapturedReset> {
        let index = self.windows.iter().position(|window| {
            window.window_id == evidence.window_id
                && window.history.accepts_quota_completion(evidence)
        })?;
        let window = self.windows.swap_remove(index);
        self.reports -= window.rejections.len();
        self.name_bytes -= window.rejections.keys().map(String::len).sum::<usize>();
        super::rate_limit::latest_future_reset(window.rejections.iter(), now)
    }
}

impl std::fmt::Debug for OriginalIngressHistory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OriginalIngressHistory")
            .field("attempt", &self.original.attempt.id)
            .finish_non_exhaustive()
    }
}

impl OriginalIngressHistory {
    fn accepts_quota_completion(
        &self,
        evidence: &crate::acp::control_protocol::NativePromptCompletionEvidence,
    ) -> bool {
        let receipt = &evidence.receipt;
        let expected = crate::acp::state::NativeEventOrigin::owned_attempt(
            &self.original.attempt,
            Some(self.original.owner),
        );
        crate::acp::state::NativePhysicalSource::from_receipt(receipt)
            .is_some_and(|source| Some(source.witness) == expected)
            && self
                .original
                .same_source(&receipt.attempt, receipt.owner, receipt.store.as_ref())
            && receipt.replay.closed
            && !receipt.replay.lost
            && receipt
                .session_id()
                .is_some_and(|session| self.accepts_session(session))
            && evidence.scope.owner() == Some(self.original.owner)
            && evidence.scope.origin(Some(receipt)) == Some(evidence.origin)
            && evidence.scope.correlation().is_some()
    }

    pub(super) fn accepts_session(&self, session: &str) -> bool {
        self.original.known_session_id() == Some(session)
            || self
                .result_session_id
                .get()
                .is_some_and(|id| id.0.as_ref() == session)
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

#[derive(Debug, serde::Serialize)]
pub(super) struct IngressAutonomous {
    pub(super) event: crate::acp::state::Event,
    pub(super) metadata: crate::acp::event_sender::EventMetadata,
    #[serde(skip)]
    pub(super) constructor_scope:
        Option<Arc<crate::acp::establishment::NativeConstructorObservation>>,
    #[serde(skip)]
    wire_bytes: Option<usize>,
}

/// Bootstrap updates and private barriers share one ordered, bounded replay.
/// A callback buffers rather than blocking the SDK dispatcher on a commit
/// whose handshake response may still be behind it in the same shim.
#[derive(Debug, serde::Serialize)]
pub(super) enum PendingIngress {
    Update(Box<IngressUpdate>),
    Auth(Box<IngressAuth>),
    Autonomous {
        first: Box<IngressAutonomous>,
        second: Option<Box<IngressAutonomous>>,
    },
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
    /// A registered child asked for something only the root session may do.
    RootOnly,
    AwaitCommit,
    Establishing,
    Active,
}

impl SessionIngress {
    pub(super) fn with_original_quota(
        original_quota: Arc<StateMutex<OriginalQuotaCaptures>>,
    ) -> Self {
        Self {
            original_quota,
            ..Self::default()
        }
    }

    /// Captures original evidence without granting Current permission.
    pub(super) fn capture_original_rate_limit(&self, update: &IngressUpdate) -> bool {
        use crate::acp::protocol::AcpPromptObservation;
        if update.suppress_replay {
            return true;
        }
        let Some(history) = update
            .history
            .as_ref()
            .filter(|history| history.accepts_session(update.notification.session_id.0.as_ref()))
        else {
            return true;
        };
        let Some(physical) = update.evidence.physical_source else {
            return true;
        };
        if update.original_source != Some(physical.witness)
            || crate::acp::state::NativeEventOrigin::owned_attempt(
                &history.original.attempt,
                Some(history.original.owner),
            ) != Some(physical.witness)
        {
            return true;
        }
        let Some(AcpPromptObservation::Written { window_id }) = update.evidence.prompt_observation
        else {
            return true;
        };
        let SessionUpdate::UsageUpdate(usage) = &update.notification.update else {
            return true;
        };
        let recorded = self
            .original_quota
            .lock()
            .expect("original quota captures")
            .record(history, window_id, &usage.meta);
        if recorded.is_ok() {
            return true;
        }
        let mut state = self.state.lock().expect("ingress identity");
        if let Some(pending) = state.pending.as_mut() {
            pending.updates.clear();
            pending.encoded_bytes = 0;
        }
        state.failed = true;
        drop(state);
        self.projection_lost();
        self.changed.notify_waiters();
        self.failure.notify_one();
        false
    }

    /// Consumes only this authenticated original's exact Written window.
    pub(super) fn take_original_rate_limit_reset(
        &self,
        evidence: &crate::acp::control_protocol::NativePromptCompletionEvidence,
        now: crate::time::Timestamp,
    ) -> Option<super::rate_limit::CapturedReset> {
        self.original_quota
            .lock()
            .expect("original quota captures")
            .take(evidence, now)
    }

    pub(super) fn set_autonomous_negotiated(&self, enabled: bool) {
        self.state
            .lock()
            .expect("ingress identity")
            .native_autonomous = enabled;
    }

    pub(super) fn capture_autonomous(
        &self,
        phase: crate::acp::control_protocol::NativeAutonomousPhase,
        notification: crate::acp::control_protocol::NativeAutonomousNotification,
        evidence: crate::acp::control_protocol::NativeNotifyEvidence,
        wire_bytes: Option<usize>,
    ) -> Option<IngressAutonomous> {
        use crate::acp::event_sender::{
            EventMetadata, NativeAutonomousIdentity, NativeAutonomousMetadata,
        };
        let stamp = evidence.autonomous?;
        if stamp.phase != phase
            || stamp.run_id != notification.run_id
            || stamp.source != notification.source
            || stamp.ordinal == 0
        {
            return None;
        }
        let physical = evidence.physical_source?;
        let constructor = {
            let state = self.state.lock().expect("ingress identity");
            if !state.native_autonomous || state.failed || state.callbacks_revoked {
                return None;
            }
            let original = state.original.as_ref()?;
            if crate::acp::state::NativeEventOrigin::owned_attempt(
                &original.attempt,
                Some(original.owner),
            ) != Some(physical.witness)
            {
                return None;
            }
            let observed = evidence.session_stamp.or_else(|| {
                let pending = evidence.original_request_stamp?;
                (pending.attempt_id == original.attempt.id).then_some(
                    crate::acp::establishment::NativeSessionStamp {
                        context: pending.context,
                        generation: pending.generation,
                        profile_identity: pending.profile_identity,
                    },
                )
            })?;
            let root = SessionId::from(notification.session_id.clone());
            (
                state.history.as_ref()?.constructor.get()?.clone(),
                observed,
                root,
            )
        };
        let (constructor, observed, root) = constructor;
        let projection = self.projection.lock().expect("ingress projection").clone();
        let constructor = constructor
            .for_autonomous(observed, root.clone(), projection)
            .ok()?;
        let mut metadata = EventMetadata::history();
        metadata.native_origin = Some(physical.witness);
        metadata.reported_session_id = Some(root.clone());
        metadata.prompt_observation = evidence.prompt_observation;
        metadata.native_autonomous = Some(Arc::new(NativeAutonomousMetadata {
            identity: Arc::new(NativeAutonomousIdentity {
                origin: physical.witness,
                root_session_id: root,
                run_id: stamp.run_id,
                ordinal: stamp.ordinal,
                source: stamp.source,
            }),
            phase,
            stop_reason: notification.stop_reason.clone(),
            qualified: true,
        }));
        let event = match phase {
            crate::acp::control_protocol::NativeAutonomousPhase::Started => {
                crate::acp::state::Event::ThinkingStarted
            }
            crate::acp::control_protocol::NativeAutonomousPhase::Ended => {
                crate::acp::state::Event::Stopped {
                    reason: match notification.stop_reason.as_deref() {
                        Some("end_turn") => "agent_idle".into(),
                        Some(reason) => reason.into(),
                        None => "native_autonomous_ended".into(),
                    },
                }
            }
        };
        Some(IngressAutonomous {
            event,
            metadata,
            constructor_scope: Some(constructor),
            wire_bytes,
        })
    }

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
                result_session_id: std::sync::OnceLock::new(),
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
        self.projection
            .lock()
            .expect("ingress projection")
            .upgrade()?
            .original_window()?;
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
            if session != reported
                && !self
                    .child_route(&SessionId::from(reported.to_owned()))
                    .is_some_and(|route| route.source == Some(physical.witness))
            {
                return None;
            }
        }
        let parent_session = history.as_ref().and_then(|history| {
            history
                .result_session_id
                .get()
                .map(|session| Arc::clone(&session.0))
        });
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
            parent_session,
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
                    pending.candidate = Some(id.clone());
                }
            }
        }
        // Only the authenticated, correlated result for this frozen original
        // can certify its pre-result child history. This grants no Current scope.
        if !state.failed && generation.is_none_or(|generation| generation == state.generation) {
            if let Some(history) = &state.history {
                let receipt = self
                    .projection
                    .lock()
                    .expect("ingress projection")
                    .upgrade()
                    .and_then(|projection| projection.receipt_snapshot());
                if receipt.as_ref().is_some_and(|receipt| {
                    receipt.session_id() == Some(id.0.as_ref())
                        && history.original.same_source(
                            &receipt.attempt,
                            receipt.owner,
                            receipt.store.as_ref(),
                        )
                }) {
                    let _ = history.result_session_id.set(id);
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
        let unmanaged = self
            .projection
            .lock()
            .expect("ingress projection")
            .strong_count()
            == 0;
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
        let mut updates = pending;
        let mut discarded = 0;
        updates.retain_mut(|entry| match entry {
            PendingIngress::Update(update) => {
                let captured_source = update.original_source;
                let source_admitted = match update.evidence.physical_source {
                    Some(physical) => {
                        captured_source == Some(physical.witness)
                            && state.original.as_ref().is_some_and(|original| {
                                crate::acp::state::NativeEventOrigin::owned_attempt(
                                    &original.attempt,
                                    Some(original.owner),
                                ) == Some(physical.witness)
                            })
                    }
                    None => unmanaged,
                };
                let routed = source_admitted
                    && (Some(&update.notification.session_id) == id.as_ref()
                        || children
                            .get(&update.notification.session_id)
                            .is_some_and(|route| {
                                route.source
                                    == update.evidence.physical_source.map(|source| source.witness)
                            }));
                if routed {
                    register_child(
                        &mut children,
                        id.as_ref(),
                        &update.notification,
                        update.evidence.physical_source.map(|source| source.witness),
                    );
                    if update.child_route.is_none() {
                        update.child_route = freeze_child_route(
                            &children,
                            &update.notification,
                            update.evidence.physical_source.map(|source| source.witness),
                        );
                    }
                }
                if routed || update.evidence.physical_source.is_some() {
                    true
                } else {
                    if is_adapter_frame(&update.notification) {
                        discarded += 1;
                    }
                    false
                }
            }
            PendingIngress::PromptCompleted(_)
            | PendingIngress::NativeLost
            | PendingIngress::Replayed(_)
            | PendingIngress::Autonomous { .. }
            | PendingIngress::Auth(_) => true,
        });
        state.discarded_adapter_frames += discarded;
        state.current = id;
        state.children = children;
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
            PendingIngress::Autonomous { .. } => MAX_PENDING_BYTES,
            PendingIngress::Update(_) => unreachable!("updates use scoped admission"),
        };
        let mut count = PendingByteCount {
            written: 0,
            limit: marker_limit.min(MAX_PENDING_BYTES - pending.encoded_bytes),
        };
        let encoded = match &marker {
            PendingIngress::Autonomous { first, .. } if first.wire_bytes.is_some() => {
                first.wire_bytes.filter(|bytes| *bytes <= count.limit)
            }
            _ => serde_json::to_writer(&mut count, &marker)
                .ok()
                .map(|()| count.written),
        };
        if pending.updates.len() >= MAX_PENDING_UPDATES || encoded.is_none() {
            pending.updates.clear();
            pending.encoded_bytes = 0;
            state.failed = true;
            self.projection_lost();
            drop(state);
            self.changed.notify_waiters();
            self.failure.notify_one();
            return Err(Self::overflow_error());
        }
        pending.encoded_bytes += encoded.expect("checked bounded marker size");
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
            let mut state = self.state.lock().expect("ingress identity");
            let source = update
                .evidence
                .physical_source
                .map(|physical| physical.witness);
            update.original_source = state.original.as_ref().and_then(|original| {
                crate::acp::state::NativeEventOrigin::owned_attempt(
                    &original.attempt,
                    Some(original.owner),
                )
                .filter(|original| Some(*original) == source)
            });
            if update.original_source.is_some() && state.pending.is_none() {
                let IdentityState {
                    current, children, ..
                } = &mut *state;
                register_child(children, current.as_ref(), &update.notification, source);
                update.child_route = freeze_child_route(children, &update.notification, source);
            }
            state
                .history
                .as_ref()
                .filter(|history| {
                    update.evidence.physical_source.is_some_and(|physical| {
                        crate::acp::state::NativeEventOrigin::owned_attempt(
                            &history.original.attempt,
                            Some(history.original.owner),
                        ) == Some(physical.witness)
                    }) && (history.accepts_session(update.notification.session_id.0.as_ref())
                        || (state.pending.is_some()
                            && history.original.known_session_id().is_none()
                            && matches!(
                                history.original.attempt.target,
                                crate::acp::establishment::EstablishmentTarget::Fork { .. }
                            )))
                })
                .cloned()
        };
        update.suppress_replay = suppress_replay && update.history.is_some();
        update.current_scope = self.notification_scope(update.evidence).filter(|context| {
            context
                .receipt
                .as_ref()
                .and_then(|receipt| receipt.session_id())
                .is_some_and(|session| {
                    session == update.notification.session_id.0.as_ref()
                        || update.child_route.as_ref().is_some_and(|(_, route)| {
                            session == route.parent.0.as_ref() && context.origin() == route.source
                        })
                })
        });
        update.metadata_scope = matches!(
            &update.notification.update,
            agent_client_protocol::v1::schema::SessionUpdate::CurrentModeUpdate(_)
                | agent_client_protocol::v1::schema::SessionUpdate::UsageUpdate(_)
                | agent_client_protocol::v1::schema::SessionUpdate::AvailableCommandsUpdate(_)
                | agent_client_protocol::v1::schema::SessionUpdate::ConfigOptionUpdate(_)
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
        mut notification: IngressUpdate,
        wire_bytes: Option<usize>,
    ) -> agent_client_protocol::Result<Option<IngressUpdate>> {
        let unmanaged = self
            .projection
            .lock()
            .expect("ingress projection")
            .strong_count()
            == 0;
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
        let source_admitted = match notification.evidence.physical_source {
            Some(physical) => {
                notification.original_source == Some(physical.witness)
                    && state.original.as_ref().is_some_and(|original| {
                        crate::acp::state::NativeEventOrigin::owned_attempt(
                            &original.attempt,
                            Some(original.owner),
                        ) == Some(physical.witness)
                    })
            }
            None => unmanaged,
        };
        let routed = source_admitted
            && (state.current.as_ref() == Some(&notification.notification.session_id)
                || state
                    .children
                    .get(&notification.notification.session_id)
                    .is_some_and(|route| {
                        route.source
                            == notification
                                .evidence
                                .physical_source
                                .map(|source| source.witness)
                    }));
        if routed {
            let IdentityState {
                current, children, ..
            } = &mut *state;
            register_child(
                children,
                current.as_ref(),
                &notification.notification,
                notification
                    .evidence
                    .physical_source
                    .map(|source| source.witness),
            );
            if notification.child_route.is_none() {
                notification.child_route = freeze_child_route(
                    children,
                    &notification.notification,
                    notification
                        .evidence
                        .physical_source
                        .map(|source| source.witness),
                );
            }
        }
        if routed || notification.evidence.physical_source.is_some() {
            Ok(Some(notification))
        } else {
            if is_adapter_frame(&notification.notification) {
                state.discarded_adapter_frames += 1;
            }
            Ok(None)
        }
    }

    pub(super) fn take_discarded_adapter_frames(&self) -> u64 {
        std::mem::take(&mut self.state.lock().unwrap().discarded_adapter_frames)
    }

    /// The caller retains the ingress fence while inspecting this route.
    pub(super) fn child_route(&self, id: &SessionId) -> Option<ChildRoute> {
        self.state.lock().unwrap().children.get(id).cloned()
    }

    fn request_admission(
        &self,
        scope: &IngressRequestScope,
        id: &SessionId,
        policy: RequestPolicy,
    ) -> RequestAdmission {
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
        } else if state.children.get(id).is_some_and(|route| {
            route.source
                == crate::acp::state::NativeEventOrigin::owned_attempt(
                    &original.attempt,
                    Some(original.owner),
                )
        }) {
            match policy {
                RequestPolicy::ParentOrChild => RequestAdmission::Active,
                RequestPolicy::ParentOnly => RequestAdmission::RootOnly,
            }
        } else {
            RequestAdmission::Reject
        }
    }

    pub(super) fn request_refused() -> agent_client_protocol::Error {
        let mut error = agent_client_protocol::Error::invalid_params();
        error.message = "native callback scope is no longer active".into();
        error
    }

    /// Callback retirement, not responder-map removal, releases this marker.
    pub(super) fn park(self: &Arc<Self>) -> ParkedRequest {
        self.parked.fetch_add(1, Ordering::AcqRel);
        ParkedRequest(Arc::clone(self))
    }

    pub(super) fn has_parked(&self) -> bool {
        self.parked.load(Ordering::Acquire) > 0
    }

    pub(super) async fn request(
        &self,
        scope: &IngressRequestScope,
        id: &SessionId,
        policy: RequestPolicy,
    ) -> agent_client_protocol::Result<Option<RwLockReadGuard<'_, ()>>> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            match self.request_admission(scope, id, policy) {
                RequestAdmission::Reject => return Err(Self::request_refused()),
                RequestAdmission::RootOnly => {
                    let mut error = agent_client_protocol::Error::invalid_params();
                    error.message = "subagent sessions cannot make this request".into();
                    return Err(error);
                }
                RequestAdmission::AwaitCommit => changed.await,
                RequestAdmission::Establishing => return Ok(None),
                RequestAdmission::Active => {
                    let guard = self.fence.read().await;
                    if self.request_admission(scope, id, policy) == RequestAdmission::Active {
                        return Ok(Some(guard));
                    }
                }
            }
        }
    }

    async fn request_invalid(
        &self,
        scope: &IngressRequestScope,
        id: &SessionId,
        policy: RequestPolicy,
    ) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if matches!(
                self.request_admission(scope, id, policy),
                RequestAdmission::Reject | RequestAdmission::RootOnly
            ) {
                return;
            }
            changed.await;
        }
    }

    pub(super) async fn run_request<T>(
        self: &Arc<Self>,
        scope: IngressRequestScope,
        id: &SessionId,
        policy: RequestPolicy,
        work: impl Future<Output = agent_client_protocol::Result<T>>,
    ) -> agent_client_protocol::Result<T> {
        let guard = self.request(&scope, id, policy).await?;
        let _parked = (policy == RequestPolicy::ParentOrChild).then(|| self.park());
        let original = self
            .state
            .lock()
            .unwrap()
            .original
            .clone()
            .ok_or_else(Self::request_refused)?;
        tokio::select! {
            biased;
            _ = self.request_invalid(&scope, id, policy) => Err(Self::request_refused()),
            result = async {
                let valid = tokio::task::spawn_blocking(move || original.validate()).await;
                if !matches!(valid, Ok(Ok(())))
                    || matches!(self.request_admission(&scope, id, policy), RequestAdmission::Reject | RequestAdmission::RootOnly)
                {
                    return Err(Self::request_refused());
                }
                if policy == RequestPolicy::ParentOrChild {
                    drop(guard);
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

pub(super) struct ParkedRequest(Arc<SessionIngress>);

impl Drop for ParkedRequest {
    fn drop(&mut self) {
        self.0.parked.fetch_sub(1, Ordering::AcqRel);
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
            &mut tailer.notification.update
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

    #[tokio::test]
    #[serial_test::serial]
    async fn admitted_callbacks_share_progress_but_exclude_identity_writes() {
        use futures_util::FutureExt as _;
        crate::acp::supervisor::test_support::managed::with_managed(|host| {
            Box::pin(async move {
                let directory = host.tempdir();
                let (config, peer) =
                    host.native_peer(directory.path().into(), "codex", "codex", |request| {
                        let result = match request["method"].as_str() {
                            Some("initialize") => {
                                serde_json::json!({"protocolVersion":1,"agentCapabilities":{}})
                            }
                            Some("session/new") => {
                                serde_json::json!({"sessionId":"shared-native-root"})
                            }
                            _ => serde_json::json!({}),
                        };
                        Some(
                            serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":result}),
                        )
                    });
                let mut client = host.spawn(config, "shared-native-admission").await.unwrap();
                client.ready().await;
                let control = client.actual().test_control();
                let ingress = Arc::clone(&control.ingress);
                let old = ingress.current().expect("actual Original result");
                let scope = ingress.capture_request();
                let request = ingress
                    .request(&scope, &old, RequestPolicy::ParentOnly)
                    .await
                    .unwrap()
                    .expect("committed Original admission");
                peer.send(
                    serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{
                        "sessionId":old,"update":{"sessionUpdate":"agent_message_chunk",
                        "content":{"type":"text","text":"progress while read admission is held"}}
                    }}),
                )
                .await;
                tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    loop {
                        match client.next_event().await {
                            Some(crate::acp::state::Event::AgentMessageChunk { text, .. })
                                if text == "progress while read admission is held" =>
                            {
                                break
                            }
                            Some(_) => {}
                            None => panic!("actual native stream ended before progress"),
                        }
                    }
                })
                .await
                .expect("native progress cannot wait for the held admission");
                let second = ingress
                    .request(&scope, &old, RequestPolicy::ParentOnly)
                    .now_or_never()
                    .expect("another admitted callback shares the session")
                    .unwrap()
                    .expect("committed Original admission");
                drop(second);
                let mut transition = std::pin::pin!(ingress.fence.write());
                assert!(futures_util::poll!(transition.as_mut()).is_pending());
                let mut stale =
                    std::pin::pin!(ingress.request(&scope, &old, RequestPolicy::ParentOnly));
                assert!(
                    futures_util::poll!(stale.as_mut()).is_pending(),
                    "queued writer excludes later readers"
                );
                drop(request);
                let transition = transition.await;
                ingress.revoke_requests();
                drop(transition);
                assert!(
                    stale.await.is_err(),
                    "old admission must be rechecked after the write"
                );
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn candidate_callback_waits_without_blocking_commit_write() {
        crate::acp::supervisor::test_support::managed::with_managed(|host| {
            Box::pin(async move {
                let directory = host.tempdir();
                let (config, _peer) =
                    host.native_peer(directory.path().into(), "codex", "codex", |request| {
                        let result = match request["method"].as_str() {
                            Some("initialize") => {
                                serde_json::json!({"protocolVersion":1,"agentCapabilities":{}})
                            }
                            Some("session/new") => {
                                serde_json::json!({"sessionId":"candidate-native-root"})
                            }
                            _ => serde_json::json!({}),
                        };
                        Some(
                            serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":result}),
                        )
                    });
                let client = host
                    .spawn(config, "candidate-native-admission")
                    .await
                    .unwrap();
                client.ready().await;
                let control = client.actual().test_control();
                let ingress = Arc::clone(&control.ingress);
                let id = ingress.current().expect("actual Original result");
                let original = ingress.state.lock().unwrap().original.clone().unwrap();
                let generation = {
                    let _guard = ingress.fence.write().await;
                    ingress.begin()
                };
                ingress.resolve(Some(generation), id.clone());
                let scope = ingress.capture_request();
                let mut request =
                    std::pin::pin!(ingress.request(&scope, &id, RequestPolicy::ParentOnly));
                assert!(futures_util::poll!(request.as_mut()).is_pending());
                let commit = ingress
                    .fence
                    .try_write()
                    .expect("AwaitCommit holds no read guard");
                assert!(ingress.authorize_original(
                    Arc::clone(&original.attempt),
                    original.owner,
                    original.store.clone(),
                    original.postimage.get().cloned()
                ));
                ingress.finish(Some(id.clone())).unwrap();
                assert!(
                    futures_util::poll!(request.as_mut()).is_pending(),
                    "callback cannot overtake commit replay"
                );
                drop(commit);
                let admitted = request
                    .await
                    .unwrap()
                    .expect("committed Original admission");
                drop(admitted);
            })
        })
        .await;
    }

    fn announce(parent: &str, child: &str) -> IngressUpdate {
        SessionIngressNotification::decode_update(
            serde_json::json!({
                "sessionId": parent,
                "update": { "sessionUpdate": "subagent_update", "sessionId": child },
            }),
            false,
        )
        .expect("subagent_update")
        .0
    }

    async fn admitted(ingress: &SessionIngress, n: IngressUpdate) -> bool {
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
    #[serial_test::serial]
    async fn requests_from_a_child_obey_the_policy() {
        crate::acp::supervisor::test_support::managed::with_managed(|host| {
            Box::pin(async move {
                let directory = host.tempdir();
                let (config, peer) =
                    host.native_peer(directory.path().into(), "codex", "codex", |request| {
                        let result = match request["method"].as_str() {
                            Some("initialize") => {
                                serde_json::json!({"protocolVersion":1,"agentCapabilities":{}})
                            }
                            Some("session/new") => {
                                serde_json::json!({"sessionId":"policy-native-root"})
                            }
                            _ => serde_json::json!({}),
                        };
                        Some(
                            serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":result}),
                        )
                    });
                let client = host.spawn(config, "child-native-policy").await.unwrap();
                client.ready().await;
                let control = client.actual().test_control();
                let ingress = Arc::clone(&control.ingress);
                let root = ingress.current().expect("actual Original result");
                let scope = ingress.capture_request();
                let child = SessionId::from("c1");
                assert!(ingress
                    .request(&scope, &child, RequestPolicy::ParentOrChild)
                    .await
                    .is_err());
                peer.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{
                "sessionId":root,"update":{"sessionUpdate":"subagent_update","sessionId":child}
            }})).await;
                tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    while ingress.child_route(&child).is_none() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("the actual native parent declares its child");
                assert!(ingress
                    .request(&scope, &child, RequestPolicy::ParentOrChild)
                    .await
                    .is_ok());
                let refused = ingress
                    .request(&scope, &child, RequestPolicy::ParentOnly)
                    .await
                    .map(|_| ())
                    .unwrap_err();
                assert_eq!(
                    refused.message,
                    "subagent sessions cannot make this request"
                );
                assert!(ingress
                    .request(&scope, &root, RequestPolicy::ParentOnly)
                    .await
                    .is_ok());
            })
        })
        .await;
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
        let ids: Vec<_> = replay
            .iter()
            .map(|entry| match entry {
                PendingIngress::Update(notification) => {
                    notification.notification.session_id.0.to_string()
                }
                _ => panic!("child replay contains only session updates"),
            })
            .collect();
        assert_eq!(ids, ["root", "c2", "c1"]);
        assert!(admitted(&ingress, notif("c1")).await);
        assert!(admitted(&ingress, notif("c2")).await);

        ingress.begin();
        ingress.finish(Some(SessionId::from("fresh"))).unwrap();
        assert!(!admitted(&ingress, notif("c1")).await);
    }
}
