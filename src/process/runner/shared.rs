//! State shared by the control accept loop and the agent stdout fanout: the bounded
//! daemon-bound queue, request correlation, and the runner-owned ACP handshake.

use super::jsonrpc::{
    parse_agent_call_outcome, parse_native_message, parse_prompt_outcome, NativeMessage,
};
use crate::acp::control_protocol::{
    self, ControlBody, HandshakeFailure, HandshakeFailureOrigin, NativeChannelAvailability,
    NativeChannelLossCause, NativeChannelLostMarker, NativeReplayDelivery, PromptCompletedMarker,
    SessionReplayed,
};
use crate::acp::establishment::{
    EstablishmentAttempt, EstablishmentBlockReason, EstablishmentPostimage,
    NativeEstablishmentOutcome, NativeEstablishmentReceipt,
};
use crate::process::worker_registry;
use agent_client_protocol::JsonRpcMessage;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWrite;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

#[cfg(test)]
#[path = "../../../tests/integration/common/managed_runner.rs"]
pub(crate) mod managed_runner;

#[derive(Clone, Copy)]
enum NativeCallbackWritePurpose {
    Decision,
    ProtocolRefusal,
}

#[derive(Clone)]
enum NativeWriteScope {
    Pending(Arc<EstablishmentAttempt>),
    Applied {
        attempt: Arc<EstablishmentAttempt>,
        postimage: Arc<EstablishmentPostimage>,
    },
    Operation {
        receipt: Arc<NativeEstablishmentReceipt>,
        permission: Arc<crate::acp::establishment::NativeOperationPermission>,
    },
    Callback {
        attempt: Arc<EstablishmentAttempt>,
        context: Arc<crate::acp::establishment::NativeRequestContext>,
        call_id: u64,
        purpose: NativeCallbackWritePurpose,
    },
}

#[derive(Clone)]
struct NativeWriteAuthority {
    scope: NativeWriteScope,
    fence: Arc<crate::session::NativeInputFence>,
    owner: crate::acp::runner_lifecycle::RunnerIdentity,
    request_scope: crate::acp::establishment::NativeRequestScope,
    attachment: Option<u64>,
}

impl NativeWriteAuthority {
    fn attempt(&self) -> &EstablishmentAttempt {
        match &self.scope {
            NativeWriteScope::Pending(attempt)
            | NativeWriteScope::Applied { attempt, .. }
            | NativeWriteScope::Callback { attempt, .. } => attempt,
            NativeWriteScope::Operation { receipt, .. } => &receipt.attempt,
        }
    }

    fn callback(&self) -> Option<(u64, &Arc<crate::acp::establishment::NativeRequestContext>)> {
        match &self.scope {
            NativeWriteScope::Callback {
                call_id, context, ..
            } => Some((*call_id, context)),
            _ => None,
        }
    }

    fn callback_decision(&self) -> Option<&crate::acp::establishment::NativeRequestContext> {
        match &self.scope {
            NativeWriteScope::Callback {
                context,
                purpose: NativeCallbackWritePurpose::Decision,
                ..
            } => Some(context),
            _ => None,
        }
    }

    fn registry_matches(
        &self,
        registry: &worker_registry::RegistryGuard<'_>,
    ) -> anyhow::Result<bool> {
        let record = registry.record()?.ok_or(crate::session::SessionGone)?;
        Ok(self.owner.birth_fingerprint.is_some()
            && self.owner.pid == std::process::id()
            && registry.stop()?.is_none()
            && record.session_id == self.attempt().session_id
            && record.source_profile.as_deref() == Some(self.attempt().profile.as_str())
            && crate::acp::runner_lifecycle::RunnerIdentity::from_record(&record) == self.owner)
    }

    fn matches(&self, row: &crate::session::Instance) -> bool {
        if row.ensure_startable().is_err() || !row.is_structured() || row.terminal_handoff.is_some()
        {
            return false;
        }
        match &self.scope {
            NativeWriteScope::Pending(attempt) => {
                matches!(&row.acp_establishment,
                    Some(crate::acp::establishment::AcpEstablishment::Pending { attempt: current })
                        if current.as_ref() == attempt.as_ref())
                    && attempt.allows_effects()
                    && attempt.destination.matches(row)
            }
            NativeWriteScope::Applied { postimage, .. } => {
                postimage.choice.matches(row)
                    && row.acp_establishment
                        == Some(crate::acp::establishment::AcpEstablishment::Applied {
                            attempt_id: postimage.attempt_id,
                        })
            }
            NativeWriteScope::Operation { permission, .. } => {
                permission.session.generation == row.lifecycle_generation
                    && permission.session.postimage.choice.matches(row)
                    && row.acp_establishment
                        == Some(crate::acp::establishment::AcpEstablishment::Applied {
                            attempt_id: permission.session.postimage.attempt_id,
                        })
            }
            NativeWriteScope::Callback { .. } => false,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NativeWriteOutcome {
    Written,
    Refused,
    Lost,
}

enum NativeWriteWait {
    Native,
    Registry,
    Callback,
    Ticket,
}

#[derive(Debug, Clone, Copy)]
enum EstablishmentWait {
    Handshake,
    Channel,
    Callbacks,
    Native,
    Registry,
}

enum EstablishmentAdmission {
    Installed {
        owner: crate::acp::runner_lifecycle::RunnerIdentity,
        fence: Arc<crate::session::NativeInputFence>,
        scope: crate::acp::establishment::NativeRequestScope,
    },
    Wait {
        reason: EstablishmentWait,
        previous: Option<NativeSessionCache>,
        store: Option<crate::session::ExecutionBinding>,
    },
}

enum NativeRequestFailure {
    NotIssued,
    OutcomeUnknown,
}

pub(super) struct RunnerShared {
    pub(super) control: Mutex<ControlChannel>,
    pub(super) control_wake: tokio::sync::Notify,
    pub(super) control_space: tokio::sync::Notify,
    /// Connection-bound frames and correlations are purged by this id on disconnect.
    pub(super) next_attachment_id: AtomicU64,
    pub(super) handshake: Mutex<RunnerHandshake>,
    /// Serializes handshake round trips without blocking stdout fanout.
    pub(super) handshake_gate: Mutex<()>,
    pub(super) next_req_id: AtomicI64,
    pub(super) pending_server_calls: Mutex<HashMap<u64, PendingServerCall>>,
    pub(super) next_call_id: AtomicU64,
    pub(super) registry_owner: Option<(String, u32)>,
    /// A durability failure makes further runner state unsafe to expose.
    pub(super) fatal: AtomicBool,
    pub(super) fatal_wake: tokio::sync::Notify,
    native_channel: std::sync::Mutex<NativeChannelState>,
    native_changed: tokio::sync::Notify,
    native_dispatch_finished: tokio::sync::Notify,
    #[cfg(test)]
    test_producer: std::sync::Mutex<Option<managed_runner::ProducerProbe>>,
    #[cfg(test)]
    test_native_registry_wait: std::sync::Mutex<Option<std::sync::mpsc::Sender<()>>>,
}

pub(super) struct PendingServerCall {
    pub(super) agent_id: serde_json::Value,
    pub(super) method: String,
    pub(super) attachment_id: u64,
    authority: Option<NativeWriteAuthority>,
    resolving: bool,
}

enum NativeReplyTarget {
    Handshake(tokio::sync::oneshot::Sender<HandshakeResponse>),
    Prompt,
    Agent { call_id: u64, attachment_id: u64 },
}

struct NativePendingCall {
    target: NativeReplyTarget,
    scope: crate::acp::establishment::NativeRequestScope,
    receipt: Option<Arc<NativeEstablishmentReceipt>>,
    write: NativeWriteProgress,
    window: Option<NativePromptWindow>,
}

struct NativePromptWindow {
    id: crate::acp::protocol::AcpPromptWindowId,
    uncertain_output: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NativeWriteProgress {
    Pending,
    Prefix(usize),
    Complete,
}

/// Cancellation never turns an unterminated physical request into a reusable
/// native stream. Fully accepted originals remain pending until response/EOF.
struct NativePendingRegistration<'a> {
    shared: &'a RunnerShared,
    id: i64,
}

impl Drop for NativePendingRegistration<'_> {
    fn drop(&mut self) {
        let partial = {
            let mut native = self
                .shared
                .native_channel
                .lock()
                .expect("native channel lock");
            match native.calls.get(&self.id).map(|call| call.write) {
                Some(NativeWriteProgress::Pending) => {
                    native.calls.remove(&self.id);
                    false
                }
                Some(NativeWriteProgress::Prefix(_)) => true,
                Some(NativeWriteProgress::Complete) | None => false,
            }
        };
        if partial {
            self.shared
                .observe_native_loss(NativeChannelLossCause::WriteFailed);
        }
    }
}

/// A handshake response whose following stdout dispatch waits for publication.
pub(super) struct HandshakeResponse {
    pub(super) value: serde_json::Value,
    pub(super) release: tokio::sync::oneshot::Sender<()>,
}

#[derive(Default)]
struct NativeChannelState {
    loss: Option<NativeChannelLossCause>,
    dispatching: bool,
    active_attachment: Option<u64>,
    /// These are the actual pending RPC records, not a second window registry.
    calls: HashMap<i64, NativePendingCall>,
    /// Immutable original custody for synchronous, pre-await ingress capture.
    receipt: Option<Arc<NativeEstablishmentReceipt>>,
    history_fence: Option<Arc<crate::session::NativeInputFence>>,
    postimage: Option<Arc<EstablishmentPostimage>>,
    /// Only this original's actual collector may fill its callback postimage.
    callback_original: Option<Arc<crate::acp::establishment::NativeCallbackOriginal>>,
}

struct NativeIngressFrame {
    notification: Option<(String, serde_json::Value)>,
    request: Option<(serde_json::Value, String, serde_json::Value)>,
    response: Option<(i64, NativePendingCall, serde_json::Value)>,
    evidence: control_protocol::NativeNotifyEvidence,
    callback_authority: Option<NativeWriteAuthority>,
}

/// There is one stdout dispatcher. Its accepted message may finish publication
/// after a concurrent stdin failure, but unrelated commands cannot borrow it.
struct NativeDispatch<'a>(&'a RunnerShared);

impl Drop for NativeDispatch<'_> {
    fn drop(&mut self) {
        self.0
            .native_channel
            .lock()
            .expect("native channel lock")
            .dispatching = false;
        self.0.native_dispatch_finished.notify_waiters();
        self.0.control_wake.notify_one();
    }
}

#[derive(Default)]
pub(super) struct RunnerHandshake {
    pub(super) initialized: Option<serde_json::Value>,
    pub(super) session: Option<NativeSessionCache>,
}

#[derive(Clone)]
pub(super) struct NativeSessionCache {
    pub(super) receipt: Arc<NativeEstablishmentReceipt>,
    /// None means issued with an unknown result. It never grants a second RPC.
    pub(super) result: Option<Result<Arc<serde_json::Value>, Arc<serde_json::Value>>>,
    pub(super) postimage: Option<Arc<EstablishmentPostimage>>,
    failure_origin: HandshakeFailureOrigin,
    local_failure: Option<Arc<HandshakeFailure>>,
    unknown: Arc<serde_json::Value>,
    collector_finished: bool,
}

impl NativeSessionCache {
    fn accepts_live_fresh(
        &self,
        attempt: &EstablishmentAttempt,
        store: Option<&crate::session::ExecutionBinding>,
        owner: crate::acp::runner_lifecycle::RunnerIdentity,
    ) -> bool {
        use crate::acp::establishment::{AuthorityPhase, EstablishmentOrigin, EstablishmentTarget};
        let Some(postimage) = self.postimage.as_ref() else {
            return false;
        };
        self.collector_finished
            && self.local_failure.is_none()
            && matches!(&self.result, Some(Ok(_)))
            && self.receipt.owner == owner
            && self.receipt.replay.closed
            && !self.receipt.replay.lost
            && self.receipt.session_id().is_some()
            && self.receipt.same_store(store)
            && postimage.attempt_id == self.receipt.attempt.id
            && attempt.id != self.receipt.attempt.id
            && attempt.origin == EstablishmentOrigin::Fresh
            && attempt.target == EstablishmentTarget::New
            && attempt.producer.is_some()
            && attempt.profile == self.receipt.attempt.profile
            && attempt.session_id == self.receipt.attempt.session_id
            && attempt.created_at == self.receipt.attempt.created_at
            && attempt.source_context == attempt.context
            && attempt.preimage == postimage.choice
            && attempt.preimage.authority.is_some_and(|authority| {
                authority.id == self.receipt.attempt.id
                    && authority.phase == AuthorityPhase::Applied
            })
    }

    pub(super) fn frame(
        &self,
        native_channel: NativeChannelAvailability,
        original_delivery: bool,
    ) -> ControlBody {
        match &self.result {
            Some(Ok(result)) => ControlBody::SessionReady {
                acp_session_id: self
                    .receipt
                    .session_id()
                    .expect("successful native result has a correlated identity")
                    .to_owned(),
                result: Arc::clone(result),
                native_channel,
                receipt: Arc::clone(&self.receipt),
                postimage: self.postimage.clone(),
                original_delivery,
                delivery: NativeReplayDelivery::original(self.receipt.replay),
                local_failure: self.local_failure.clone(),
            },
            outcome => ControlBody::EstablishmentFailed {
                receipt: Arc::clone(&self.receipt),
                error: match outcome {
                    Some(Err(error)) => Arc::clone(error),
                    _ => Arc::clone(&self.unknown),
                },
                origin: self.failure_origin,
                original_delivery,
                delivery: NativeReplayDelivery::original(self.receipt.replay),
            },
        }
    }
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
    PromptCompleted(i64),
    Handshake,
    ServerCall,
    ServerCallSettled,
    AgentReply,
    // Not `Notify`: shedding it would leave the daemon's session/load waiting.
    SessionReplayed,
    NativeLoss,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum NativePublication {
    Work,
    Dispatch,
    CachedReceipt,
    ReverseSettlement,
}

pub(super) struct QueuedControl {
    pub(super) id: u64,
    pub(super) scope: DeliveryScope,
    pub(super) kind: QueuedKind,
    pub(super) wire: Arc<[u8]>,
    replay: Option<control_protocol::NativeReplayFrame>,
}

#[derive(Default)]
pub(super) struct ControlChannel {
    pub(super) queue: VecDeque<QueuedControl>,
    pub(super) queued_bytes: usize,
    pub(super) active_attachment: Option<u64>,
    pub(super) session_announced: bool,
    /// Transport ordering only: the SDK has received its original scope or result.
    pub(super) callbacks_announced: bool,
    pub(super) next_entry_id: u64,
    pub(super) in_flight: Option<u64>,
    pub(super) last_prompt_completion: Option<Arc<[u8]>>,
    prompt_head: Option<control_protocol::NativePromptHead>,
    native_loss: Option<QueuedControl>,
    native_loss_delivered: Option<u64>,
    replay: Option<NativeReplayAccount>,
}

struct NativeReplayAccount {
    attempt_id: crate::acp::establishment::AttemptId,
    notifications: u64,
    open: bool,
    lost: bool,
}

impl ControlChannel {
    fn replay_delivery(&self, receipt: &NativeEstablishmentReceipt) -> NativeReplayDelivery {
        let mut delivery = NativeReplayDelivery {
            after: receipt.replay.notifications,
            through: receipt.replay.notifications,
            lost: receipt.replay.lost,
        };
        let mut previous: Option<u64> = None;
        for frame in self.queue.iter().filter_map(|entry| entry.replay) {
            if frame.attempt_id != receipt.attempt.id {
                continue;
            }
            if let Some(ordinal) = previous {
                if ordinal.checked_add(1) != Some(frame.ordinal) {
                    delivery.lost = true;
                }
            } else if let Some(after) = frame.ordinal.checked_sub(1) {
                delivery.after = after;
            } else {
                delivery.lost = true;
            }
            previous = Some(frame.ordinal);
        }
        if previous.is_some_and(|ordinal| ordinal != delivery.through)
            || self.replay.as_ref().is_none_or(|account| {
                account.attempt_id != receipt.attempt.id
                    || account.notifications != delivery.through
                    || account.open
                    || account.lost
            })
        {
            delivery.lost = true;
        }
        delivery
    }

    pub(super) fn remove_at(&mut self, index: usize) -> QueuedControl {
        let frame = self.queue.remove(index).expect("queue index exists");
        self.queued_bytes = self.queued_bytes.saturating_sub(frame.wire.len());
        frame
    }

    pub(super) fn make_room(&mut self, kind: QueuedKind, incoming_bytes: usize) -> bool {
        let management = matches!(kind, QueuedKind::Handshake | QueuedKind::SessionReplayed);
        let frame_limit = control_protocol::MAX_CONTROL_QUEUE_FRAMES
            + if management {
                control_protocol::CONTROL_BOOTSTRAP_RESERVED_FRAMES
            } else {
                0
            };
        loop {
            let frames_full = self.queue.len() >= frame_limit;
            let bytes_full = self
                .queued_bytes
                .saturating_add(incoming_bytes)
                .saturating_add(
                    self.last_prompt_completion
                        .as_ref()
                        .map_or(0, |wire| wire.len()),
                )
                > control_protocol::MAX_CONTROL_QUEUE_BYTES
                    - control_protocol::MAX_NATIVE_LOSS_FRAME_BYTES;
            if !frames_full && !bytes_full {
                return true;
            }
            // Announced identity lets actual writes drain the immutable backlog.
            if self.active_attachment.is_some() && self.session_announced {
                return false;
            }
            if management && frames_full && !bytes_full {
                return false;
            }
            let Some(index) = self.queue.iter().position(|frame| {
                frame.kind == QueuedKind::Notify && self.in_flight != Some(frame.id)
            }) else {
                return false;
            };
            let removed = self.remove_at(index);
            if let Some(frame) = removed.replay {
                if let Some(replay) = self
                    .replay
                    .as_mut()
                    .filter(|replay| replay.attempt_id == frame.attempt_id)
                {
                    replay.lost = true;
                }
            }
        }
    }

    pub(super) fn push(&mut self, scope: DeliveryScope, kind: QueuedKind, wire: Arc<[u8]>) {
        self.push_projected(scope, kind, wire, None);
    }

    fn push_projected(
        &mut self,
        scope: DeliveryScope,
        kind: QueuedKind,
        wire: Arc<[u8]>,
        replay: Option<control_protocol::NativeReplayFrame>,
    ) {
        let id = self.next_entry_id;
        self.next_entry_id = self.next_entry_id.wrapping_add(1);
        self.queued_bytes += wire.len();
        self.queue.push_back(QueuedControl {
            id,
            scope,
            kind,
            wire,
            replay,
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
            control: Mutex::new(ControlChannel::default()),
            control_wake: tokio::sync::Notify::new(),
            control_space: tokio::sync::Notify::new(),
            next_attachment_id: AtomicU64::new(1),
            handshake: Mutex::new(RunnerHandshake::default()),
            handshake_gate: Mutex::new(()),
            next_req_id: AtomicI64::new(RUNNER_REQUEST_ID_BASE),
            pending_server_calls: Mutex::new(HashMap::new()),
            next_call_id: AtomicU64::new(1),
            registry_owner,
            fatal: AtomicBool::new(false),
            fatal_wake: tokio::sync::Notify::new(),
            native_channel: std::sync::Mutex::new(NativeChannelState::default()),
            native_changed: tokio::sync::Notify::new(),
            native_dispatch_finished: tokio::sync::Notify::new(),
            #[cfg(test)]
            test_producer: std::sync::Mutex::new(None),
            #[cfg(test)]
            test_native_registry_wait: std::sync::Mutex::new(None),
        }
    }

    #[cfg(test)]
    pub(super) fn test_record_producer(&self, event: impl FnOnce() -> serde_json::Value) {
        if let Some(probe) = self.test_producer.lock().unwrap().as_mut() {
            probe.push(event());
        }
    }

    #[cfg(test)]
    fn test_capture_original_prompt(
        &self,
        attachment: u64,
        params: &serde_json::Value,
        scope: &crate::acp::establishment::NativeRequestScope,
    ) {
        if let Some(probe) = self.test_producer.lock().unwrap().as_mut() {
            if probe.original.is_none() {
                probe.original = Some(managed_runner::ObservedOriginalPrompt {
                    attachment,
                    params: params.clone(),
                    scope: scope.clone(),
                });
            }
            probe.push(serde_json::json!({
                "stage": "admitted",
                "attachment": attachment,
                "params": params,
                "scope": scope,
            }));
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

    pub(super) fn native_loss(&self) -> Option<NativeChannelLossCause> {
        self.native_channel
            .lock()
            .expect("native channel lock")
            .loss
    }

    pub(super) fn native_availability(&self) -> NativeChannelAvailability {
        match self.native_loss() {
            Some(cause) => NativeChannelAvailability::Unavailable { cause },
            None => NativeChannelAvailability::Available,
        }
    }

    /// First loss closes native admission synchronously. No async queue or
    /// ordinary backlog is needed to retain the observation.
    pub(super) fn observe_native_loss(&self, cause: NativeChannelLossCause) {
        let mut native = self.native_channel.lock().expect("native channel lock");
        if native.loss.is_some() {
            return;
        }
        native.loss = Some(cause);
        drop(native);
        self.native_changed.notify_waiters();
        self.control_space.notify_waiters();
        self.control_wake.notify_one();
    }

    pub(super) async fn wait_native_loss(&self) -> NativeChannelLossCause {
        loop {
            let changed = self.native_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if let Some(cause) = self.native_loss() {
                return cause;
            }
            changed.await;
        }
    }

    /// A reaped Native still owns its queued Original output.
    pub(super) async fn wait_native_delivery(&self) {
        if self.next_attachment_id.load(Ordering::Relaxed) == 1 {
            return;
        }
        self.wait_native_loss().await;
        self.wait_native_dispatch().await;
        loop {
            let delivered = self.control_space.notified();
            tokio::pin!(delivered);
            delivered.as_mut().enable();
            let channel = self.control.lock().await;
            if channel.queue.is_empty()
                && channel.in_flight.is_none()
                && channel.native_loss_delivered.is_some()
            {
                return;
            }
            drop(channel);
            delivered.await;
        }
    }

    async fn wait_native_dispatch(&self) {
        loop {
            let finished = self.native_dispatch_finished.notified();
            tokio::pin!(finished);
            finished.as_mut().enable();
            if !self
                .native_channel
                .lock()
                .expect("native channel lock")
                .dispatching
            {
                return;
            }
            finished.await;
        }
    }

    fn begin_native_dispatch(&self) -> Option<NativeDispatch<'_>> {
        let mut native = self.native_channel.lock().expect("native channel lock");
        if native.loss.is_some() {
            return None;
        }
        debug_assert!(!native.dispatching, "stdout has one dispatcher");
        native.dispatching = true;
        Some(NativeDispatch(self))
    }

    /// Internal retirement only: dropping correlations does not invent a peer
    /// error, permission decision, aborted prompt, or process-exit observation.
    pub(super) async fn retire_native_waiters(&self) {
        self.wait_native_loss().await;
        self.wait_native_dispatch().await;
        self.native_channel
            .lock()
            .expect("native channel lock")
            .calls
            .clear();
        self.pending_server_calls.lock().await.clear();
    }
    fn capture_native_frame(&self, line: &[u8]) -> NativeIngressFrame {
        let (notification, request, response_value) = match parse_native_message(line) {
            Some(NativeMessage::Notification { method, params }) => {
                (Some((method, params)), None, None)
            }
            Some(NativeMessage::Request {
                agent_id,
                method,
                params,
            }) => (None, Some((agent_id, method, params)), None),
            Some(NativeMessage::Response { id, value }) => (None, None, Some((id, value))),
            None => (None, None, None),
        };
        let update = notification.as_ref().and_then(|(method, params)| {
            (method == "session/update")
                .then_some(params)
                .and_then(|params| params.get("update").or_else(|| params.get(1)))
        });
        let update_kind = update
            .and_then(|update| update.get("sessionUpdate"))
            .and_then(serde_json::Value::as_str);
        let is_content = matches!(
            update_kind,
            Some(
                "agent_message_chunk"
                    | "agent_thought_chunk"
                    | "tool_call"
                    | "tool_call_update"
                    | "plan"
            )
        );
        let session_metadata = matches!(
            update_kind,
            Some(
                "usage_update"
                    | "config_option_update"
                    | "current_mode_update"
                    | "available_commands_update"
            )
        );
        let needs_session = matches!(
            update_kind,
            Some(
                "usage_update"
                    | "config_option_update"
                    | "tool_call"
                    | "tool_call_update"
                    | "current_mode_update"
                    | "available_commands_update"
                    | "session_info_update"
            )
        ) || update.is_some_and(crate::acp::acp_client::native_compaction_cue)
            || notification
                .as_ref()
                .is_some_and(|(method, _)| method == crate::acp::state::AUTH_STATUS_UPDATE_METHOD)
            || request.is_some();
        let resource_start = update_kind == Some("tool_call_update")
            && notification
                .as_ref()
                .and_then(|(_, params)| params.get("update").or_else(|| params.get(1)))
                .is_some_and(|update| {
                    crate::acp::acp_client::native_resource_start_cue(update).is_some()
                });
        let reported_session = notification
            .as_ref()
            .map(|(_, params)| params)
            .or_else(|| request.as_ref().map(|(_, _, params)| params))
            .and_then(|params| params.get("sessionId").or_else(|| params.get(0)))
            .and_then(serde_json::Value::as_str);
        let source = if needs_session || resource_start {
            let native = self.native_channel.lock().expect("native channel lock");
            native
                .receipt
                .as_ref()
                .filter(|receipt| {
                    receipt
                        .session_id()
                        .is_some_and(|sid| reported_session.is_none_or(|reported| reported == sid))
                        || (request.is_some()
                            && receipt.session_id().is_none()
                            && native.calls.values().any(|call| {
                                call.write == NativeWriteProgress::Complete
                                    && matches!(&call.target, NativeReplyTarget::Handshake(_))
                                    && call
                                        .receipt
                                        .as_ref()
                                        .is_some_and(|original| Arc::ptr_eq(original, receipt))
                                    && call.scope.origin(Some(receipt))
                                        == crate::acp::state::NativeEventOrigin::owned_attempt(
                                            &receipt.attempt,
                                            Some(receipt.owner),
                                        )
                            }))
                })
                .zip(native.history_fence.as_ref())
                .map(|(receipt, fence)| {
                    (
                        Arc::clone(receipt),
                        Arc::clone(fence),
                        native.postimage.clone(),
                    )
                })
        } else {
            None
        };
        let original_io = request.as_ref().is_some_and(|(_, method, _)| {
            method.starts_with("fs/") || method.starts_with("terminal/")
        });
        // Original I/O takes its effect guards outside the stdout reader.
        let held =
            source
                .as_ref()
                .filter(|_| !original_io)
                .and_then(|(receipt, fence, postimage)| {
                    let capture = || {
                        let (physical, stamps) = fence
                    .lock_original_history_project(|row, storage| {
                        // Select purpose from the checked durable phase, never
                        // from failure to reconstruct Current authorization.
                        if matches!(
                            row.acp_establishment,
                            Some(crate::acp::establishment::AcpEstablishment::Pending { .. })
                        ) {
                            let original_callback = request.as_ref().is_some_and(|(_, method, _)| {
                                matches!(
                                    method.as_str(),
                                    "session/request_permission" | "elicitation/create"
                                )
                            });
                            let resolved_metadata = session_metadata
                                && receipt.replay.closed
                                && !receipt.replay.lost
                                && receipt.session_id().is_some()
                                && receipt.session_id() == reported_session
                                && postimage.as_deref().is_some_and(|postimage| {
                                    receipt
                                        .postimage(receipt.attempt.context)
                                        .is_ok_and(|expected| &expected == postimage)
                                });
                            anyhow::ensure!(
                                original_callback || resolved_metadata,
                                crate::acp::establishment::EstablishmentRefusal::Unconfirmed
                            );
                            crate::acp::establishment::OriginalRequestStamp::capture(
                                receipt,
                                row,
                                storage,
                                reported_session,
                            )
                            .map(|stamp| (None, Some(stamp)))
                        } else {
                            let postimage = postimage.as_deref().ok_or(
                                crate::acp::establishment::EstablishmentRefusal::Unconfirmed,
                            )?;
                            crate::acp::establishment::NativeSessionPermission::capture_stamp(
                                receipt, postimage, row, storage,
                            )
                            .map(|stamp| (Some(stamp), None))
                        }
                    })
                    .ok()??;
                        let registry = loop {
                            if let Some(registry) =
                                worker_registry::try_registry_lock(&receipt.attempt.session_id)
                                    .ok()?
                            {
                                break registry;
                            }
                            #[cfg(test)]
                            if let Some(waiting) =
                                self.test_native_registry_wait.lock().unwrap().take()
                            {
                                let _ = waiting.send(());
                            }
                            // Keep the selected phase and stamps under lifecycle exclusion.
                            drop(
                                worker_registry::acquire_registry_lock(&receipt.attempt.session_id)
                                    .ok()?,
                            );
                        };
                        crate::acp::establishment::NativeSessionPermission::check_stamp_owner(
                            receipt, &registry,
                        )
                        .ok()?;
                        Some((physical, registry, stamps))
                    };
                    // Framing precedes queue admission. Only sparse state and compaction
                    // lifecycle observations enter this blocking capability check.
                    match tokio::runtime::Handle::try_current()
                        .map(|handle| handle.runtime_flavor())
                    {
                        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => {
                            tokio::task::block_in_place(capture)
                        }
                        _ => capture(),
                    }
                });
        let mut native = self.native_channel.lock().expect("native channel lock");
        let source_current = source.as_ref().is_some_and(|(receipt, fence, postimage)| {
            native.loss.is_none()
                && native
                    .receipt
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, receipt))
                && native
                    .history_fence
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, fence))
                && match (&native.postimage, postimage) {
                    (Some(current), Some(captured)) => Arc::ptr_eq(current, captured),
                    (None, None) => true,
                    _ => false,
                }
        });
        let captured_stamp = source_current
            .then(|| held.as_ref().and_then(|(_, _, stamps)| stamps.0))
            .flatten();
        let session_stamp = needs_session.then_some(captured_stamp).flatten();
        let resource_start_stamp = resource_start.then_some(captured_stamp).flatten();
        let original_request_stamp = source_current
            .then(|| held.as_ref().and_then(|(_, _, stamps)| stamps.1))
            .flatten();
        let callback_original = source_current
            .then(|| native.callback_original.clone())
            .flatten();
        let physical_receipt = if request.is_some() {
            source
                .as_ref()
                .filter(|(receipt, _, _)| source_current && !receipt.replay.lost)
                .map(|(receipt, _, _)| receipt.as_ref())
        } else {
            native.receipt.as_deref()
        };
        let physical_source = native
            .loss
            .is_none()
            .then(|| {
                physical_receipt.and_then(crate::acp::state::NativePhysicalSource::from_receipt)
            })
            .flatten();
        let mut first_window = None;
        let mut windows = 0;
        let available = native.loss.is_none();
        for call in native.calls.values_mut() {
            if !available || call.write != NativeWriteProgress::Complete {
                continue;
            }
            let Some(receipt) = call.receipt.as_ref() else {
                continue;
            };
            if reported_session.is_none() || receipt.session_id() != reported_session {
                continue;
            }
            if call.scope.origin(Some(receipt)).is_none() {
                continue;
            }
            let Some(window) = call.window.as_mut() else {
                continue;
            };
            windows += 1;
            first_window.get_or_insert(window.id);
            if is_content {
                window.uncertain_output = true;
            }
        }
        let prompt_observation = reported_session.map(|_| match windows {
            0 => crate::acp::protocol::AcpPromptObservation::NoWrittenPrompt,
            1 => crate::acp::protocol::AcpPromptObservation::Written {
                window_id: first_window.expect("one qualified written original"),
            },
            _ => crate::acp::protocol::AcpPromptObservation::Ambiguous,
        });
        let response = response_value
            .and_then(|(id, value)| native.calls.remove(&id).map(|call| (id, call, value)));
        let premature = response
            .as_ref()
            .is_some_and(|(_, call, _)| call.write != NativeWriteProgress::Complete);
        drop(native);
        drop(held);
        let callback_authority = request
            .as_ref()
            .filter(|(_, method, _)| {
                matches!(
                    method.as_str(),
                    "session/request_permission" | "elicitation/create"
                ) && (session_stamp.is_some() || original_request_stamp.is_some())
            })
            .and_then(|_| {
                let (receipt, fence, _) = source.as_ref()?;
                let context = crate::acp::establishment::NativeRequestContext::callback(
                    callback_original?,
                    session_stamp,
                    original_request_stamp,
                    reported_session.map(Arc::from),
                )
                .ok()?;
                Some(NativeWriteAuthority {
                    scope: NativeWriteScope::Callback {
                        attempt: Arc::clone(&receipt.attempt),
                        context: Arc::new(context),
                        call_id: 0,
                        purpose: NativeCallbackWritePurpose::Decision,
                    },
                    fence: Arc::clone(fence),
                    owner: receipt.owner,
                    request_scope: crate::acp::establishment::NativeRequestScope::Observation,
                    attachment: None,
                })
            });
        if premature {
            self.observe_native_loss(NativeChannelLossCause::ProtocolEnded);
        }
        NativeIngressFrame {
            notification,
            request,
            response: response.filter(|_| !premature),
            callback_authority,
            evidence: control_protocol::NativeNotifyEvidence {
                physical_source,
                session_stamp,
                original_request_stamp,
                resource_start_stamp,
                prompt_observation,
            },
        }
    }

    pub(super) fn deliver_line<'a>(
        &'a self,
        line: &'a [u8],
        agent_stdin: &'a Mutex<tokio::process::ChildStdin>,
    ) -> impl std::future::Future<Output = ()> + Send + 'a {
        let dispatch = self.begin_native_dispatch();
        let captured = dispatch.as_ref().map(|_| self.capture_native_frame(line));
        async move {
            let Some(_dispatch) = dispatch else {
                return;
            };
            self.deliver_captured(agent_stdin, captured.expect("accepted native frame"))
                .await;
        }
    }

    async fn deliver_captured(
        &self,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        captured: NativeIngressFrame,
    ) {
        let NativeIngressFrame {
            notification,
            request,
            response,
            callback_authority,
            evidence,
        } = captured;
        if let Some((id, pending, value)) = response {
            match pending.target {
                NativeReplyTarget::Handshake(tx) => {
                    let (release, released) = tokio::sync::oneshot::channel();
                    let _ = tx.send(HandshakeResponse { value, release });
                    // Original handshake publication precedes the next callback.
                    let _ = released.await;
                }
                NativeReplyTarget::Prompt => match parse_prompt_outcome(value) {
                    Ok(outcome) => {
                        let completion = pending
                            .scope
                            .origin(pending.receipt.as_deref())
                            .zip(pending.window)
                            .zip(pending.receipt)
                            .map(|((origin, window), receipt)| {
                                Box::new(control_protocol::NativePromptCompletionEvidence {
                                    origin,
                                    prompt_req_id: id,
                                    scope: pending.scope,
                                    receipt,
                                    window_id: window.id,
                                    uncertain_output: window.uncertain_output,
                                })
                            });
                        self.enqueue_native(
                            DeliveryScope::Persistent,
                            QueuedKind::PromptCompleted(id),
                            ControlBody::PromptCompleted {
                                prompt_req_id: id,
                                outcome,
                                evidence: completion,
                            },
                            NativePublication::Dispatch,
                        )
                        .await;
                    }
                    Err(_) => self.observe_native_loss(NativeChannelLossCause::ProtocolEnded),
                },
                NativeReplyTarget::Agent {
                    call_id,
                    attachment_id,
                } => {
                    let frame = match parse_agent_call_outcome(value) {
                        Ok(Ok(result)) => ControlBody::AgentResult { call_id, result },
                        Ok(Err(error)) => ControlBody::AgentError { call_id, error },
                        Err(_) => {
                            self.observe_native_loss(NativeChannelLossCause::ProtocolEnded);
                            return;
                        }
                    };
                    self.enqueue_native(
                        DeliveryScope::Attachment(attachment_id),
                        QueuedKind::AgentReply,
                        frame,
                        NativePublication::Dispatch,
                    )
                    .await;
                }
            }
            return;
        }

        if let Some((agent_id, method, params)) = request {
            self.forward_server_call(
                agent_id,
                method,
                params,
                agent_stdin,
                evidence,
                callback_authority,
            )
            .await;
            return;
        }

        if let Some((method, params)) = notification {
            if SessionReplayed::matches_method(&method)
                || PromptCompletedMarker::matches_method(&method)
                || NativeChannelLostMarker::matches_method(&method)
            {
                return;
            }
            let replay = {
                let mut channel = self.control.lock().await;
                channel
                    .replay
                    .as_mut()
                    .filter(|replay| replay.open)
                    .and_then(|replay| match replay.notifications.checked_add(1) {
                        Some(ordinal) => {
                            replay.notifications = ordinal;
                            Some(control_protocol::NativeReplayFrame {
                                attempt_id: replay.attempt_id,
                                ordinal,
                            })
                        }
                        None => {
                            replay.lost = true;
                            None
                        }
                    })
            };
            let enqueued = self
                .enqueue_native(
                    DeliveryScope::Persistent,
                    QueuedKind::Notify,
                    ControlBody::Notify {
                        method,
                        params,
                        replay,
                        evidence,
                    },
                    NativePublication::Dispatch,
                )
                .await;
            if !enqueued {
                let mut channel = self.control.lock().await;
                if let Some(account) = channel.replay.as_mut().filter(|account| {
                    replay.is_some_and(|frame| frame.attempt_id == account.attempt_id)
                }) {
                    account.lost = true;
                }
            }
        }
    }

    async fn forward_server_call(
        &self,
        agent_id: serde_json::Value,
        method: String,
        params: serde_json::Value,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        evidence: control_protocol::NativeNotifyEvidence,
        mut authority: Option<NativeWriteAuthority>,
    ) {
        let Some(attachment_id) = self.control.lock().await.active_attachment else {
            self.answer_agent(
                agent_stdin,
                &agent_id,
                disconnected_server_call_outcome(&method),
            )
            .await;
            return;
        };
        let session_scoped = control_protocol::session_scoped_callback(&method, &params);
        if session_scoped {
            loop {
                let changed = self.control_space.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                let usable = match self.handshake.lock().await.session.as_ref() {
                    Some(cache) => match &cache.result {
                        // A pending constructor can itself depend on this callback.
                        None => break,
                        Some(result) => result.is_ok(),
                    },
                    None => false,
                };
                if !usable {
                    self.answer_agent(
                        agent_stdin,
                        &agent_id,
                        Err(control_protocol::JsonRpcError::new(
                            -32602,
                            "the original native callback scope is unavailable",
                        )),
                    )
                    .await;
                    return;
                }
                {
                    let channel = self.control.lock().await;
                    if channel.active_attachment != Some(attachment_id)
                        || channel.callbacks_announced
                        || self.native_loss().is_some()
                    {
                        break;
                    }
                }
                // Closed results precede their following callbacks in the shim.
                changed.await;
            }
        }
        if self.native_loss().is_some() {
            return;
        }
        let call_id = self.next_call_id.fetch_add(1, Ordering::Relaxed);
        if let Some(authority) = &mut authority {
            authority.attachment = Some(attachment_id);
            let NativeWriteScope::Callback {
                call_id: ticket, ..
            } = &mut authority.scope
            else {
                unreachable!("only frozen callbacks accompany reverse requests");
            };
            *ticket = call_id;
        }
        let body = ControlBody::ServerCall {
            call_id,
            method: method.clone(),
            params,
            evidence,
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
        if self.native_loss().is_some() {
            return;
        }
        if pending.len() >= MAX_OUTSTANDING_REQUESTS
            || !channel.make_room(QueuedKind::ServerCall, wire.len())
        {
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
                authority,
                resolving: false,
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
        self.answer_agent_scoped(agent_stdin, agent_id, outcome, None)
            .await
    }

    async fn answer_agent_scoped(
        &self,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        agent_id: &serde_json::Value,
        outcome: Result<serde_json::Value, control_protocol::JsonRpcError>,
        authority: Option<&NativeWriteAuthority>,
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
        match authority {
            Some(authority) => {
                self.write_agent_line_scoped(agent_stdin, &response, Some(authority))
                    .await
                    == NativeWriteOutcome::Written
            }
            None => self.write_agent_line(agent_stdin, &response).await,
        }
    }

    /// False requires closing this attachment: its terminal settlement could not queue.
    pub(super) async fn resolve_server_call(
        &self,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        attachment_id: u64,
        call_id: u64,
        outcome: Result<serde_json::Value, control_protocol::JsonRpcError>,
        session_id: &str,
    ) -> bool {
        let pending = {
            let mut pending = self.pending_server_calls.lock().await;
            match pending.get_mut(&call_id) {
                Some(call) if call.attachment_id == attachment_id && !call.resolving => {
                    call.resolving = true;
                    Some((
                        call.agent_id.clone(),
                        call.method.clone(),
                        call.authority.clone(),
                    ))
                }
                _ => None,
            }
        };
        let Some((agent_id, method, mut authority)) = pending else {
            debug!(
                target: "acp.runner",
                session = %session_id,
                call_id,
                "ignoring answer for an unknown or detached reverse call"
            );
            return true;
        };
        let human_decision = matches!(
            method.as_str(),
            "session/request_permission" | "elicitation/create"
        );
        if outcome.is_err() {
            if let Some(NativeWriteAuthority {
                scope: NativeWriteScope::Callback { purpose, .. },
                ..
            }) = &mut authority
            {
                // Refusal retains the exact physical ticket and attachment, not decision authority.
                *purpose = NativeCallbackWritePurpose::ProtocolRefusal;
            }
        }
        let written = if human_decision && authority.is_none() && outcome.is_ok() {
            false
        } else {
            self.answer_agent_scoped(agent_stdin, &agent_id, outcome, authority.as_ref())
                .await
        };
        self.pending_server_calls.lock().await.remove(&call_id);
        self.control_space.notify_waiters();
        if !written {
            warn!(
                target: "acp.runner",
                session = %session_id,
                call_id,
                method = %method,
                "agent stdin write failed answering a reverse call"
            );
        }
        // Release daemon callback custody only after the actual write attempt ends.
        // This reports settlement, not successful delivery or decision authority.
        self.enqueue_native(
            DeliveryScope::Attachment(attachment_id),
            QueuedKind::ServerCallSettled,
            ControlBody::ServerCallSettled { call_id },
            NativePublication::ReverseSettlement,
        )
        .await
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
            channel.purge_attachment(attachment_id);
            drained
        };
        self.control_space.notify_waiters();
        if self.native_loss().is_some() {
            return;
        }
        if !drained.is_empty() {
            info!(
                target: "acp.runner",
                session = %session_id,
                count = drained.len(),
                "synthesising responses for reverse calls on control disconnect"
            );
        }
        for (_, pending) in drained {
            if pending.resolving {
                continue;
            }
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

    pub(super) async fn enqueue(
        &self,
        scope: DeliveryScope,
        kind: QueuedKind,
        body: ControlBody,
    ) -> bool {
        self.enqueue_native(scope, kind, body, NativePublication::Work)
            .await
    }

    /// Only current dispatch, cached management receipts, and terminal reverse
    /// settlements may publish after loss. None permits new native work.
    async fn enqueue_native(
        &self,
        scope: DeliveryScope,
        kind: QueuedKind,
        mut body: ControlBody,
        publication: NativePublication,
    ) -> bool {
        if publication == NativePublication::Work && self.native_loss().is_some() {
            return false;
        }
        let announces_session = matches!(&body,
            ControlBody::SessionReady { receipt, .. }
            | ControlBody::EstablishmentFailed { receipt, .. } if receipt.replay.closed);
        let replay = match &body {
            ControlBody::Notify { replay, .. } => *replay,
            _ => None,
        };
        let mut wire: Arc<[u8]> = match control_protocol::encode_frame(&body) {
            Ok(frame) => frame.into(),
            Err(error) => {
                warn!(target: "acp.runner", %error, "dropping unrepresentable control frame");
                return false;
            }
        };
        loop {
            let space = self.control_space.notified();
            tokio::pin!(space);
            space.as_mut().enable();
            {
                let mut channel = self.control.lock().await;
                if let DeliveryScope::Attachment(attachment_id) = scope {
                    if channel.active_attachment != Some(attachment_id) {
                        return false;
                    }
                }
                let native = self.native_channel.lock().expect("native channel lock");
                if let Some(cause) = native.loss {
                    if publication == NativePublication::Work {
                        return false;
                    }
                    if let ControlBody::SessionReady { native_channel, .. } = &mut body {
                        let unavailable = NativeChannelAvailability::Unavailable { cause };
                        if *native_channel != unavailable {
                            *native_channel = unavailable;
                            drop(native);
                            drop(channel);
                            // Only a concurrent loss changes an already encoded
                            // receipt. Do not serialize a large result under the
                            // synchronous admission lock.
                            wire = match control_protocol::encode_frame(&body) {
                                Ok(frame) => frame.into(),
                                Err(_) => return false,
                            };
                            continue;
                        }
                    }
                }
                if channel.make_room(kind, wire.len()) {
                    channel.push_projected(scope, kind, Arc::clone(&wire), replay);
                    #[cfg(test)]
                    self.test_record_producer(|| serde_json::json!({
                        "stage": "queued",
                        "body": serde_json::from_slice::<serde_json::Value>(&wire[4..]).unwrap(),
                    }));
                    if announces_session {
                        channel.session_announced = true;
                        channel.callbacks_announced =
                            matches!(&body, ControlBody::SessionReady { .. });
                    }
                    drop(native);
                    #[cfg(debug_assertions)]
                    if matches!(kind, QueuedKind::PromptCompleted(_)) {
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
                if publication == NativePublication::ReverseSettlement {
                    // Do not park daemon callback custody behind a full queue.
                    // The caller closes this exact attachment on failure.
                    return false;
                }
                if kind == QueuedKind::Notify
                    && !(channel.active_attachment.is_some()
                        && channel.session_announced
                        && !channel.queue.is_empty())
                {
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
        mut frame: ControlBody,
        established: bool,
        before_loss: bool,
    ) {
        let scope = DeliveryScope::Attachment(attachment_id);
        // Freeze the outstanding original range before either awaited enqueue.
        // In-flight entries remain in this queue until a successful socket flush.
        let replay = match &mut frame {
            ControlBody::SessionReady {
                receipt, delivery, ..
            } if established && receipt.replay.closed => {
                *delivery = self.control.lock().await.replay_delivery(receipt);
                Some(SessionReplayed {
                    attempt_id: receipt.attempt.id,
                    closure: receipt.replay,
                    delivery: *delivery,
                })
            }
            ControlBody::EstablishmentFailed {
                receipt, delivery, ..
            } if receipt.replay.closed => {
                *delivery = self.control.lock().await.replay_delivery(receipt);
                Some(SessionReplayed {
                    attempt_id: receipt.attempt.id,
                    closure: receipt.replay,
                    delivery: *delivery,
                })
            }
            _ => None,
        };
        let publication = if before_loss {
            NativePublication::Dispatch
        } else if matches!(
            &frame,
            ControlBody::Initialized { .. }
                | ControlBody::SessionReady { .. }
                | ControlBody::EstablishmentFailed { .. }
        ) {
            NativePublication::CachedReceipt
        } else {
            NativePublication::Work
        };
        if self
            .enqueue_native(scope, QueuedKind::Handshake, frame, publication)
            .await
        {
            if let Some(marker) = replay {
                self.enqueue_native(
                    scope,
                    QueuedKind::SessionReplayed,
                    ControlBody::Notify {
                        method: marker.method().into(),
                        params: serde_json::to_value(marker)
                            .expect("private replay marker serializes"),
                        replay: None,
                        evidence: control_protocol::NativeNotifyEvidence::default(),
                    },
                    publication,
                )
                .await;
            }
        }
    }

    pub(super) async fn begin_attachment(&self) -> u64 {
        let attachment_id = self.next_attachment_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut channel = self.control.lock().await;
            self.native_channel
                .lock()
                .expect("native channel lock")
                .active_attachment = Some(attachment_id);
            channel.active_attachment = Some(attachment_id);
            channel.session_announced = false;
            channel.callbacks_announced = false;
        }
        self.control_space.notify_waiters();
        self.control_wake.notify_one();
        attachment_id
    }
    pub(super) async fn prompt_attachment_snapshot(
        &self,
    ) -> (
        Option<control_protocol::NativePromptHead>,
        Option<Arc<[u8]>>,
    ) {
        let channel = self.control.lock().await;
        (
            channel.prompt_head.clone(),
            channel.last_prompt_completion.clone(),
        )
    }

    pub(super) async fn install_control(
        &self,
        out: &mut Option<tokio::net::unix::OwnedWriteHalf>,
        session_id: &str,
        prompt_head: Option<control_protocol::NativePromptHead>,
    ) -> bool {
        let hello = ControlBody::Hello {
            control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
            session_id: session_id.to_string(),
            prompt_head,
        };
        write_control_frame(out.as_mut().expect("write half present"), &hello).await
    }

    pub(super) async fn next_outbound(&self, attachment_id: u64) -> Option<(u64, Arc<[u8]>)> {
        let mut channel = self.control.lock().await;
        if channel.active_attachment != Some(attachment_id) || channel.in_flight.is_some() {
            return None;
        }
        let (loss, dispatching) = {
            let native = self.native_channel.lock().expect("native channel lock");
            (native.loss, native.dispatching)
        };
        let selected = if let Some(cause) = loss {
            if !dispatching && channel.native_loss.is_none() {
                let cached_session = self.handshake.lock().await.session.is_some();
                let wire: Arc<[u8]> =
                    control_protocol::encode_frame(&ControlBody::NativeChannelLost {
                        cause,
                        cached_session,
                    })
                    .expect("bounded native loss frame serializes")
                    .into();
                assert!(wire.len() <= control_protocol::MAX_NATIVE_LOSS_FRAME_BYTES);
                let id = channel.next_entry_id;
                channel.next_entry_id = id.wrapping_add(1);
                channel.native_loss = Some(QueuedControl {
                    id,
                    scope: DeliveryScope::Persistent,
                    kind: QueuedKind::NativeLoss,
                    wire,
                    replay: None,
                });
            }
            // Management replies retain their routing priority. Buffered native
            // updates still drain before loss, without requiring SessionReady.
            channel
                .queue
                .iter()
                .find(|frame| frame.kind == QueuedKind::Handshake)
                .or_else(|| channel.queue.front())
                .or_else(|| {
                    (!dispatching && channel.native_loss_delivered != Some(attachment_id))
                        .then_some(channel.native_loss.as_ref())
                        .flatten()
                })
        } else {
            let announced = channel.session_announced;
            channel
                .queue
                .iter()
                .find(|frame| frame.kind == QueuedKind::Handshake)
                .or_else(|| {
                    channel.queue.iter().find(|frame| {
                        announced || !matches!(frame.scope, DeliveryScope::Persistent)
                    })
                })
        };
        let frame = selected?;
        let (id, wire) = (frame.id, Arc::clone(&frame.wire));
        channel.in_flight = Some(id);
        Some((id, wire))
    }

    pub(super) async fn commit_outbound(&self, attachment_id: u64, entry_id: u64) {
        let mut channel = self.control.lock().await;
        if channel.active_attachment == Some(attachment_id) && channel.in_flight == Some(entry_id) {
            #[cfg(test)]
            if let Some(frame) = channel
                .queue
                .iter()
                .find(|frame| frame.id == entry_id)
                .or_else(|| {
                    channel
                        .native_loss
                        .as_ref()
                        .filter(|frame| frame.id == entry_id)
                })
            {
                self.test_record_producer(|| serde_json::json!({
                    "stage": "committed",
                    "attachment": attachment_id,
                    "body": serde_json::from_slice::<serde_json::Value>(&frame.wire[4..]).unwrap(),
                }));
            }
            if channel
                .native_loss
                .as_ref()
                .is_some_and(|frame| frame.id == entry_id)
            {
                channel.native_loss_delivered = Some(attachment_id);
            }
            if let Some(index) = channel.queue.iter().position(|frame| frame.id == entry_id) {
                if matches!(channel.queue[index].kind, QueuedKind::PromptCompleted(id)
                    if channel.prompt_head.as_ref().is_some_and(|head| head.prompt_req_id == id))
                {
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
        self.write_agent_line_scoped(agent_stdin, value, None).await == NativeWriteOutcome::Written
    }

    fn refused_native_write(&self, written: usize) -> NativeWriteOutcome {
        if written == 0 {
            NativeWriteOutcome::Refused
        } else {
            self.observe_native_loss(NativeChannelLossCause::ProtocolEnded);
            NativeWriteOutcome::Lost
        }
    }

    async fn write_agent_line_scoped(
        &self,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        value: &serde_json::Value,
        authority: Option<&NativeWriteAuthority>,
    ) -> NativeWriteOutcome {
        use std::task::Poll;
        if self.native_loss().is_some() {
            return NativeWriteOutcome::Lost;
        }
        let mut bytes = match serde_json::to_vec(value) {
            Ok(bytes) => bytes,
            _ => return NativeWriteOutcome::Refused,
        };
        bytes.push(b'\n');
        #[cfg(test)]
        self.test_record_producer(|| {
            serde_json::json!({
                "stage": "write-lock",
                "id": value.get("id"),
                "method": value.get("method"),
            })
        });
        let mut stdin = tokio::select! {
            biased;
            _ = self.wait_native_loss() => return NativeWriteOutcome::Lost,
            stdin = agent_stdin.lock() => stdin,
        };
        let mut written = 0;
        let mut registry_deadline = None;
        loop {
            let guarded = std::future::poll_fn(|cx| {
                let callback = authority.and_then(NativeWriteAuthority::callback);
                let _callback_source = match authority
                    .and_then(NativeWriteAuthority::callback_decision)
                    .filter(|_| written < bytes.len())
                {
                    Some(context) => match context.try_decision_guard() {
                        Ok(guards) => Some(guards),
                        Err(error) if error.is::<worker_registry::RegistryBusy>() => {
                            return Poll::Ready(Err(NativeWriteWait::Callback));
                        }
                        Err(_) => return Poll::Ready(Ok(self.refused_native_write(written))),
                    },
                    None => None,
                };
                let _lifecycle =
                    match authority.filter(|_| written < bytes.len() && callback.is_none()) {
                        Some(authority) => match authority
                            .fence
                            .try_lock_observation_matching(|row| authority.matches(row))
                        {
                            Ok(Some(guard)) => Some(guard),
                            Ok(None) => return Poll::Ready(Err(NativeWriteWait::Native)),
                            Err(_) => return Poll::Ready(Ok(self.refused_native_write(written))),
                        },
                        None => None,
                    };
                let _registry = match authority
                    .filter(|_| written < bytes.len() && callback.is_none())
                {
                    Some(authority) => {
                        match worker_registry::try_registry_lock(&authority.attempt().session_id) {
                            Ok(Some(registry))
                                if authority.registry_matches(&registry).unwrap_or(false) =>
                            {
                                Some(registry)
                            }
                            Ok(None) => return Poll::Ready(Err(NativeWriteWait::Registry)),
                            _ => return Poll::Ready(Ok(self.refused_native_write(written))),
                        }
                    }
                    None => None,
                };
                let _callback_ticket = match callback.filter(|_| written < bytes.len()) {
                    Some((call_id, context)) => match self.pending_server_calls.try_lock() {
                        Ok(pending)
                            if pending.get(&call_id).is_some_and(|call| {
                                call.resolving
                                    && Some(call.attachment_id)
                                        == authority.and_then(|a| a.attachment)
                                    && call
                                        .authority
                                        .as_ref()
                                        .and_then(NativeWriteAuthority::callback)
                                        .is_some_and(|(_, captured)| Arc::ptr_eq(captured, context))
                            }) =>
                        {
                            Some(pending)
                        }
                        Ok(_) => return Poll::Ready(Ok(self.refused_native_write(written))),
                        Err(_) => return Poll::Ready(Err(NativeWriteWait::Ticket)),
                    },
                    None => None,
                };
                let mut native = self.native_channel.lock().expect("native channel lock");
                if native.loss.is_some() {
                    return Poll::Ready(Ok(NativeWriteOutcome::Lost));
                }
                if authority
                    .and_then(|authority| authority.attachment)
                    .is_some_and(|attachment| native.active_attachment != Some(attachment))
                    && written < bytes.len()
                {
                    drop(native);
                    return Poll::Ready(Ok(self.refused_native_write(written)));
                }
                while written < bytes.len() {
                    match std::pin::Pin::new(&mut *stdin).poll_write(cx, &bytes[written..]) {
                        Poll::Ready(Ok(0)) | Poll::Ready(Err(_)) => {
                            #[cfg(test)]
                            self.test_record_producer(|| {
                                serde_json::json!({
                                    "stage": "write-failed",
                                    "id": value.get("id"),
                                    "method": value.get("method"),
                                    "accepted_bytes": written,
                                })
                            });
                            drop(native);
                            self.observe_native_loss(NativeChannelLossCause::WriteFailed);
                            return Poll::Ready(Ok(NativeWriteOutcome::Lost));
                        }
                        Poll::Ready(Ok(count)) => {
                            written += count;
                            if let Some(id) = value
                                .get("method")
                                .and_then(|_| value.get("id"))
                                .and_then(serde_json::Value::as_i64)
                            {
                                if let Some(call) = native.calls.get_mut(&id) {
                                    call.write = if written == bytes.len() {
                                        NativeWriteProgress::Complete
                                    } else {
                                        NativeWriteProgress::Prefix(written)
                                    };
                                    if matches!(call.write, NativeWriteProgress::Complete) {
                                        self.control_space.notify_waiters();
                                    }
                                }
                            }
                            #[cfg(test)]
                            if written == bytes.len() {
                                self.test_record_producer(|| {
                                    serde_json::json!({
                                        "stage": "written",
                                        "id": value.get("id"),
                                        "method": value.get("method"),
                                    })
                                });
                            }
                        }
                        Poll::Pending => return Poll::Pending,
                    }
                }
                match std::pin::Pin::new(&mut *stdin).poll_flush(cx) {
                    Poll::Ready(Ok(())) => Poll::Ready(Ok(NativeWriteOutcome::Written)),
                    Poll::Ready(Err(_)) => {
                        drop(native);
                        self.observe_native_loss(NativeChannelLossCause::WriteFailed);
                        Poll::Ready(Ok(NativeWriteOutcome::Lost))
                    }
                    Poll::Pending => Poll::Pending,
                }
            });
            let result = tokio::select! {
                biased;
                _ = self.wait_native_loss() => return NativeWriteOutcome::Lost,
                result = guarded => result,
            };
            let wait = match result {
                Ok(outcome) => return outcome,
                Err(wait) => wait,
            };
            let authority = authority
                .expect("only a native permit reports contention")
                .clone();
            let waited = match wait {
                NativeWriteWait::Native => tokio::task::spawn_blocking(move || {
                    drop(
                        authority
                            .fence
                            .lock_observation_matching(|row| authority.matches(row))?,
                    );
                    anyhow::Ok(())
                })
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result),
                NativeWriteWait::Registry => {
                    let deadline = *registry_deadline.get_or_insert_with(|| {
                        tokio::time::Instant::now() + Duration::from_secs(3)
                    });
                    tokio::select! {
                        biased;
                        _ = self.wait_native_loss() => return NativeWriteOutcome::Lost,
                        result = worker_registry::registry_lock_until(&authority.attempt().session_id, deadline) => result.map(drop),
                    }
                }
                NativeWriteWait::Callback => {
                    #[cfg(test)]
                    let mut registry_wait = self.test_native_registry_wait.lock().unwrap().take();
                    tokio::task::spawn_blocking(move || {
                        let NativeWriteScope::Callback {
                            attempt, context, ..
                        } = authority.scope
                        else {
                            unreachable!("only callback source validation reports this wait");
                        };
                        loop {
                            match context.decision_guard(false) {
                                Ok(guards) => {
                                    drop(guards);
                                    return anyhow::Ok(());
                                }
                                Err(error) if error.is::<worker_registry::RegistryBusy>() => {
                                    // The failed guard has dropped Native exclusion. No
                                    // revalidation can reacquire it until this raw wait ends.
                                    #[cfg(test)]
                                    if let Some(waiting) = registry_wait.take() {
                                        let _ = waiting.send(());
                                    }
                                    drop(worker_registry::acquire_registry_lock(
                                        &attempt.session_id,
                                    )?);
                                }
                                Err(error) => return Err(error),
                            }
                        }
                    })
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|result| result)
                }
                NativeWriteWait::Ticket => {
                    drop(self.pending_server_calls.lock().await);
                    Ok(())
                }
            };
            if waited.is_err() {
                return self.refused_native_write(written);
            }
        }
    }

    async fn agent_request(
        &self,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        method: &str,
        params: serde_json::Value,
        dispatch_release: &mut Option<tokio::sync::oneshot::Sender<()>>,
        authority: Option<&NativeWriteAuthority>,
    ) -> Result<serde_json::Value, NativeRequestFailure> {
        let (id, rx) = {
            let mut native = self.native_channel.lock().expect("native channel lock");
            if native.loss.is_some() {
                return Err(NativeRequestFailure::OutcomeUnknown);
            }
            let id = self.next_req_id.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = tokio::sync::oneshot::channel();
            let scope = authority.map_or(
                crate::acp::establishment::NativeRequestScope::Observation,
                |authority| authority.request_scope.clone(),
            );
            let receipt = native
                .receipt
                .as_ref()
                .filter(|receipt| scope.origin(Some(receipt)).is_some())
                .cloned();
            native.calls.insert(
                id,
                NativePendingCall {
                    target: NativeReplyTarget::Handshake(tx),
                    scope,
                    receipt,
                    write: NativeWriteProgress::Pending,
                    window: None,
                },
            );
            (id, rx)
        };
        let _registration = NativePendingRegistration { shared: self, id };
        let written = self
            .write_agent_line_scoped(agent_stdin, &rpc_request(id, method, params), authority)
            .await;
        if written != NativeWriteOutcome::Written
            && self
                .native_channel
                .lock()
                .expect("native channel lock")
                .calls
                .remove(&id)
                .is_some()
        {
            return Err(match written {
                NativeWriteOutcome::Refused => NativeRequestFailure::NotIssued,
                _ => NativeRequestFailure::OutcomeUnknown,
            });
        }
        let response = tokio::select! {
            biased;
            response = rx => response.map_err(|_| NativeRequestFailure::OutcomeUnknown)?,
            _ = async {
                self.wait_native_loss().await;
                self.wait_native_dispatch().await;
            } => return Err(NativeRequestFailure::OutcomeUnknown),
        };
        let HandshakeResponse {
            value: response,
            release,
        } = response;
        *dispatch_release = Some(release);
        Ok(response)
    }

    async fn user_authority(
        &self,
        session_id: Option<&str>,
        scope: crate::acp::establishment::NativeRequestScope,
        attachment_id: u64,
    ) -> Result<NativeWriteAuthority, ()> {
        let (receipt, postimage) = {
            let handshake = self.handshake.lock().await;
            let cached = handshake.session.as_ref().ok_or(())?;
            let postimage = cached.postimage.as_ref().ok_or(())?;
            if session_id.is_none() || session_id != postimage.choice.acp_session_id.as_deref() {
                return Err(());
            }
            (Arc::clone(&cached.receipt), Arc::clone(postimage))
        };
        tokio::task::spawn_blocking(move || {
            match scope {
                crate::acp::establishment::NativeRequestScope::Operation { permission } => {
                    let fence = permission.session.effect_fence(&receipt).map_err(|_| ())?;
                    Ok(NativeWriteAuthority {
                        owner: permission.session.owner,
                        request_scope: crate::acp::establishment::NativeRequestScope::Operation {
                            permission: Arc::clone(&permission),
                        },
                        attachment: Some(attachment_id),
                        scope: NativeWriteScope::Operation {
                            receipt,
                            permission,
                        },
                        fence,
                    })
                }
                original @ crate::acp::establishment::NativeRequestScope::Original { .. } => {
                    {
                        let _effect = original.effect_guard(&receipt).map_err(|_| ())?;
                    }
                    let fence = receipt.attempt.applied_fence(&postimage).map_err(|_| ())?;
                    Ok(NativeWriteAuthority {
                        scope: NativeWriteScope::Applied {
                            attempt: Arc::clone(&receipt.attempt),
                            postimage,
                        },
                        fence,
                        owner: receipt.owner,
                        request_scope: original,
                        attachment: Some(attachment_id),
                    })
                }
                // A server callback ticket or an observation is not authority
                // for a new client-to-agent business request.
                _ => Err(()),
            }
        })
        .await
        .map_err(|_| ())?
    }

    async fn refuse_prompt(&self, id: i64) {
        if self
            .native_channel
            .lock()
            .expect("native channel lock")
            .calls
            .remove(&id)
            .is_none()
        {
            return;
        }
        self.enqueue(
            DeliveryScope::Persistent,
            QueuedKind::PromptCompleted(id),
            ControlBody::PromptCompleted {
                prompt_req_id: id,
                outcome: control_protocol::PromptOutcome::Error {
                    code: control_protocol::INVALID_PARAMS as i32,
                    message: "native conversation establishment is unresolved or changed".into(),
                    data: None,
                },
                evidence: None,
            },
        )
        .await;
    }

    pub(super) async fn agent_prompt(
        &self,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        attachment_id: u64,
        params: serde_json::Value,
        scope: crate::acp::establishment::NativeRequestScope,
    ) -> Option<i64> {
        #[cfg(test)]
        self.test_capture_original_prompt(attachment_id, &params, &scope);
        let id = {
            let native = self.native_channel.lock().expect("native channel lock");
            if native.loss.is_some() {
                return None;
            }
            self.next_req_id.fetch_add(1, Ordering::Relaxed)
        };
        let window_id = crate::acp::protocol::AcpPromptWindowId::new();
        let correlation = scope.correlation();
        if !self
            .enqueue(
                DeliveryScope::Attachment(attachment_id),
                QueuedKind::PromptStarted,
                ControlBody::PromptStarted {
                    prompt_req_id: id,
                    correlation,
                    window_id,
                },
            )
            .await
        {
            return None;
        }
        {
            let mut channel = self.control.lock().await;
            let mut native = self.native_channel.lock().expect("native channel lock");
            if native.loss.is_some() {
                return None;
            }
            channel.last_prompt_completion = None;
            channel.prompt_head = Some(control_protocol::NativePromptHead {
                prompt_req_id: id,
                scope: scope.clone(),
                window_id,
            });
            let receipt = native
                .receipt
                .as_ref()
                .filter(|receipt| scope.origin(Some(receipt)).is_some())
                .cloned();
            native.calls.insert(
                id,
                NativePendingCall {
                    target: NativeReplyTarget::Prompt,
                    scope: scope.clone(),
                    receipt,
                    write: NativeWriteProgress::Pending,
                    window: Some(NativePromptWindow {
                        id: window_id,
                        uncertain_output: false,
                    }),
                },
            );
        }
        let _registration = NativePendingRegistration { shared: self, id };
        self.control_space.notify_waiters();
        let authority = match self
            .user_authority(
                params.get("sessionId").and_then(serde_json::Value::as_str),
                scope,
                attachment_id,
            )
            .await
        {
            Ok(authority) => authority,
            Err(()) => {
                self.refuse_prompt(id).await;
                return Some(id);
            }
        };
        match self
            .write_agent_line_scoped(
                agent_stdin,
                &rpc_request(id, PROMPT_METHOD, params),
                Some(&authority),
            )
            .await
        {
            NativeWriteOutcome::Written => {}
            NativeWriteOutcome::Refused => self.refuse_prompt(id).await,
            NativeWriteOutcome::Lost => {
                self.native_channel
                    .lock()
                    .expect("native channel lock")
                    .calls
                    .remove(&id);
                return None;
            }
        }
        Some(id)
    }

    pub(super) async fn agent_cancel(
        &self,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        attachment_id: u64,
        acp_session_id: &str,
        scope: crate::acp::establishment::NativeRequestScope,
    ) {
        let Ok(authority) = self
            .user_authority(Some(acp_session_id), scope, attachment_id)
            .await
        else {
            return;
        };
        let note = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": acp_session_id },
        });
        if self
            .write_agent_line_scoped(agent_stdin, &note, Some(&authority))
            .await
            != NativeWriteOutcome::Written
        {
            warn!(target: "acp.runner", acp_session_id, "failed to deliver session/cancel to agent");
        }
    }

    pub(super) async fn run_or_replay_initialize(
        &self,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        request: serde_json::Value,
        dispatch_release: &mut Option<tokio::sync::oneshot::Sender<()>>,
    ) -> Result<serde_json::Value, HandshakeFailure> {
        let _gate = self.handshake_gate.lock().await;
        if let Some(cached) = self.handshake.lock().await.initialized.clone() {
            return Ok(cached);
        }
        if self.native_loss().is_some() {
            return Err(HandshakeFailure::new(
                HandshakeFailureOrigin::LocalTransport,
                transport_error("native channel unavailable"),
            ));
        }
        let response = self
            .agent_request(agent_stdin, "initialize", request, dispatch_release, None)
            .await
            .map_err(|_| {
                HandshakeFailure::new(
                    HandshakeFailureOrigin::LocalTransport,
                    transport_error("agent closed before answering initialize"),
                )
            })?;
        let result = handshake_result(response)?;
        self.handshake.lock().await.initialized = Some(result.clone());
        Ok(result)
    }

    /// Admission records issuance before native I/O. The original request is
    /// runner-owned, not attachment-owned, so a disconnected daemon cannot
    /// cancel its result collection or make the same key emit another RPC.
    pub(super) async fn start_or_replay_session(
        self: &Arc<Self>,
        agent_stdin: Arc<Mutex<tokio::process::ChildStdin>>,
        attachment_id: u64,
        method: String,
        request: serde_json::Value,
        attempt: Arc<EstablishmentAttempt>,
        store: Option<crate::session::ExecutionBinding>,
    ) {
        let _gate = self.handshake_gate.lock().await;
        let mut cached = self.handshake.lock().await.session.clone();
        if let Some(cached) = &cached {
            if cached.receipt.attempt.id == attempt.id {
                let identical = cached.receipt.attempt.as_ref() == attempt.as_ref()
                    && cached.receipt.same_store(store.as_ref())
                    && attempt.target.accepts_request(&method, &request);
                let frame = if identical {
                    cached.frame(self.native_availability(), false)
                } else {
                    HandshakeFailure::new(
                        HandshakeFailureOrigin::LocalState,
                        transport_error("establishment key reused for a different request"),
                    )
                    .into_frame(&method)
                };
                let established = matches!(frame, ControlBody::SessionReady { .. });
                self.enqueue_handshake(attachment_id, frame, established, false)
                    .await;
                return;
            }
            if cached.result.is_none() {
                self.enqueue_handshake(
                    attachment_id,
                    HandshakeFailure::new(
                        HandshakeFailureOrigin::LocalState,
                        transport_error("the original native establishment is still unresolved"),
                    )
                    .into_frame(&method),
                    false,
                    false,
                )
                .await;
                return;
            }
        }
        if !attempt.target.accepts_request(&method, &request) || self.native_loss().is_some() {
            self.enqueue_handshake(
                attachment_id,
                HandshakeFailure::new(
                    HandshakeFailureOrigin::LocalState,
                    transport_error("native establishment request has no matching authority"),
                )
                .into_frame(&method),
                false,
                false,
            )
            .await;
            return;
        }
        let mut store = store;
        let mut registry_deadline = None;
        let (owner, fence, scope) = loop {
            let shared = Arc::clone(self);
            let original = Arc::clone(&attempt);
            let prepared = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                // Native I/O and registry reads precede all small runner locks.
                // Keep both exclusions until the cache swap has linearized.
                let fence = original.effect_fence()?;
                let storage = crate::session::Storage::open_reference(&original.profile)?;
                let _native = fence.lock_observation_matching(|row| {
                    row.ensure_startable().is_ok()
                        && row.is_structured()
                        && row.terminal_handoff.is_none()
                        && original.matches_pending(row, &storage).unwrap_or(false)
                        && cached.as_ref().is_none_or(|old| {
                            old.receipt
                                .attempt
                                .matches_physical_source(row, &storage)
                                .unwrap_or(false)
                        })
                })?;
                let (id, pid) = shared
                    .registry_owner
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("runner ownership unavailable"))?;
                let Some(registry) = worker_registry::try_registry_lock(id)? else {
                    return Ok(EstablishmentAdmission::Wait {
                        reason: EstablishmentWait::Registry,
                        previous: cached,
                        store,
                    });
                };
                let record = registry.record()?.ok_or(crate::session::SessionGone)?;
                anyhow::ensure!(
                    registry.stop()?.is_none()
                        && record.pid == *pid
                        && *pid == std::process::id()
                        && record.session_id == original.session_id
                        && record.source_profile.as_deref() == Some(original.profile.as_str()),
                    "runner establishment owner changed"
                );
                let owner = crate::acp::runner_lifecycle::RunnerIdentity::from_record(&record);
                let birth = crate::process::process_birth_identity(*pid)?
                    .map(|identity| identity.fingerprint());
                anyhow::ensure!(
                    owner.birth_fingerprint.is_some() && owner.birth_fingerprint == birth,
                    "runner establishment birth is unconfirmed"
                );
                let mut handshake = match shared.handshake.try_lock() {
                    Ok(guard) => guard,
                    Err(_) => {
                        return Ok(EstablishmentAdmission::Wait {
                            reason: EstablishmentWait::Handshake,
                            previous: cached,
                            store,
                        })
                    }
                };
                let mut channel = match shared.control.try_lock() {
                    Ok(guard) => guard,
                    Err(_) => {
                        return Ok(EstablishmentAdmission::Wait {
                            reason: EstablishmentWait::Channel,
                            previous: cached,
                            store,
                        })
                    }
                };
                let callbacks = match shared.pending_server_calls.try_lock() {
                    Ok(guard) => guard,
                    Err(_) => {
                        return Ok(EstablishmentAdmission::Wait {
                            reason: EstablishmentWait::Callbacks,
                            previous: cached,
                            store,
                        })
                    }
                };
                let mut native = match shared.native_channel.try_lock() {
                    Ok(guard) => guard,
                    Err(std::sync::TryLockError::WouldBlock) => {
                        return Ok(EstablishmentAdmission::Wait {
                            reason: EstablishmentWait::Native,
                            previous: cached,
                            store,
                        });
                    }
                    Err(std::sync::TryLockError::Poisoned(_)) => {
                        anyhow::bail!("native channel custody is unconfirmed")
                    }
                };
                anyhow::ensure!(native.loss.is_none(), "native channel ended");
                match (&handshake.session, &cached) {
                    (None, None) => {}
                    (Some(current), Some(previous)) => {
                        anyhow::ensure!(
                            native.calls.values().all(|call| {
                                call.write == NativeWriteProgress::Complete
                                    && call.window.is_some()
                                    && call.receipt.as_ref().is_some_and(|receipt| {
                                        receipt.replay.closed
                                            && !receipt.replay.lost
                                            && receipt.session_id().is_some()
                                            && call.scope.origin(Some(receipt)).is_some()
                                    })
                            }) && callbacks.is_empty()
                                && !native.dispatching,
                            "native outcomes are still owned"
                        );
                        anyhow::ensure!(
                            Arc::ptr_eq(&current.receipt, &previous.receipt)
                                && current.accepts_live_fresh(&original, store.as_ref(), owner)
                                && channel.queue.is_empty()
                                && channel.in_flight.is_none()
                                && channel.replay.as_ref().is_some_and(|replay| {
                                    replay.attempt_id == current.receipt.attempt.id
                                        && !replay.open
                                        && !replay.lost
                                }),
                            "the exact Applied predecessor is not fully consumed"
                        );
                    }
                    _ => anyhow::bail!("native establishment cache changed"),
                }
                let scope = crate::acp::establishment::NativeRequestScope::Original {
                    attempt: Arc::clone(&original),
                    owner,
                    store: store.clone().map(Arc::new),
                    operation: None,
                };
                handshake.session = Some(NativeSessionCache {
                    receipt: Arc::new(NativeEstablishmentReceipt {
                        attempt: Arc::clone(&original),
                        owner,
                        store,
                        outcome: NativeEstablishmentOutcome::Failed {
                            reason: EstablishmentBlockReason::OutcomeUnknown,
                        },
                        replay: crate::acp::establishment::NativeReplayClosure {
                            notifications: 0,
                            closed: false,
                            lost: false,
                        },
                    }),
                    result: None,
                    postimage: None,
                    failure_origin: HandshakeFailureOrigin::LocalState,
                    unknown: Arc::new(transport_error(
                        "original native establishment outcome is unknown",
                    )),
                    local_failure: None,
                    collector_finished: false,
                });
                native.receipt = handshake
                    .session
                    .as_ref()
                    .map(|cached| Arc::clone(&cached.receipt));
                native.history_fence = Some(Arc::clone(&fence));
                native.postimage = None;
                let original_receipt = native.receipt.as_ref().expect("installed original receipt");
                native.callback_original = Some(Arc::new(
                    crate::acp::establishment::NativeCallbackOriginal {
                        attempt: Arc::clone(&original_receipt.attempt),
                        owner: original_receipt.owner,
                        store: original_receipt.store.clone(),
                        postimage: std::sync::OnceLock::new(),
                        notification_scope: std::sync::Mutex::new(None),
                    },
                ));
                channel.last_prompt_completion = None;
                channel.prompt_head = None;
                channel.session_announced = false;
                channel.callbacks_announced = channel.active_attachment == Some(attachment_id);
                channel.replay = Some(NativeReplayAccount {
                    attempt_id: original.id,
                    notifications: 0,
                    open: true,
                    lost: false,
                });
                Ok(EstablishmentAdmission::Installed {
                    owner,
                    fence,
                    scope,
                })
            })
            .await;
            match prepared {
                Ok(Ok(EstablishmentAdmission::Installed {
                    owner,
                    fence,
                    scope,
                })) => break (owner, fence, scope),
                Ok(Ok(EstablishmentAdmission::Wait {
                    reason,
                    previous,
                    store: retained_store,
                })) => {
                    cached = previous;
                    store = retained_store;
                    // No native, registry, or runner-state guard survives this await.
                    let waiting = async {
                        match reason {
                            EstablishmentWait::Handshake => drop(self.handshake.lock().await),
                            EstablishmentWait::Channel => drop(self.control.lock().await),
                            EstablishmentWait::Callbacks => {
                                drop(self.pending_server_calls.lock().await)
                            }
                            EstablishmentWait::Registry => {
                                let deadline = *registry_deadline.get_or_insert_with(|| {
                                    tokio::time::Instant::now() + Duration::from_secs(3)
                                });
                                drop(
                                    worker_registry::registry_lock_until(
                                        &attempt.session_id,
                                        deadline,
                                    )
                                    .await?,
                                );
                            }
                            EstablishmentWait::Native => {
                                let shared = Arc::clone(self);
                                tokio::task::spawn_blocking(move || {
                                    drop(
                                        shared.native_channel.lock().expect("native channel lock"),
                                    );
                                })
                                .await?;
                            }
                        }
                        Ok::<_, anyhow::Error>(())
                    };
                    let waited = tokio::select! {
                        biased;
                        _ = self.wait_native_loss() => false,
                        result = waiting => result.is_ok(),
                    };
                    if waited {
                        continue;
                    }
                }
                Ok(Err(error)) => {
                    warn!(target: "acp.runner", %error, "native establishment admission refused");
                }
                Err(error) => {
                    warn!(target: "acp.runner", %error, "native establishment admission task failed");
                }
            }
            self.enqueue_handshake(
                attachment_id,
                HandshakeFailure::new(
                    HandshakeFailureOrigin::LocalState,
                    transport_error("native establishment admission is unconfirmed"),
                )
                .into_frame(&method),
                false,
                false,
            )
            .await;
            return;
        };
        self.control_space.notify_waiters();
        let shared = Arc::clone(self);
        tokio::spawn(async move {
            let mut dispatch_release = None;
            let authority = NativeWriteAuthority {
                scope: NativeWriteScope::Pending(Arc::clone(&attempt)),
                fence,
                owner,
                request_scope: scope,
                attachment: Some(attachment_id),
            };
            let response = shared
                .agent_request(
                    &agent_stdin,
                    &method,
                    request,
                    &mut dispatch_release,
                    Some(&authority),
                )
                .await;
            let closure = {
                let mut channel = shared.control.lock().await;
                let replay = channel
                    .replay
                    .as_mut()
                    .expect("original native replay is retained");
                assert_eq!(replay.attempt_id, attempt.id);
                replay.open = false;
                crate::acp::establishment::NativeReplayClosure {
                    notifications: replay.notifications,
                    closed: response.is_ok(),
                    lost: replay.lost,
                }
            };
            let (outcome, result, failure_origin) = match response {
                Ok(response) => match handshake_result(response) {
                    Ok(result) => match attempt.target.resulting_session_id(&result) {
                        Ok(id) => (
                            NativeEstablishmentOutcome::Succeeded {
                                session_id: id.to_owned(),
                            },
                            Ok(Arc::new(result)),
                            HandshakeFailureOrigin::NativeResponse,
                        ),
                        Err(_) => (
                            NativeEstablishmentOutcome::Failed {
                                reason: EstablishmentBlockReason::OutcomeUnknown,
                            },
                            Err(Arc::new(transport_error(
                                "native result did not identify the selected conversation",
                            ))),
                            HandshakeFailureOrigin::LocalResult,
                        ),
                    },
                    Err(failure) => {
                        let reason = if failure.origin == HandshakeFailureOrigin::NativeResponse {
                            if failure
                                .error
                                .get("code")
                                .and_then(serde_json::Value::as_i64)
                                == Some(-32601)
                            {
                                EstablishmentBlockReason::Unsupported
                            } else {
                                EstablishmentBlockReason::Rejected
                            }
                        } else {
                            EstablishmentBlockReason::OutcomeUnknown
                        };
                        (
                            NativeEstablishmentOutcome::Failed { reason },
                            Err(Arc::new(failure.error)),
                            failure.origin,
                        )
                    }
                },
                Err(NativeRequestFailure::NotIssued) => (
                    NativeEstablishmentOutcome::Failed {
                        reason: EstablishmentBlockReason::NotIssued,
                    },
                    Err(Arc::new(transport_error(
                        "native establishment was refused before emission",
                    ))),
                    HandshakeFailureOrigin::LocalState,
                ),
                Err(NativeRequestFailure::OutcomeUnknown) => (
                    NativeEstablishmentOutcome::Failed {
                        reason: EstablishmentBlockReason::OutcomeUnknown,
                    },
                    Err(Arc::new(transport_error(
                        "native establishment ended without a result",
                    ))),
                    HandshakeFailureOrigin::LocalTransport,
                ),
            };
            let (frame, established) = {
                let mut handshake = shared.handshake.lock().await;
                let cached = handshake
                    .session
                    .as_mut()
                    .expect("issued native attempt is retained");
                assert_eq!(cached.receipt.attempt.id, attempt.id);
                let mut receipt = cached.receipt.as_ref().clone();
                receipt.outcome = outcome;
                receipt.replay = closure;
                cached.receipt = Arc::new(receipt);
                cached.result = Some(result);
                cached.failure_origin = failure_origin;
                cached.postimage = cached.receipt.postimage(attempt.context).ok().map(Arc::new);
                {
                    let mut native = shared.native_channel.lock().expect("native channel lock");
                    native.receipt = Some(Arc::clone(&cached.receipt));
                    native.postimage = cached.postimage.clone();
                    if let (Some(original), Some(postimage)) =
                        (&native.callback_original, &cached.postimage)
                    {
                        assert!(original.attempt.same_source(&cached.receipt.attempt));
                        let _ = original.postimage.set(Arc::clone(postimage));
                    }
                }
                if let Some(sid) = cached.receipt.session_id() {
                    if let Err(error) = shared.persist_acp_session_id(sid) {
                        cached.local_failure = Some(Arc::new(HandshakeFailure::new(
                            HandshakeFailureOrigin::LocalState,
                            serde_json::to_value(error).expect("JSON-RPC error serializes"),
                        )));
                    }
                }
                let frame = cached.frame(shared.native_availability(), true);
                let established = matches!(frame, ControlBody::SessionReady { .. });
                (frame, established)
            };
            shared.control_space.notify_waiters();
            shared
                .enqueue_handshake(
                    attachment_id,
                    frame,
                    established,
                    dispatch_release.is_some(),
                )
                .await;
            drop(dispatch_release);
            {
                let mut handshake = shared.handshake.lock().await;
                let cached = handshake
                    .session
                    .as_mut()
                    .expect("original cache is retained");
                assert_eq!(cached.receipt.attempt.id, attempt.id);
                cached.collector_finished = true;
            }
            shared.control_space.notify_waiters();
        });
    }

    /// Read-only adoption waits for the original runner-owned collection, never
    /// issuing initialize/load/fork/new. Attachment closure cancels this waiter.
    pub(super) async fn resume_session(
        &self,
        attachment_id: u64,
    ) -> Option<(NativeSessionCache, bool)> {
        let original = loop {
            let changed = self.control_space.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let original = {
                let handshake = self.handshake.lock().await;
                let cache = handshake.session.as_ref()?;
                let native = self.native_channel.lock().expect("native channel lock");
                if cache.result.is_some() || native.loss.is_some() {
                    return Some((cache.clone(), false));
                }
                if cache.collector_finished {
                    return None;
                }
                let mut originals = native.calls.values().filter(|call| {
                    let NativeReplyTarget::Handshake(_) = &call.target else {
                        return false;
                    };
                    let crate::acp::establishment::NativeRequestScope::Original {
                        attempt,
                        owner,
                        store,
                        operation: None,
                    } = &call.scope
                    else {
                        return false;
                    };
                    attempt.as_ref() == cache.receipt.attempt.as_ref()
                        && *owner == cache.receipt.owner
                        && cache.receipt.same_store(store.as_deref())
                });
                let issued = match (originals.next(), originals.next()) {
                    (Some(call), None) => {
                        let NativeReplyTarget::Handshake(sender) = &call.target else {
                            unreachable!()
                        };
                        matches!(call.write, NativeWriteProgress::Complete)
                            && !sender.is_closed()
                            && call
                                .receipt
                                .as_ref()
                                .is_some_and(|receipt| Arc::ptr_eq(receipt, &cache.receipt))
                            && native
                                .receipt
                                .as_ref()
                                .is_some_and(|receipt| Arc::ptr_eq(receipt, &cache.receipt))
                    }
                    _ => false,
                };
                issued.then(|| Arc::clone(&cache.receipt))
            };
            if let Some(original) = original {
                break original;
            }
            tokio::select! {
                _ = changed => {}
                _ = self.wait_native_loss() => {}
            }
        };
        if !self
            .enqueue_native(
                DeliveryScope::Attachment(attachment_id),
                QueuedKind::Handshake,
                ControlBody::OriginalEstablishment {
                    invocation: Arc::clone(&original),
                },
                NativePublication::CachedReceipt,
            )
            .await
        {
            return None;
        }
        {
            let mut channel = self.control.lock().await;
            if channel.active_attachment != Some(attachment_id) {
                return None;
            }
            // Handshake priority sends the descriptor before any newly released call.
            channel.callbacks_announced = true;
        }
        self.control_space.notify_waiters();
        loop {
            let changed = self.control_space.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let handshake = self.handshake.lock().await;
                match &handshake.session {
                    Some(cache) if cache.receipt.attempt.as_ref() == original.attempt.as_ref() => {
                        if cache.result.is_some() || self.native_loss().is_some() {
                            return Some((cache.clone(), true));
                        }
                    }
                    _ => return None,
                }
            }
            tokio::select! {
                _ = changed => {}
                _ = self.wait_native_loss() => {}
            }
        }
    }

    pub(super) async fn acp_session_id(&self) -> Option<String> {
        self.handshake
            .lock()
            .await
            .session
            .as_ref()
            .and_then(|cached| cached.receipt.session_id().map(str::to_owned))
    }
    pub(super) async fn issue_agent_call(
        &self,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        attachment_id: u64,
        call_id: u64,
        method: &str,
        params: serde_json::Value,
        scope: crate::acp::establishment::NativeRequestScope,
    ) {
        if matches!(method, "session/new" | "session/load" | "session/fork") {
            self.enqueue(
                DeliveryScope::Attachment(attachment_id),
                QueuedKind::AgentReply,
                ControlBody::AgentError {
                    call_id,
                    error: control_protocol::JsonRpcError::new(
                        control_protocol::INVALID_PARAMS,
                        "native establishment requires correlated authority",
                    ),
                },
            )
            .await;
            return;
        }
        let authority = if method == "session/list" {
            None
        } else {
            match self
                .user_authority(
                    params.get("sessionId").and_then(serde_json::Value::as_str),
                    scope.clone(),
                    attachment_id,
                )
                .await
            {
                Ok(authority) => Some(authority),
                Err(()) => {
                    self.enqueue(
                        DeliveryScope::Attachment(attachment_id),
                        QueuedKind::AgentReply,
                        ControlBody::AgentError {
                            call_id,
                            error: control_protocol::JsonRpcError::new(
                                control_protocol::INVALID_PARAMS,
                                "native conversation establishment is unresolved or changed",
                            ),
                        },
                    )
                    .await;
                    return;
                }
            }
        };
        let req_id = {
            let mut native = self.native_channel.lock().expect("native channel lock");
            if native.loss.is_some() {
                return;
            }
            let req_id = self.next_req_id.fetch_add(1, Ordering::Relaxed);
            let receipt = native
                .receipt
                .as_ref()
                .filter(|receipt| scope.origin(Some(receipt)).is_some())
                .cloned();
            native.calls.insert(
                req_id,
                NativePendingCall {
                    target: NativeReplyTarget::Agent {
                        call_id,
                        attachment_id,
                    },
                    scope,
                    receipt,
                    write: NativeWriteProgress::Pending,
                    window: (method == PROMPT_METHOD).then(|| NativePromptWindow {
                        id: crate::acp::protocol::AcpPromptWindowId::new(),
                        uncertain_output: false,
                    }),
                },
            );
            req_id
        };
        let _registration = NativePendingRegistration {
            shared: self,
            id: req_id,
        };
        if self
            .write_agent_line_scoped(
                agent_stdin,
                &rpc_request(req_id, method, params),
                authority.as_ref(),
            )
            .await
            != NativeWriteOutcome::Written
        {
            if self
                .native_channel
                .lock()
                .expect("native channel lock")
                .calls
                .remove(&req_id)
                .is_none()
            {
                return; // The agent already answered while the write was finishing.
            }
            if self.native_loss().is_some() {
                return;
            }
            self.enqueue(
                DeliveryScope::Attachment(attachment_id),
                QueuedKind::AgentReply,
                ControlBody::AgentError {
                    call_id,
                    error: control_protocol::JsonRpcError::new(
                        control_protocol::INVALID_PARAMS,
                        "native conversation establishment is unresolved or changed",
                    ),
                },
            )
            .await;
        }
    }
}

pub(super) fn rpc_request(id: i64, method: &str, params: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

pub(super) fn handshake_result(
    mut response: serde_json::Value,
) -> Result<serde_json::Value, HandshakeFailure> {
    let invalid = || {
        HandshakeFailure::new(
            HandshakeFailureOrigin::LocalResult,
            transport_error("invalid native handshake response"),
        )
    };
    if response.get("jsonrpc").and_then(serde_json::Value::as_str) != Some("2.0") {
        return Err(invalid());
    }
    let object = response.as_object_mut().ok_or_else(invalid)?;
    let result = object.remove("result");
    if let Some(error) = object.remove("error") {
        let valid = result.is_none()
            && error.as_object().is_some_and(|error| {
                error
                    .get("code")
                    .and_then(serde_json::Value::as_i64)
                    .is_some_and(|code| i32::try_from(code).is_ok())
                    && error
                        .get("message")
                        .is_some_and(serde_json::Value::is_string)
            });
        return Err(if valid {
            HandshakeFailure::new(HandshakeFailureOrigin::NativeResponse, error)
        } else {
            invalid()
        });
    }
    result.ok_or_else(invalid)
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

    #[tokio::test]
    #[serial_test::serial]
    async fn original_io_framing_progresses_while_lifecycle_admission_is_blocked() {
        use super::super::test_support::OwnedNativeFixture;

        let mut fixture = OwnedNativeFixture::load("original-io-framing");
        fixture.begin().await;
        assert_eq!(fixture.read_request().await["method"], "session/load");
        let lifecycle = fixture
            .storage
            .acquire_instance_lifecycle_lock(&fixture.attempt.session_id)
            .unwrap();
        let shared = Arc::clone(&fixture.shared);
        let (framed, observed) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            let frames = [
                "fs/read_text_file",
                "fs/write_text_file",
                "terminal/create",
                "terminal/output",
                "terminal/release",
                "terminal/wait_for_exit",
                "terminal/kill",
            ]
            .map(|method| {
                let line = serde_json::to_vec(&serde_json::json!({
                    "jsonrpc": "2.0", "id": method, "method": method,
                    "params": {"sessionId": "original-io-framing"},
                }))
                .unwrap();
                shared.capture_native_frame(&line)
            });
            let _ = framed.send(());
            frames
        });
        let progress = observed.recv_timeout(Duration::from_secs(10));
        drop(lifecycle);
        let frames = reader.join().unwrap();
        progress.expect("Original IO must not block subsequent Native stdout frames");
        for frame in frames {
            assert!(frame.evidence.physical_source.is_some());
            assert!(frame.evidence.session_stamp.is_none());
            assert!(frame.evidence.original_request_stamp.is_none());
            assert!(frame.callback_authority.is_none());
        }
    }

    #[test]
    #[serial_test::serial]
    fn native_callback_framing_waits_for_registry_without_changing_original_stamp() {
        use super::super::test_support::OwnedNativeFixture;
        use crate::acp::establishment::OriginalRequestStamp;
        use std::sync::mpsc;

        for multithread in [false, true] {
            let mut builder = if multithread {
                let mut builder = tokio::runtime::Builder::new_multi_thread();
                builder.worker_threads(2);
                builder
            } else {
                tokio::runtime::Builder::new_current_thread()
            };
            builder.enable_all().build().unwrap().block_on(async {
                for method in ["session/request_permission", "elicitation/create"] {
                    let mut fixture = OwnedNativeFixture::load("original-registry-wait");
                    fixture.begin().await;
                    let request = fixture.read_request().await;
                    assert_eq!(request["method"], "session/load");
                    let receipt = fixture
                        .shared
                        .native_channel
                        .lock()
                        .unwrap()
                        .receipt
                        .clone()
                        .unwrap();
                    let row = fixture
                        .storage
                        .load_instances_complete()
                        .unwrap()
                        .into_iter()
                        .find(|row| row.id == fixture.attempt.session_id)
                        .unwrap();
                    let expected = OriginalRequestStamp::capture(
                        &receipt,
                        &row,
                        &fixture.storage,
                        Some("original-registry-wait"),
                    )
                    .unwrap();
                    let (waiting, waited) = mpsc::channel();
                    *fixture.shared.test_native_registry_wait.lock().unwrap() = Some(waiting);
                    let (locked, acquired) = mpsc::channel();
                    let shared = Arc::clone(&fixture.shared);
                    let id = fixture.attempt.session_id.clone();
                    let profile = fixture.attempt.profile.clone();
                    let holder = std::thread::spawn(move || {
                        let registry = worker_registry::acquire_registry_lock(&id).unwrap();
                        locked.send(()).unwrap();
                        waited.recv_timeout(Duration::from_secs(10)).unwrap();
                        let storage = crate::session::Storage::open_reference(&profile).unwrap();
                        assert!(storage
                            .try_acquire_instance_lifecycle_lock(&id)
                            .unwrap()
                            .is_none());
                        assert!(shared.native_channel.try_lock().is_ok());
                        assert!(shared.pending_server_calls.try_lock().is_ok());
                        drop(registry);
                    });
                    acquired.recv_timeout(Duration::from_secs(10)).unwrap();
                    let line = serde_json::to_vec(&serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": 19,
                        "method": method,
                        "params": {"sessionId": "original-registry-wait"},
                    }))
                    .unwrap();
                    let captured = fixture.shared.capture_native_frame(&line);
                    holder.join().unwrap();
                    assert_eq!(captured.evidence.original_request_stamp, Some(expected));
                    assert_eq!(captured.evidence.session_stamp, None);
                    assert_eq!(captured.evidence.resource_start_stamp, None);
                    assert_eq!(
                        captured.evidence.physical_source,
                        crate::acp::state::NativePhysicalSource::from_receipt(&receipt),
                    );
                    assert_eq!(captured.request.unwrap().1, method);
                }
            });
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn original_decision_wire_rechecks_source_after_stdin_wait() {
        use super::super::test_support::OwnedNativeFixture;
        use std::sync::mpsc;

        for (method, case) in ["session/request_permission", "elicitation/create"]
            .into_iter()
            .flat_map(|method| {
                [
                    "live",
                    "registry-busy",
                    "owner-revoked",
                    "owner-revoked-refusal",
                    "foreign-refusal",
                ]
                .map(|case| (method, case))
            })
        {
            let mut fixture = OwnedNativeFixture::load("original-wire-source");
            fixture.begin().await;
            fixture.read_request().await;
            let sid = if case == "foreign-refusal" {
                "foreign-native"
            } else {
                "original-wire-source"
            };
            fixture
                .deliver(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": 27,
                    "method": method,
                    "params": {"sessionId": sid},
                }))
                .await;
            let (call_id, attachment_id) = {
                let pending = fixture.shared.pending_server_calls.lock().await;
                let (&id, call) = pending.iter().next().expect("actual callback ticket");
                assert_eq!(call.authority.is_some(), case != "foreign-refusal");
                (id, call.attachment_id)
            };
            let stdin = fixture.stdin.lock().await;
            let shared = Arc::clone(&fixture.shared);
            let input = Arc::clone(&fixture.stdin);
            let id = fixture.attempt.session_id.clone();
            let decision = if method == "session/request_permission" {
                serde_json::json!({
                    "outcome": {"outcome": "selected", "optionId": "allow"}
                })
            } else {
                serde_json::json!({"action": "accept", "content": {}})
            };
            let refusal = matches!(case, "owner-revoked-refusal" | "foreign-refusal");
            let refusal_message = if case == "foreign-refusal" {
                "foreign callback source"
            } else {
                "native callback scope is no longer active"
            };
            let outcome = if refusal {
                Err(control_protocol::JsonRpcError::new(-32602, refusal_message))
            } else {
                Ok(decision.clone())
            };
            let response = tokio::spawn(async move {
                shared
                    .resolve_server_call(&input, attachment_id, call_id, outcome, &id)
                    .await;
            });
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if fixture
                        .shared
                        .pending_server_calls
                        .lock()
                        .await
                        .get(&call_id)
                        .is_some_and(|call| call.resolving)
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(
                !response.is_finished(),
                "stdin gate retains actual response ticket"
            );
            let holder = if case == "registry-busy" {
                let (waiting, waited) = mpsc::channel();
                *fixture.shared.test_native_registry_wait.lock().unwrap() = Some(waiting);
                let (locked, acquired) = mpsc::channel();
                let id = fixture.attempt.session_id.clone();
                let profile = fixture.attempt.profile.clone();
                let shared = Arc::clone(&fixture.shared);
                let holder = std::thread::spawn(move || {
                    let registry = worker_registry::acquire_registry_lock(&id).unwrap();
                    locked.send(()).unwrap();
                    // The callback worker has dropped its failed decision guard and
                    // cannot revalidate while this thread retains the registry lock.
                    waited.recv_timeout(Duration::from_secs(10)).unwrap();
                    let storage = crate::session::Storage::open_reference(&profile).unwrap();
                    assert!(storage
                        .try_acquire_instance_lifecycle_lock(&id)
                        .unwrap()
                        .is_some());
                    let pending = shared.pending_server_calls.try_lock().unwrap();
                    assert!(pending.get(&call_id).is_some_and(|call| call.resolving));
                    drop(pending);
                    assert!(shared.native_channel.try_lock().is_ok());
                    drop(registry);
                });
                acquired.recv_timeout(Duration::from_secs(10)).unwrap();
                Some(holder)
            } else {
                None
            };
            if matches!(case, "owner-revoked" | "owner-revoked-refusal") {
                let mut replacement = fixture.record.clone();
                replacement.generation += 1;
                worker_registry::save(&replacement).unwrap();
            }
            drop(stdin);
            tokio::time::timeout(Duration::from_secs(10), response)
                .await
                .unwrap()
                .unwrap();
            if let Some(holder) = holder {
                holder.join().unwrap();
            }
            assert!(fixture.shared.pending_server_calls.lock().await.is_empty());
            let fence = serde_json::json!({"jsonrpc": "2.0", "method": "__wire_fence"});
            assert!(
                fixture
                    .shared
                    .write_agent_line(&fixture.stdin, &fence)
                    .await
            );
            let sent = fixture.read_request().await;
            if case == "owner-revoked" {
                assert_eq!(sent, fence, "revoked source cannot write a decision");
            } else {
                assert_eq!(sent["id"], 27);
                if refusal {
                    assert_eq!(sent["error"]["code"], -32602);
                    assert_eq!(sent["error"]["message"], refusal_message);
                    assert!(
                        sent.get("result").is_none(),
                        "protocol refusal never grants allow or accept"
                    );
                } else {
                    assert_eq!(sent["result"], decision);
                }
                assert_eq!(fixture.read_request().await, fence);
            }
        }
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

    struct RealRunnerOrdering {
        client: crate::acp::supervisor::test_support::managed::ManagedClient,
        native: crate::acp::supervisor::test_support::managed::NativePeer,
        prompts: tokio::sync::mpsc::Receiver<serde_json::Value>,
        inspection: super::managed_runner::RunnerInspection,
        session_id: String,
    }

    async fn ordering_deadline<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(15), future)
            .await
            .expect("actual runner ordering observation completes")
    }

    fn producer_frames(snapshot: &serde_json::Value, stage: &str) -> Vec<ControlBody> {
        snapshot["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|event| event["stage"] == stage)
            .filter_map(|event| event.get("body"))
            .map(|body| serde_json::from_value(body.clone()).unwrap())
            .collect()
    }

    fn producer_has(snapshot: &serde_json::Value, stage: &str, id: i64) -> bool {
        snapshot["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|event| event["stage"] == stage && event["id"] == id)
    }

    async fn producer_until(
        inspection: &super::managed_runner::RunnerInspection,
        predicate: impl Fn(&serde_json::Value) -> bool,
    ) -> serde_json::Value {
        ordering_deadline(async {
            loop {
                let snapshot = inspection.query("producer-snapshot").await;
                if predicate(&snapshot) {
                    return snapshot;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
    }

    impl RealRunnerOrdering {
        async fn new(
            scope: &mut crate::acp::supervisor::test_support::managed::ManagedScope,
            test_name: &str,
            replay_prefix: bool,
            close_stdin: bool,
        ) -> Self {
            let inspection = scope.inspect_next_runner(test_name);
            let directory = scope.tempdir();
            let session_id = format!("ordering-native-{}", uuid::Uuid::new_v4());
            let native_session = session_id.clone();
            let (session_seen, mut sessions) = tokio::sync::mpsc::channel(1);
            let (prompt_seen, prompts) = tokio::sync::mpsc::channel(2);
            let respond = move |request: &serde_json::Value| {
                let id = request.get("id")?;
                match request["method"].as_str().unwrap() {
                    "initialize" => Some(serde_json::json!({
                        "jsonrpc":"2.0", "id":id,
                        "result":{"protocolVersion":1,"agentCapabilities":{}}
                    })),
                    "session/new" => {
                        session_seen.try_send(request.clone()).unwrap();
                        None
                    }
                    "session/prompt" => {
                        assert_eq!(request["params"]["sessionId"], native_session);
                        prompt_seen.try_send(request.clone()).unwrap();
                        None
                    }
                    method => panic!("unexpected original native request: {method}"),
                }
            };
            let (config, native) = if close_stdin {
                scope.native_peer_with_stdin_close(
                    directory.path().into(),
                    "codex",
                    "codex",
                    respond,
                )
            } else {
                scope.native_peer(directory.path().into(), "codex", "codex", respond)
            };
            // Drive the real spawn while withholding only its genuine native response.
            // No fake establishment, terminal.begin, receipt, live head or Applied row.
            let client =
                {
                    // Bound debug fixture stack use across the nested launch and join.
                    let spawn = Box::pin(scope.spawn(config, "runner-ordering"));
                    let establish =
                        async {
                            let original_session = ordering_deadline(sessions.recv())
                                .await
                                .expect("actual original session/new");
                            assert_eq!(original_session["method"], "session/new");
                            inspection.query("observe-producer").await;
                            if replay_prefix {
                                native.send(serde_json::json!({
                            "jsonrpc":"2.0", "method":"session/update", "params":{
                                "sessionId": session_id,
                                "update":{"sessionUpdate":"agent_message_chunk",
                                    "content":{"type":"text","text":"original replay prefix"}}
                            }
                        })).await;
                                native.send(serde_json::json!({
                            "jsonrpc":"2.0", "method":"_aoe/session_replayed", "params":{}
                        })).await;
                            }
                            native
                                .send(serde_json::json!({
                                    "jsonrpc":"2.0", "id": original_session["id"],
                                    "result":{"sessionId":session_id}
                                }))
                                .await;
                        };
                    let (client, ()) =
                        ordering_deadline(async { tokio::join!(spawn, establish) }).await;
                    client.unwrap()
                };
            ordering_deadline(client.ready()).await;
            let receipt = client
                .actual()
                .establishment_projection()
                .unwrap()
                .receipt_snapshot()
                .expect("actual original receipt after managed readiness");
            assert_eq!(receipt.session_id(), Some(session_id.as_str()));
            assert_eq!(receipt.owner, client.lease.spawned_runner().unwrap());
            let row = crate::session::Storage::open_unwatched("test")
                .unwrap()
                .load_instances_complete()
                .unwrap()
                .into_iter()
                .find(|row| row.id == client.id)
                .unwrap();
            assert_eq!(
                row.acp_establishment,
                Some(crate::acp::establishment::AcpEstablishment::Applied {
                    attempt_id: receipt.attempt.id,
                })
            );
            Self {
                client,
                native,
                prompts,
                inspection,
                session_id,
            }
        }

        async fn next_original_prompt(&mut self) -> serde_json::Value {
            ordering_deadline(self.prompts.recv())
                .await
                .expect("actual original Native request")
        }

        async fn start_prompt(&mut self) -> serde_json::Value {
            self.client
                .send_prompt("actual original for runner ordering", &[])
                .await
                .unwrap();
            self.next_original_prompt().await
        }

        async fn complete(&self, request: &serde_json::Value) {
            self.native
                .send(serde_json::json!({
                    "jsonrpc":"2.0", "id": request["id"],
                    "result":{"stopReason":"end_turn"}
                }))
                .await;
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn post_new_command_catalog_keeps_original_scope_until_application() {
        use crate::acp::acp_client::SdkTestOptions;
        use crate::acp::event_sender::EventScope;
        use crate::acp::state::{AcpSessionId, AcpState, AgentName, Event};
        use crate::acp::supervisor::test_support::managed::with_managed;

        const NAME: &str = "process::runner::shared::tests::post_new_command_catalog_keeps_original_scope_until_application";
        if super::managed_runner::child_if_requested(NAME).await {
            return;
        }
        with_managed(|scope| Box::pin(async move {
            let inspection = scope.inspect_next_runner(NAME);
            let (gate, release, gate_seen) = scope.gate();
            scope.configure_sdk(SdkTestOptions {
                fresh_response_application: Some(gate),
                ..Default::default()
            });
            let directory = scope.tempdir();
            let (seen, mut requests) = tokio::sync::mpsc::channel(1);
            let (config, native) = scope.native_peer(
                directory.path().into(), "codex", "codex", move |request| {
                    let id = request.get("id")?;
                    match request["method"].as_str().unwrap() {
                        "initialize" => Some(serde_json::json!({
                            "jsonrpc":"2.0", "id":id,
                            "result":{"protocolVersion":1,"agentCapabilities":{}}
                        })),
                        "session/new" => {
                            seen.try_send(request.clone()).unwrap();
                            None
                        }
                        method => panic!("unexpected Native request: {method}"),
                    }
                },
            );
            let catalog = |sid: &str, name: &str| serde_json::json!({
                "jsonrpc":"2.0", "method":"session/update", "params":{
                    "sessionId":sid, "update":{
                        "sessionUpdate":"available_commands_update",
                        "availableCommands":[{"name":name,"description":name}]
                    }
                }
            });
            let spawn = Box::pin(scope.spawn(config, "post-new-catalog"));
            let observe = async {
                let request = ordering_deadline(requests.recv()).await.unwrap();
                inspection.query("observe-producer").await;
                native.send(catalog("catalog-native", "before-result")).await;
                native.send(serde_json::json!({
                    "jsonrpc":"2.0", "id":request["id"],
                    "result":{"sessionId":"catalog-native"}
                })).await;
                ordering_deadline(gate_seen).await.unwrap();
                native.send(catalog("foreign-native", "foreign")).await;
                native.send(catalog("catalog-native", "help")).await;
                let captured = producer_until(&inspection, |snapshot| {
                    producer_frames(snapshot, "committed").iter().any(|frame| {
                        matches!(frame, ControlBody::Notify { params, .. }
                            if params.pointer("/update/availableCommands/0/name")
                                == Some(&serde_json::json!("help")))
                    })
                }).await;
                let mut catalogs = Vec::new();
                for frame in producer_frames(&captured, "committed") {
                    if let ControlBody::Notify { params, evidence, .. } = frame {
                        if let Some(name) = params.pointer("/update/availableCommands/0/name") {
                            catalogs.push(name.as_str().unwrap().to_owned());
                            assert!(evidence.session_stamp.is_none());
                            assert_eq!(evidence.original_request_stamp.is_some(), name == "help");
                            assert!(evidence.resource_start_stamp.is_none());
                        }
                    }
                }
                assert_eq!(catalogs, ["before-result", "foreign", "help"]);
                assert!(release.release());
            };
            let (client, ()) = ordering_deadline(async { tokio::join!(spawn, observe) }).await;
            let mut client = client.unwrap();
            ordering_deadline(client.ready()).await;
            loop {
                if matches!(ordering_deadline(client.recv()).await.expect("Native metadata event"),
                    Event::AvailableCommandsUpdated { commands }
                        if commands.iter().any(|command| command.name == "help"))
                {
                    break;
                }
            }
            let stored = scope.state.acp_event_store.replay_from(&client.id, 0).unwrap();
            let mut projected = AcpState::new(
                AcpSessionId(client.id.clone()), AgentName("codex".into()), None,
            );
            for event in stored {
                if let Event::AvailableCommandsUpdated { commands } = event.event.as_ref() {
                    let expected = if commands[0].name == "help" {
                        EventScope::SessionCurrent
                    } else {
                        EventScope::HistoryOnly
                    };
                    assert_eq!(event.metadata.scope, expected);
                }
                projected.apply_scoped_event(event.seq, &event.event, &event.metadata).unwrap();
            }
            assert_eq!(projected.available_commands.len(), 1);
            assert_eq!(projected.available_commands[0].name, "help");
            assert_eq!(native.requests().iter().filter(|method| method.as_str() == "session/new").count(), 1);
            client.shutdown().await.unwrap();
        })).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn prompt_write_failure_does_not_own_a_pending_original() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        const NAME: &str =
            "process::runner::shared::tests::prompt_write_failure_does_not_own_a_pending_original";
        if super::managed_runner::child_if_requested(NAME).await {
            return;
        }
        with_managed(|scope| Box::pin(async move {
                let fixture = Box::pin(RealRunnerOrdering::new(scope, NAME, false, true)).await;
                fixture.native.send(serde_json::json!({"__managed_close_stdin":true})).await;
                let closed = producer_until(&fixture.inspection, |snapshot| {
                    producer_frames(snapshot, "committed").iter().any(|frame|
                        matches!(frame, ControlBody::Notify { method, .. } if method == "__managed/stdin_closed"))
                }).await;
                assert!(closed["loss"].is_null(), "stdout and native process stay live after closing only fd 0");
                assert!(closed["pending"].as_array().unwrap().is_empty());
                fixture.client.send_prompt("real write must hit closed native stdin", &[]).await.unwrap();
                let failed = producer_until(&fixture.inspection, |snapshot| {
                    let events = snapshot["events"].as_array().unwrap();
                    events.iter().any(|event| event["stage"] == "prompt-returned" && event["id"].is_null())
                        && events.iter().any(|event| event["stage"] == "committed"
                            && event["body"]["kind"] == "native_channel_lost")
                }).await;
                let frames = producer_frames(&failed, "queued");
                let starts: Vec<_> = frames.iter().filter_map(|frame| match frame {
                    ControlBody::PromptStarted { prompt_req_id, .. } => Some(*prompt_req_id),
                    _ => None,
                }).collect();
                let [id] = starts.as_slice() else { panic!("one actual attempted original: {starts:?}"); };
                assert!(producer_has(&failed, "write-failed", *id));
                assert!(!producer_has(&failed, "written", *id));
                let write_failure = failed["events"].as_array().unwrap().iter()
                    .find(|event| event["stage"] == "write-failed" && event["id"] == *id).unwrap();
                assert_eq!(write_failure["accepted_bytes"], 0);
                assert!(failed["pending"].as_array().unwrap().is_empty());
                assert_eq!(failed["loss"], serde_json::to_value(NativeChannelLossCause::WriteFailed).unwrap());
                for stage in ["queued", "committed"] {
                    assert!(!producer_frames(&failed, stage).iter().any(|frame|
                        matches!(frame, ControlBody::PromptCompleted { .. } | ControlBody::AgentError { .. })));
                }
                let losses: Vec<_> = producer_frames(&failed, "committed").into_iter().filter(|frame|
                    matches!(frame, ControlBody::NativeChannelLost { .. })).collect();
                assert!(matches!(losses.as_slice(), [ControlBody::NativeChannelLost {
                    cause: NativeChannelLossCause::WriteFailed, cached_session: true,
                }]));
                assert_eq!(fixture.native.requests().iter().filter(|method| method.as_str() == PROMPT_METHOD).count(), 0);
                assert_eq!(fixture.native.requests().iter().filter(|method| method.as_str() == "session/cancel").count(), 0);
                let proof = fixture.inspection.native_spawn_proof(&fixture.client.lease).unwrap().unwrap();
                assert!(crate::process::process_birth_identity(proof.pid).unwrap()
                    .is_some_and(|birth| birth == proof.birth));
                fixture.client.shutdown().await.unwrap();
            })).await;
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

    #[tokio::test]
    #[serial_test::serial]
    async fn fork_failure_origin_requires_the_correlated_native_response() {
        use super::super::test_support::OwnedNativeFixture;
        for (code, expected_origin, expected_reason) in [
            (
                serde_json::json!(-32603),
                HandshakeFailureOrigin::NativeResponse,
                EstablishmentBlockReason::Rejected,
            ),
            (
                serde_json::json!(-32601),
                HandshakeFailureOrigin::NativeResponse,
                EstablishmentBlockReason::Unsupported,
            ),
            (
                serde_json::json!("invalid"),
                HandshakeFailureOrigin::LocalResult,
                EstablishmentBlockReason::OutcomeUnknown,
            ),
        ] {
            let mut fixture = OwnedNativeFixture::fork("original-parent");
            let original_bytes = std::fs::read(fixture.storage.sessions_path()).unwrap();
            fixture.begin().await;
            let sent = fixture.read_request().await;
            assert_eq!(sent["method"], "session/fork");
            assert_eq!(sent["params"]["sessionId"], "original-parent");
            let id = sent["id"].as_i64().unwrap();
            fixture
                .deliver(&serde_json::json!({
                    "jsonrpc":"2.0","id":id + 100,
                    "error":{"code":-32603,"message":"unrelated native rejection"}
                }))
                .await;
            assert!(fixture
                .shared
                .native_channel
                .lock()
                .unwrap()
                .calls
                .contains_key(&id));
            {
                let handshake = fixture.shared.handshake.lock().await;
                let cached = handshake
                    .session
                    .as_ref()
                    .expect("original attempt is retained");
                assert_eq!(cached.receipt.attempt.as_ref(), fixture.attempt.as_ref());
                assert!(
                    cached.result.is_none(),
                    "an unrelated response cannot finish the fork"
                );
            }
            let response = serde_json::json!({
                "jsonrpc":"2.0","id":id,
                "error":{"code":code,"message":"native fork rejected",
                    "data":{"origin":"native_response","method":"session/fork"}}
            });
            fixture.deliver(&response).await;
            let ControlBody::EstablishmentFailed {
                receipt,
                error,
                origin,
                original_delivery,
                delivery,
            } = fixture
                .wait_for_frame(|frame| matches!(frame, ControlBody::EstablishmentFailed { .. }))
                .await
            else {
                unreachable!()
            };
            assert_eq!(origin, expected_origin);
            assert_eq!(receipt.attempt.as_ref(), fixture.attempt.as_ref());
            assert_eq!(
                receipt.owner,
                crate::acp::runner_lifecycle::RunnerIdentity::from_record(&fixture.record),
            );
            assert!(receipt.owner.birth_fingerprint.is_some());
            assert!(receipt.same_store(Some(&fixture.store)));
            assert_eq!(
                receipt.outcome,
                NativeEstablishmentOutcome::Failed {
                    reason: expected_reason,
                }
            );
            assert!(original_delivery);
            assert!(receipt.replay.closed);
            assert!(!receipt.replay.lost);
            assert_eq!(receipt.replay.notifications, 0);
            assert!(delivery.matches(receipt.replay));
            if expected_origin == HandshakeFailureOrigin::NativeResponse {
                assert_eq!(error.as_ref(), &response["error"]);
            } else {
                assert_eq!(error["message"], "invalid native handshake response");
            }
            fixture.wait_for_frame(|frame| matches!(frame,
                ControlBody::Notify { method, .. } if SessionReplayed::matches_method(method)
            )).await;
            {
                let handshake = fixture.shared.handshake.lock().await;
                let cached = handshake
                    .session
                    .as_ref()
                    .expect("failure must retain the original cache");
                assert_eq!(cached.receipt.as_ref(), receipt.as_ref());
                assert!(matches!(&cached.result, Some(Err(cached_error))
                    if cached_error.as_ref() == error.as_ref()));
            }
            assert!(fixture
                .shared
                .native_channel
                .lock()
                .unwrap()
                .calls
                .is_empty());
            assert!(fixture.shared.acp_session_id().await.is_none());
            assert_eq!(
                std::fs::read(fixture.storage.sessions_path()).unwrap(),
                original_bytes
            );
            fixture.replay_same_request().await;
            fixture.assert_no_more_requests();
            assert!(fixture
                .shared
                .native_channel
                .lock()
                .unwrap()
                .calls
                .is_empty());
            let current = worker_registry::load_strict(&fixture.record.session_id)
                .unwrap()
                .unwrap();
            assert_eq!(
                crate::acp::runner_lifecycle::RunnerIdentity::from_record(&current),
                receipt.owner,
            );
        }

        let mut fixture = OwnedNativeFixture::fork("original-parent");
        let original_bytes = std::fs::read(fixture.storage.sessions_path()).unwrap();
        fixture.kill_native();
        fixture.begin().await;
        let ControlBody::EstablishmentFailed {
            receipt, origin, ..
        } = fixture
            .wait_for_frame(|frame| matches!(frame, ControlBody::EstablishmentFailed { .. }))
            .await
        else {
            unreachable!()
        };
        assert_eq!(origin, HandshakeFailureOrigin::LocalTransport);
        assert_eq!(
            receipt.outcome,
            NativeEstablishmentOutcome::Failed {
                reason: EstablishmentBlockReason::OutcomeUnknown,
            }
        );
        assert_eq!(receipt.attempt.as_ref(), fixture.attempt.as_ref());
        assert!(receipt.same_store(Some(&fixture.store)));
        assert!(!receipt.replay.closed);
        assert!(fixture
            .shared
            .native_channel
            .lock()
            .unwrap()
            .calls
            .is_empty());
        assert!(fixture.shared.acp_session_id().await.is_none());
        {
            let handshake = fixture.shared.handshake.lock().await;
            let cached = handshake
                .session
                .as_ref()
                .expect("transport failure retains the attempt");
            assert!(matches!(&cached.result, Some(Err(_))));
        }
        fixture.replay_same_request().await;
        fixture.assert_no_more_requests();
        assert_eq!(
            std::fs::read(fixture.storage.sessions_path()).unwrap(),
            original_bytes
        );
    }

    #[test]
    fn handshake_failure_origin_requires_a_genuine_error_response() {
        let rejected = handshake_result(serde_json::json!({
            "jsonrpc":"2.0","id":1,
            "error":{"code":-32603,"message":"native rejection","data":{"origin":"local_transport"}}
        }))
        .unwrap_err();
        assert_eq!(rejected.origin, HandshakeFailureOrigin::NativeResponse);
        assert_eq!(rejected.error["message"], "native rejection");
        for invalid in [
            serde_json::json!({"jsonrpc":"2.0","id":1,"result":{},"error":{"code":-32603,"message":"ambiguous"}}),
            serde_json::json!({"jsonrpc":"2.0","id":1,"error":{"code":"-32603","message":"invalid","data":{"origin":"native_response"}}}),
            serde_json::json!({"id":1,"error":{"code":-32603,"message":"invalid protocol"}}),
            serde_json::json!({"jsonrpc":"2.0","id":1}),
        ] {
            assert_eq!(
                handshake_result(invalid).unwrap_err().origin,
                HandshakeFailureOrigin::LocalResult
            );
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn prompt_completion_follows_usage_update_and_rejects_agent_marker() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        const NAME: &str = "process::runner::shared::tests::prompt_completion_follows_usage_update_and_rejects_agent_marker";
        if super::managed_runner::child_if_requested(NAME).await {
            return;
        }
        with_managed(|scope| Box::pin(async move {
                let mut fixture = Box::pin(RealRunnerOrdering::new(scope, NAME, false, false)).await;
                let request = fixture.start_prompt().await;
                let id = request["id"].as_i64().unwrap();
                let written = producer_until(&fixture.inspection,
                    |snapshot| producer_has(snapshot, "written", id)).await;
                assert!(written["pending"].as_array().unwrap().iter()
                    .any(|call| call["id"] == id && call["progress"] == "complete"));
                fixture.native.send(serde_json::json!({
                    "jsonrpc":"2.0", "method":"_aoe/prompt_completed", "params":{}
                })).await;
                let usage = serde_json::json!({
                    "sessionId":fixture.session_id,
                    "update":{
                        "sessionUpdate":"usage_update", "used":100, "size":200,
                        "_meta":{"_claude/rateLimit":{
                            "status":"rejected", "rateLimitType":"five_hour",
                            "resetsAt":4_102_444_800_i64
                        }}
                    }
                });
                fixture.native.send(serde_json::json!({
                    "jsonrpc":"2.0", "method":"session/update", "params":usage
                })).await;
                fixture.complete(&request).await;
                let snapshot = producer_until(&fixture.inspection, |snapshot| {
                    producer_frames(snapshot, "committed").iter().any(|frame|
                        matches!(frame, ControlBody::PromptCompleted { prompt_req_id, .. } if *prompt_req_id == id))
                }).await;
                for stage in ["queued", "committed"] {
                    let relevant: Vec<_> = producer_frames(&snapshot, stage).into_iter().filter(|frame| {
                        matches!(frame, ControlBody::PromptStarted { .. } | ControlBody::PromptCompleted { .. })
                            || matches!(frame, ControlBody::Notify { params, .. } if params == &usage)
                            || matches!(frame, ControlBody::Notify { method, .. } if PromptCompletedMarker::matches_method(method))
                    }).collect();
                    let [ControlBody::PromptStarted { prompt_req_id, correlation, window_id },
                        ControlBody::Notify { method, params, replay, .. },
                        ControlBody::PromptCompleted { prompt_req_id: completed, outcome, evidence: Some(evidence) }]
                        = relevant.as_slice() else { panic!("producer {stage} order: {relevant:?}"); };
                    assert_eq!((*prompt_req_id, *completed), (id, id));
                    assert_eq!(method, "session/update");
                    assert_eq!(params, &usage);
                    assert_eq!(*replay, None);
                    assert_eq!(outcome, &PromptOutcome::Completed { stop_reason: Some("end_turn".into()) });
                    assert_eq!(evidence.prompt_req_id, id);
                    assert_eq!(&evidence.window_id, window_id);
                    assert_eq!(evidence.scope.correlation(), *correlation);
                    assert_eq!(evidence.scope.origin(Some(&evidence.receipt)), Some(evidence.origin));
                    let admitted = snapshot["events"].as_array().unwrap().iter()
                        .find(|event| event["stage"] == "admitted").unwrap();
                    assert_eq!(serde_json::to_value(&evidence.scope).unwrap(), admitted["scope"]);
                    let receipt = fixture.client.actual().establishment_projection().unwrap()
                        .receipt_snapshot().unwrap();
                    assert_eq!(evidence.receipt.as_ref(), receipt.as_ref());
                }
                assert!(snapshot["pending"].as_array().unwrap().iter().all(|call| call["id"] != id));
                assert!(snapshot["loss"].is_null());
                fixture.client.shutdown().await.unwrap();
            })).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn prompt_start_precedes_agent_write_and_is_not_replayed() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        const NAME: &str =
            "process::runner::shared::tests::prompt_start_precedes_agent_write_and_is_not_replayed";
        if super::managed_runner::child_if_requested(NAME).await {
            return;
        }
        with_managed(|scope| Box::pin(async move {
                let mut fixture = Box::pin(RealRunnerOrdering::new(scope, NAME, false, false)).await;
                fixture.inspection.query("hold-native-stdin").await;
                fixture.client.send_prompt("block the actual producer write", &[]).await.unwrap();
                let blocked = producer_until(&fixture.inspection, |snapshot| {
                    snapshot["events"].as_array().unwrap().iter().any(|event|
                        event["stage"] == "write-lock" && event["method"] == PROMPT_METHOD)
                }).await;
                let starts: Vec<_> = producer_frames(&blocked, "queued").into_iter()
                    .filter(|frame| matches!(frame, ControlBody::PromptStarted { .. })).collect();
                let [ControlBody::PromptStarted { prompt_req_id: id, .. }] = starts.as_slice()
                    else { panic!("original start must queue while stdin is held: {starts:?}"); };
                let id = *id;
                let admitted = blocked["events"].as_array().unwrap().iter()
                    .find(|event| event["stage"] == "admitted").unwrap();
                let old_attachment = admitted["attachment"].as_u64().unwrap();
                assert_eq!(admitted["params"]["sessionId"], fixture.session_id);
                assert!(!producer_has(&blocked, "written", id));
                assert!(blocked["pending"].as_array().unwrap().iter()
                    .any(|call| call["id"] == id && call["progress"] == "pending"));
                assert!(matches!(fixture.prompts.try_recv(), Err(tokio::sync::mpsc::error::TryRecvError::Empty)));
                fixture.inspection.query("release-native-stdin").await;
                let request = fixture.next_original_prompt().await;
                assert_eq!(request["id"], id);
                assert_eq!(request["method"], PROMPT_METHOD);
                producer_until(&fixture.inspection,
                    |snapshot| producer_has(snapshot, "prompt-returned", id)).await;

                fixture.client = scope.adopt(fixture.client, true).await;
                ordering_deadline(fixture.client.ready()).await;
                let adopted = fixture.inspection.query("producer-snapshot").await;
                let replacement = adopted["attachment"].as_u64().unwrap();
                assert_ne!(old_attachment, replacement);
                assert_eq!(fixture.inspection.query("retry-original-from-detached-attachment").await["result"], serde_json::Value::Null);
                let rejected = fixture.inspection.query("producer-snapshot").await;
                assert_eq!(producer_frames(&adopted, "queued").iter()
                        .filter(|frame| matches!(frame, ControlBody::PromptStarted { .. })).count(),
                    producer_frames(&rejected, "queued").iter()
                        .filter(|frame| matches!(frame, ControlBody::PromptStarted { .. })).count());
                assert_eq!(adopted["events"].as_array().unwrap().iter()
                        .filter(|event| event["stage"] == "written" && event["method"] == PROMPT_METHOD).count(),
                    rejected["events"].as_array().unwrap().iter()
                        .filter(|event| event["stage"] == "written" && event["method"] == PROMPT_METHOD).count(),
                    "detached original cannot write even before the Native callback runs");
                assert_eq!(fixture.native.requests().iter().filter(|method| method.as_str() == PROMPT_METHOD).count(), 1);
                assert_eq!(fixture.native.requests().iter().filter(|method| method.as_str() == "session/cancel").count(), 0);
                fixture.complete(&request).await;
                let completed = producer_until(&fixture.inspection, |snapshot| {
                    snapshot["events"].as_array().unwrap().iter().any(|event|
                        event["stage"] == "committed" && event["attachment"] == replacement
                            && event["body"]["kind"] == "prompt_completed"
                            && event["body"]["prompt_req_id"] == id)
                }).await;
                let replacement_prompt_frames: Vec<_> = completed["events"].as_array().unwrap().iter()
                    .filter(|event| event["stage"] == "committed" && event["attachment"] == replacement)
                    .map(|event| serde_json::from_value::<ControlBody>(event["body"].clone()).unwrap())
                    .filter(|frame| matches!(frame, ControlBody::PromptStarted { .. } | ControlBody::PromptCompleted { .. }))
                    .collect();
                let [ControlBody::PromptCompleted { prompt_req_id, outcome, evidence: Some(evidence) }]
                    = replacement_prompt_frames.as_slice()
                    else { panic!("only actual durable original completion reaches replacement: {replacement_prompt_frames:?}"); };
                assert_eq!(*prompt_req_id, id);
                assert_eq!(outcome, &PromptOutcome::Completed { stop_reason: Some("end_turn".into()) });
                assert_eq!(serde_json::to_value(&evidence.scope).unwrap(), admitted["scope"]);
                assert_eq!(fixture.native.requests().iter().filter(|method| method.as_str() == PROMPT_METHOD).count(), 1);
                assert!(completed["pending"].as_array().unwrap().iter().all(|call| call["id"] != id));
                assert!(completed["loss"].is_null());
                fixture.client.shutdown().await.unwrap();
            })).await;
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
            ..
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
            QueuedKind::PromptCompleted(7),
            wire(16),
        );
        let budget = control_protocol::MAX_CONTROL_QUEUE_BYTES
            - control_protocol::MAX_NATIVE_LOSS_FRAME_BYTES;

        assert!(channel.make_room(QueuedKind::AgentReply, budget - 32));
        assert_eq!(channel.queued_bytes, 32);
        assert_eq!(channel.queue.len(), 2);
        assert!(channel
            .queue
            .iter()
            .all(|frame| frame.kind != QueuedKind::Notify));
        assert!(
            !channel.make_room(QueuedKind::AgentReply, budget - 31),
            "correlation frames must apply backpressure rather than be evicted"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn retained_large_completion_and_later_backlog_fit_the_actual_consumer() {
        use crate::acp::acp_client::SdkTestOptions;
        use crate::acp::supervisor::test_support::managed::with_managed;
        use crate::acp::Event;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        const TEST_NAME: &str = "process::runner::shared::tests::retained_large_completion_and_later_backlog_fit_the_actual_consumer";
        if super::managed_runner::child_if_requested(TEST_NAME).await {
            return;
        }
        with_managed(|scope| Box::pin(async move {
            let inspection = scope.inspect_next_runner(TEST_NAME);
            let (completion_gate, _release, completion_received) = scope.gate();
            scope.configure_sdk(SdkTestOptions { prompt_completed: Some(completion_gate), ..Default::default() });
            let directory = scope.tempdir();
            let (daemon_write, agent_read) = tokio::io::duplex(128 * 1024);
            let (mut agent_write, daemon_read) = tokio::io::duplex(128 * 1024);
            let config = scope.bridge_config_as(directory.path().into(), daemon_write, daemon_read, "codex", "codex");
            scope.spawn_helper(async move {
                let mut lines = BufReader::new(agent_read).lines();
                while let Some(line) = lines.next_line().await.unwrap() {
                    let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                    let Some(id) = request.get("id") else { continue; };
                    let response = match request["method"].as_str().unwrap() {
                        "initialize" => serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":1,"agentCapabilities":{}}}),
                        "session/new" => serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":"same-native"}}),
                        "session/prompt" => {
                            assert_eq!(request["params"]["sessionId"], "same-native");
                            serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                                "code":-32603,"message":"cached request failed",
                                "data":{"privateTokenMarker":"x".repeat(64 * 1024)}
                            }})
                        }
                        method => panic!("no implicit native operation during retained replay: {method}"),
                    };
                    agent_write.write_all(&serde_json::to_vec(&response).unwrap()).await.unwrap();
                    agent_write.write_all(b"\n").await.unwrap();
                }
            });
            let original = scope.spawn(config, "retained-bytes").await.unwrap();
            original.ready().await;
            let owner = original.lease.spawned_runner().expect("actual original child owner");
            assert_ne!(owner.pid, std::process::id(), "SystemProcessControl must never own this test process");
            original.send_prompt("retain the real native failure", &[]).await.unwrap();
            tokio::time::timeout(Duration::from_secs(10), completion_received).await.unwrap().unwrap();
            let mut budget = None;
            let mut client = scope.adopt_with(original, true, || async {
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let snapshot = inspection.query("snapshot").await;
                        if snapshot["attached"] == false && snapshot["queued_frames"] == 0 { break; }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                }).await.expect("the actual old control writer disconnects after committing its completion");
                budget = Some(inspection.query("fill-retained-backlog").await);
            }).await;
            let budget = budget.unwrap();
            assert_eq!(budget["queued_frames"], 127);
            let retained = budget["retained_bytes"].as_u64().unwrap();
            assert!(retained > 16 * 1024);
            assert!(budget["queued_bytes"].as_u64().unwrap() + retained
                + budget["loss_reserve"].as_u64().unwrap() <= budget["byte_limit"].as_u64().unwrap());
            let expected_updates = budget["queued_frames"].as_u64().unwrap() as usize;
            tokio::time::timeout(Duration::from_secs(120), async {
                let mut updates = 0;
                let mut diagnostics = 0;
                let mut stopped = 0;
                while updates < expected_updates || diagnostics == 0 || stopped == 0 {
                    let event = client.next_event().await.expect("retained runner remains attached");
                    assert!(!serde_json::to_string(&event).unwrap().contains("privateTokenMarker"));
                    match event {
                        Event::ToolCallStarted { .. } => updates += 1,
                        Event::PromptRuntimeError { .. } => diagnostics += 1,
                        Event::Stopped { reason } => {
                            assert_eq!(reason, "prompt_failed");
                            stopped += 1;
                        }
                        Event::AgentStartupError { message, .. } => panic!("{message}"),
                        Event::AcpConnectionUnavailable { .. } => panic!("bounded replay lost the native channel"),
                        _ => {}
                    }
                }
                assert_eq!((diagnostics, stopped), (1, 1));
                client.shutdown().await.unwrap();
            }).await.expect("retained replay consumer completes");
        })).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn established_session_barrier_follows_its_replay() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        const NAME: &str =
            "process::runner::shared::tests::established_session_barrier_follows_its_replay";
        if super::managed_runner::child_if_requested(NAME).await {
            return;
        }
        with_managed(|scope| Box::pin(async move {
                let fixture = Box::pin(RealRunnerOrdering::new(scope, NAME, true, false)).await;
                let snapshot = producer_until(&fixture.inspection, |snapshot| {
                    producer_frames(snapshot, "committed").iter().any(|frame|
                        matches!(frame, ControlBody::Notify { method, .. } if SessionReplayed::matches_method(method)))
                }).await;
                let relevant: Vec<_> = producer_frames(&snapshot, "committed").into_iter()
                    .filter(|frame| matches!(frame, ControlBody::SessionReady { .. })
                        || matches!(frame, ControlBody::Notify { method, .. }
                            if method == "session/update" || SessionReplayed::matches_method(method)))
                    .collect();
                let [ControlBody::SessionReady { acp_session_id, receipt, delivery, postimage: Some(postimage), .. },
                    ControlBody::Notify { method, params, replay: Some(prefix), .. },
                    ControlBody::Notify { method: marker_method, params: marker_params, replay: None, .. }]
                    = relevant.as_slice()
                    else { panic!("actual producer flush order must be ready, original replay, genuine barrier: {relevant:?}"); };
                assert_eq!(acp_session_id, &fixture.session_id);
                assert_eq!(method, "session/update");
                assert_eq!(params, &serde_json::json!({
                    "sessionId":fixture.session_id,
                    "update":{"sessionUpdate":"agent_message_chunk",
                        "content":{"type":"text","text":"original replay prefix"}}
                }));
                assert_eq!(prefix.attempt_id, receipt.attempt.id);
                assert_eq!(prefix.ordinal, 1);
                assert_eq!(receipt.replay.notifications, 1);
                assert!(receipt.replay.closed && !receipt.replay.lost);
                assert!(delivery.matches(receipt.replay));
                assert!(SessionReplayed::matches_method(marker_method));
                let marker: SessionReplayed = serde_json::from_value(marker_params.clone()).unwrap();
                assert_eq!(marker.attempt_id, receipt.attempt.id);
                assert_eq!(marker.closure, receipt.replay);
                assert_eq!(marker.delivery, *delivery);
                assert_eq!(postimage.attempt_id, receipt.attempt.id);
                assert_eq!(receipt.as_ref(), fixture.client.actual().establishment_projection().unwrap()
                    .receipt_snapshot().unwrap().as_ref());
                assert!(snapshot["loss"].is_null());
                fixture.client.shutdown().await.unwrap();
            })).await;
    }

    #[tokio::test]
    async fn outbound_frame_remains_queued_until_write_commit() {
        let shared = RunnerShared::new(None);
        shared
            .enqueue(
                DeliveryScope::Persistent,
                QueuedKind::PromptCompleted(7),
                ControlBody::PromptCompleted {
                    prompt_req_id: 7,
                    outcome: PromptOutcome::Aborted,
                    evidence: None,
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
    async fn disconnect_fences_late_replies_without_consuming_the_new_observation() {
        let (shared, stdin, mut child) = shared_with_stdin().await;
        let old_attachment = shared.begin_attachment().await;
        assert!(
            shared
                .enqueue(
                    DeliveryScope::Attachment(old_attachment),
                    QueuedKind::Handshake,
                    ControlBody::Initialized {
                        result: serde_json::json!({})
                    },
                )
                .await
        );
        shared
            .issue_agent_call(
                &stdin,
                old_attachment,
                9,
                "session/list",
                serde_json::json!({}),
                crate::acp::establishment::NativeRequestScope::Observation,
            )
            .await;
        let first = read_agent_stdin(&stdin, &mut child).await;
        let first: serde_json::Value = serde_json::from_str(first.trim()).unwrap();
        let first_id = first["id"].as_i64().unwrap();
        shared.disconnect_control(old_attachment, &stdin, "s").await;
        let current_attachment = shared.begin_attachment().await;
        assert!(
            !shared
                .enqueue(
                    DeliveryScope::Attachment(old_attachment),
                    QueuedKind::Handshake,
                    ControlBody::Initialized {
                        result: serde_json::json!({"stale": true})
                    },
                )
                .await
        );
        shared
            .issue_agent_call(
                &stdin,
                current_attachment,
                5,
                "session/list",
                serde_json::json!({}),
                crate::acp::establishment::NativeRequestScope::Observation,
            )
            .await;
        let second = read_agent_stdin(&stdin, &mut child).await;
        let second: serde_json::Value = serde_json::from_str(second.trim()).unwrap();
        let second_id = second["id"].as_i64().unwrap();
        assert_ne!(first_id, second_id);
        let old = serde_json::json!({"jsonrpc":"2.0","id":first_id,
            "error":{"code":-32001,"message":"old attachment"}});
        shared
            .deliver_line(&serde_json::to_vec(&old).unwrap(), &stdin)
            .await;
        assert!(queued(&shared).await.is_empty());
        assert!(shared
            .native_channel
            .lock()
            .unwrap()
            .calls
            .contains_key(&second_id));
        let current = serde_json::json!({"jsonrpc":"2.0","id":second_id,
            "error":{"code":-32000,"message":"current observation failed"}});
        shared
            .deliver_line(&serde_json::to_vec(&current).unwrap(), &stdin)
            .await;
        let frames = queued(&shared).await;
        assert!(matches!(frames.as_slice(),
            [ControlBody::AgentError { call_id: 5, error }] if error.code == -32000));
        assert!(shared.native_channel.lock().unwrap().calls.is_empty());
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
                attachment_id.wrapping_add(1),
                call_id,
                Ok(serde_json::json!({"content": "wrong attachment"})),
                "s",
            )
            .await;
        let held_stdin = stdin.lock().await;
        let first = shared.resolve_server_call(
            &stdin,
            attachment_id,
            call_id,
            Ok(serde_json::json!({"content": "x"})),
            "s",
        );
        tokio::pin!(first);
        assert!(futures_util::poll!(&mut first).is_pending());
        assert!(!queued(&shared)
            .await
            .iter()
            .any(|frame| matches!(frame, ControlBody::ServerCallSettled { .. })));
        drop(held_stdin);
        assert!(first.await);
        shared
            .resolve_server_call(
                &stdin,
                attachment_id,
                call_id,
                Ok(serde_json::json!({"content": "y"})),
                "s",
            )
            .await;
        let frames = queued(&shared).await;
        let settlements: Vec<_> = frames
            .iter()
            .filter_map(|frame| match frame {
                ControlBody::ServerCallSettled { call_id } => Some(*call_id),
                _ => None,
            })
            .collect();
        assert_eq!(
            settlements,
            [call_id],
            "one actual terminal reverse attempt"
        );
        let written = read_agent_stdin(&stdin, &mut child).await;
        assert_eq!(
            written.trim().lines().count(),
            1,
            "exactly one response reached the agent: {written}"
        );
        shared.disconnect_control(attachment_id, &stdin, "s").await;
        shared.begin_attachment().await;
        assert!(
            queued(&shared).await.is_empty(),
            "settlement cannot replay to a replacement"
        );
    }

    #[tokio::test]
    async fn reverse_settlement_follows_a_failed_native_write() {
        let (shared, stdin, mut child) = shared_with_stdin().await;
        let attachment_id = shared.begin_attachment().await;
        shared
            .deliver_line(
                br#"{"jsonrpc":"2.0","id":3,"method":"fs/read_text_file","params":{}}"#,
                &stdin,
            )
            .await;
        let call_id = *shared
            .pending_server_calls
            .lock()
            .await
            .keys()
            .next()
            .unwrap();
        child.kill().await.unwrap();
        child.wait().await.unwrap();
        assert!(
            shared
                .resolve_server_call(
                    &stdin,
                    attachment_id,
                    call_id,
                    Ok(serde_json::json!({"content": "not delivered"})),
                    "s",
                )
                .await
        );
        assert_eq!(
            shared.native_loss(),
            Some(NativeChannelLossCause::WriteFailed)
        );
        let frames = queued(&shared).await;
        let settlements: Vec<_> = frames
            .iter()
            .filter_map(|frame| match frame {
                ControlBody::ServerCallSettled { call_id } => Some(*call_id),
                _ => None,
            })
            .collect();
        assert_eq!(
            settlements,
            [call_id],
            "settlement does not imply write success"
        );
        assert!(shared.pending_server_calls.lock().await.is_empty());
    }

    #[tokio::test]
    async fn full_control_queue_requires_connection_cleanup_after_reverse_write() {
        let (shared, stdin, mut child) = shared_with_stdin().await;
        let attachment_id = shared.begin_attachment().await;
        shared
            .deliver_line(
                br#"{"jsonrpc":"2.0","id":3,"method":"fs/read_text_file","params":{}}"#,
                &stdin,
            )
            .await;
        let call_id = *shared
            .pending_server_calls
            .lock()
            .await
            .keys()
            .next()
            .unwrap();
        {
            let mut channel = shared.control.lock().await;
            let wire = Arc::clone(&channel.queue.front().expect("actual reverse call").wire);
            // Saturate only this fixture's queue; no writer consumes these entries.
            while channel.queue.len() < control_protocol::MAX_CONTROL_QUEUE_FRAMES {
                channel.push(
                    DeliveryScope::Attachment(attachment_id),
                    QueuedKind::ServerCall,
                    Arc::clone(&wire),
                );
            }
        }
        let keep_connection = tokio::time::timeout(
            Duration::from_secs(10),
            shared.resolve_server_call(
                &stdin,
                attachment_id,
                call_id,
                Ok(serde_json::json!({"content": "x"})),
                "s",
            ),
        )
        .await
        .expect("terminal settlement cannot wait for queue space");
        assert!(
            !keep_connection,
            "the caller must close the original control socket"
        );
        assert!(shared.pending_server_calls.lock().await.is_empty());
        assert!(!queued(&shared)
            .await
            .iter()
            .any(|frame| matches!(frame, ControlBody::ServerCallSettled { .. })));
        let written = read_agent_stdin(&stdin, &mut child).await;
        let sent: serde_json::Value = serde_json::from_str(written.trim()).unwrap();
        assert_eq!(
            sent["result"]["content"], "x",
            "the actual Native write already ended"
        );
        shared.disconnect_control(attachment_id, &stdin, "s").await;
        assert!(queued(&shared).await.is_empty());
    }
}
