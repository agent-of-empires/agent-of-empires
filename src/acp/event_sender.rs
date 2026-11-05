//! One ordered SDK event lane, with private original-establishment accounting.

use std::sync::{Arc, Mutex, Weak};
use tokio::sync::{mpsc, oneshot, watch};

use super::control_protocol::NativeReplayDelivery;
use super::establishment::{
    EstablishmentAttempt, EstablishmentBlockReason, EstablishmentPostimage,
    NativeEstablishmentReceipt, NativeReplayClosure,
};
use super::establishment::{NativeRequestContext, NativeRequestScope};
use super::operation_effect::ControlKind;
use super::protocol::{
    AcpEventScope, AcpOperationMetadata, AcpOriginalRequestResult, AcpPromptObservation,
    AcpPromptWindowId,
};
use super::state::{AcpConnectionUnavailability, Event, NativeEventOrigin, NativeEventSubject};

/// The fourth value is private persisted provenance, never a public grant.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
#[repr(i64)]
pub(crate) enum EventScope {
    LegacyUnqualified = 0,
    #[default]
    HistoryOnly = 1,
    SessionCurrent = 2,
    OperationCurrent = 3,
}

impl EventScope {
    pub(crate) fn public(self) -> AcpEventScope {
        match self {
            Self::LegacyUnqualified | Self::HistoryOnly => AcpEventScope::HistoryOnly,
            Self::SessionCurrent => AcpEventScope::SessionCurrent,
            Self::OperationCurrent => AcpEventScope::OperationCurrent,
        }
    }

    pub(crate) fn from_sql(value: i64) -> anyhow::Result<Self> {
        match value {
            0 => Ok(Self::LegacyUnqualified),
            1 => Ok(Self::HistoryOnly),
            2 => Ok(Self::SessionCurrent),
            3 => Ok(Self::OperationCurrent),
            _ => anyhow::bail!("invalid persisted ACP event scope"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EventMetadata {
    #[serde(skip)]
    pub(crate) scope: EventScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) native_origin: Option<NativeEventOrigin>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) native_prompt_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// Exact Native payload SID, not permission or message authorship.
    pub(crate) reported_session_id: Option<agent_client_protocol::schema::v1::SessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) operation: Option<AcpOperationMetadata>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) prompt_observation: Option<AcpPromptObservation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) output_uncertainty: Option<AcpPromptWindowId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) clear_after_seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) resource_ref: Option<uuid::Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) resource_witness: Option<Arc<crate::acp::establishment::NativeResourceWitness>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) availability_denial: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) host_action: bool,
}

impl EventMetadata {
    pub(crate) const fn history() -> Self {
        Self {
            scope: EventScope::HistoryOnly,
            native_origin: None,
            native_prompt_id: None,
            reported_session_id: None,
            operation: None,
            prompt_observation: None,
            output_uncertainty: None,
            clear_after_seq: None,
            resource_ref: None,
            resource_witness: None,
            availability_denial: false,
            host_action: false,
        }
    }

    pub(crate) fn host(event: &Event) -> Self {
        let mut metadata = Self {
            host_action: true,
            ..Self::history()
        };
        if matches!(event, Event::AgentSwitched { .. }) {
            metadata.scope = EventScope::SessionCurrent;
        }
        metadata
    }

    pub(crate) fn historical(mut self) -> Self {
        self.make_historical();
        self
    }

    pub(crate) fn make_historical(&mut self) {
        if self.scope != EventScope::LegacyUnqualified {
            self.scope = EventScope::HistoryOnly;
        }
        self.availability_denial = false;
    }

    fn is_unqualified(&self) -> bool {
        self.native_origin.is_none()
            && self.native_prompt_id.is_none()
            && self.reported_session_id.is_none()
            && self.operation.is_none()
            && self.prompt_observation.is_none()
            && self.output_uncertainty.is_none()
            && self.clear_after_seq.is_none()
            && self.resource_ref.is_none()
            && self.resource_witness.is_none()
            && !self.availability_denial
            && !self.host_action
    }

    pub(crate) fn validate_for_event(&self, event: &Event) -> anyhow::Result<()> {
        let resource_matches =
            self.resource_witness
                .as_deref()
                .is_some_and(|witness| match event {
                    Event::BackgroundAgentLaunched {
                        agent_id,
                        tool_call_id,
                        output_file,
                        workflow,
                        ..
                    } => witness.matches_agent_launch(
                        agent_id,
                        tool_call_id,
                        output_file,
                        workflow.as_ref().map(|workflow| workflow.run_id.as_str()),
                    ),
                    Event::BackgroundTaskStarted {
                        task_id,
                        tool_call_id,
                        workflow,
                        ..
                    } => witness.matches_task_launch(
                        task_id,
                        tool_call_id.as_deref(),
                        workflow.as_ref(),
                    ),
                    _ => false,
                });
        let unavailable = match event {
            Event::AcpConnectionUnavailable {
                source,
                cause,
                phase,
                native_subject,
            } => Some((
                AcpConnectionUnavailability {
                    source: *source,
                    cause: *cause,
                    phase: *phase,
                },
                *native_subject,
            )),
            _ => None,
        };
        self.validate_shape(
            ControlKind::from_event(event),
            unavailable,
            resource_matches,
        )
    }

    pub(crate) fn validate_control(
        &self,
        kind: ControlKind<'_>,
        unavailable: Option<(AcpConnectionUnavailability, Option<NativeEventSubject>)>,
    ) -> anyhow::Result<()> {
        self.validate_shape(kind, unavailable, false)
    }

    fn validate_shape(
        &self,
        kind: ControlKind<'_>,
        unavailable: Option<(AcpConnectionUnavailability, Option<NativeEventSubject>)>,
        resource_matches: bool,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.clear_after_seq.is_none()
                || (matches!(kind, ControlKind::Cleared)
                    && self.native_origin.is_some()
                    && !self.host_action),
            "conversation display cut requires a qualified clear"
        );
        if let Some(id) = self.native_prompt_id {
            let Some(AcpPromptObservation::Written { window_id }) = self.prompt_observation else {
                anyhow::bail!("Native prompt identity has no actual Written window");
            };
            anyhow::ensure!(
                id > 0
                    && self.native_origin.is_some_and(|origin| origin
                        .owner
                        .is_some_and(|owner| owner.birth_fingerprint.is_some()))
                    && self
                        .operation
                        .is_some_and(|operation| operation.outcome.is_some())
                    && self
                        .output_uncertainty
                        .is_none_or(|known| known == window_id)
                    && !self.host_action
                    && !self.availability_denial
                    && self.resource_ref.is_none()
                    && self.resource_witness.is_none(),
                "Native prompt identity has no qualified original outcome"
            );
        }
        anyhow::ensure!(
            !self.host_action
                || ((self.scope == EventScope::HistoryOnly
                    || (self.scope == EventScope::SessionCurrent
                        && matches!(kind, ControlKind::Switched)))
                    && self.native_origin.is_none()
                    && self.reported_session_id.is_none()
                    && self.operation.is_none()
                    && self.prompt_observation.is_none()
                    && self.output_uncertainty.is_none()
                    && (self.resource_ref.is_none()
                        || matches!(kind, ControlKind::ResourceTrackingEnd))
                    && self.resource_witness.is_none()
                    && !self.availability_denial),
            "host action cannot claim Native qualification"
        );
        anyhow::ensure!(
            self.resource_ref.is_none() || self.scope == EventScope::HistoryOnly,
            "resource history cannot grant current state"
        );
        if let Some(witness) = self.resource_witness.as_deref() {
            anyhow::ensure!(
                resource_matches
                    && self.resource_ref == Some(witness.allocation)
                    && self.native_origin == Some(witness.origin),
                "resource witness does not match its launch"
            );
        } else {
            anyhow::ensure!(
                self.resource_ref.is_none() || !matches!(kind, ControlKind::ResourceLaunch),
                "qualified resource launch has no first-I/O witness"
            );
        }
        let current = matches!(
            self.scope,
            EventScope::SessionCurrent | EventScope::OperationCurrent
        );
        anyhow::ensure!(
            !current
                || !matches!(kind, ControlKind::Unavailable { .. })
                || self.availability_denial,
            "current ACP unavailability requires a qualified denial"
        );
        if !self.availability_denial {
            return Ok(());
        }
        anyhow::ensure!(
            self.scope == EventScope::SessionCurrent
                && self.operation.is_none()
                && self.prompt_observation.is_none()
                && self.output_uncertainty.is_none()
                && self.resource_ref.is_none()
                && self.resource_witness.is_none(),
            "availability denial cannot carry positive operation or resource qualification"
        );
        let (observed, subject) = match (kind, unavailable) {
            (ControlKind::Unavailable { .. }, Some((observed, Some(subject)))) => {
                (observed, subject)
            }
            _ => anyhow::bail!("availability denial has no private control observation"),
        };
        let consistent = match subject.kind {
            crate::acp::state::NativeSubjectEvent::ChannelUnavailable(known) => known == observed,
            crate::acp::state::NativeSubjectEvent::AdmissionRefusal(refusal) => {
                refusal.observation() == observed
            }
            _ => false,
        };
        anyhow::ensure!(
            consistent,
            "availability denial differs from its private source"
        );
        Ok(())
    }

    pub(crate) fn encode_private(&self) -> anyhow::Result<Option<String>> {
        anyhow::ensure!(
            !matches!(
                self.scope,
                EventScope::SessionCurrent | EventScope::OperationCurrent
            ) || self.native_origin.is_some()
                || self.availability_denial
                || self.host_action,
            "current ACP event has no Native source"
        );
        anyhow::ensure!(
            self.scope != EventScope::LegacyUnqualified || self.is_unqualified(),
            "legacy ACP event cannot carry qualification"
        );
        if self.is_unqualified() {
            anyhow::ensure!(
                matches!(
                    self.scope,
                    EventScope::LegacyUnqualified | EventScope::HistoryOnly
                ),
                "current ACP event has no qualification"
            );
            return Ok(None);
        }
        Ok(Some(serde_json::to_string(self)?))
    }

    pub(crate) fn decode_private(scope: i64, json: Option<&str>) -> anyhow::Result<Self> {
        let scope = EventScope::from_sql(scope)?;
        anyhow::ensure!(
            scope != EventScope::LegacyUnqualified || json.is_none(),
            "legacy ACP event cannot carry persisted qualification"
        );
        let mut metadata = match json {
            Some(json) => serde_json::from_str::<Self>(json)?,
            None => {
                anyhow::ensure!(
                    matches!(
                        scope,
                        EventScope::LegacyUnqualified | EventScope::HistoryOnly
                    ),
                    "current ACP event has no persisted qualification"
                );
                Self::history()
            }
        };
        metadata.scope = scope;
        anyhow::ensure!(
            matches!(
                scope,
                EventScope::LegacyUnqualified | EventScope::HistoryOnly
            ) || metadata.native_origin.is_some()
                || metadata.availability_denial
                || metadata.host_action,
            "current ACP event has no persisted qualification"
        );
        anyhow::ensure!(
            metadata.resource_ref.is_none() || scope == EventScope::HistoryOnly,
            "resource history cannot grant current state"
        );
        if let Some(witness) = metadata.resource_witness.as_deref() {
            anyhow::ensure!(
                metadata.resource_ref == Some(witness.allocation)
                    && metadata.native_origin == Some(witness.origin),
                "ACP resource witness differs from its event source"
            );
        }
        Ok(metadata)
    }
}

/// One payload allocation is shared by the drain, sink and public frame.
/// Frozen qualification travels in the channel, never in a parallel ledger.
#[derive(Debug)]
pub struct AcpEventData {
    pub event: Arc<Event>,
    pub(crate) metadata: EventMetadata,
    pub(crate) current_scope: Option<Arc<NativeRequestContext>>,
    pub(crate) constructor_scope:
        Option<Arc<crate::acp::establishment::NativeConstructorObservation>>,
    pub(crate) journal_publication: Option<oneshot::Sender<bool>>,
    pub(crate) pending_request: Option<Arc<super::acp_client::pending::PendingRequestPublication>>,
}

impl AcpEventData {
    pub(crate) fn from_host(event: Event) -> Self {
        let metadata = EventMetadata::host(&event);
        Self {
            event: Arc::new(event),
            metadata,
            current_scope: None,
            constructor_scope: None,
            journal_publication: None,
            pending_request: None,
        }
    }

    pub(crate) fn from_history(event: Event) -> Self {
        Self {
            event: Arc::new(event),
            metadata: EventMetadata::history(),
            current_scope: None,
            constructor_scope: None,
            journal_publication: None,
            pending_request: None,
        }
    }
}

/// Private SDK barriers do not allocate or masquerade as public Event payloads.
#[derive(Debug)]
pub enum SdkEvent {
    Data(AcpEventData),
    EstablishmentClosed {
        delivery: Arc<EstablishmentDelivery>,
    },
    ControlPlaneLoss(ControlPlaneLoss),
}

pub struct ControlPlaneLoss {
    pub(crate) connection: Arc<crate::acp::acp_client::ConnectionState>,
    pub(crate) observation: crate::acp::state::AcpConnectionUnavailability,
    pub(crate) origin: Option<NativeEventOrigin>,
}

impl std::fmt::Debug for ControlPlaneLoss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlPlaneLoss")
            .field("observation", &self.observation)
            .field("origin", &self.origin)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone)]
pub(crate) enum EstablishmentResolution {
    Pending,
    Applied(Arc<EstablishmentPostimage>),
    Retiring,
    Blocked,
    Superseded,
    Unconfirmed,
}

/// Never serialized as a public event. The drain handles this before any
/// retirement filtering, publication, or sequence allocation.
pub struct EstablishmentDelivery {
    pub(crate) attempt: Arc<EstablishmentAttempt>,
    pub(crate) receipt: Option<Arc<NativeEstablishmentReceipt>>,
    pub(crate) failure: Option<EstablishmentBlockReason>,
    pub(crate) first: u64,
    pub(crate) through: u64,
    pub(crate) original: bool,
    pub(crate) complete: bool,
    resolution: watch::Sender<EstablishmentResolution>,
}

impl std::fmt::Debug for EstablishmentDelivery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EstablishmentDelivery")
            .finish_non_exhaustive()
    }
}

impl EstablishmentDelivery {
    pub(crate) fn resolve(&self, result: EstablishmentResolution) {
        self.resolution.send_replace(result);
    }

    pub(crate) async fn settled(&self) -> EstablishmentResolution {
        let mut result = self.resolution.subscribe();
        loop {
            let value = result.borrow().clone();
            if !matches!(value, EstablishmentResolution::Pending) {
                return value;
            }
            if result.changed().await.is_err() {
                return EstablishmentResolution::Unconfirmed;
            }
        }
    }
}

#[derive(Clone, Copy)]
enum RetirementCheckpoint {
    Native,
    PermanentForget(uuid::Uuid),
}

struct ProjectionWindow {
    attempt: Arc<EstablishmentAttempt>,
    receipt: Option<Arc<NativeEstablishmentReceipt>>,
    first: u64,
    original: bool,
    rpc_possible: bool,
    raw_first: Option<u64>,
    wire_range: Option<NativeReplayDelivery>,
    raw_seen: u64,
    wire_closed: bool,
    wire_forwarded: bool,
    lost: bool,
    delivery: Option<Arc<EstablishmentDelivery>>,
    sealed: bool,
    retirement_checkpoint: Option<RetirementCheckpoint>,
    clear_after_seq: Option<u64>,
}

impl ProjectionWindow {
    fn new(attempt: Arc<EstablishmentAttempt>, first: u64, original: bool) -> Self {
        Self {
            attempt,
            receipt: None,
            first,
            original,
            rpc_possible: !original,
            raw_first: None,
            wire_range: None,
            raw_seen: 0,
            wire_closed: false,
            wire_forwarded: false,
            lost: false,
            delivery: None,
            sealed: false,
            retirement_checkpoint: None,
            clear_after_seq: None,
        }
    }
}

#[derive(Default)]
struct Emissions {
    next: u64,
    window: Option<ProjectionWindow>,
}

#[derive(Default)]
pub(crate) struct ProjectionState {
    emissions: Mutex<Emissions>,
    changed: tokio::sync::Notify,
    received: std::sync::atomic::AtomicU64,
    replay: Mutex<Option<crate::acp::event_store::ReplayCommit>>,
}

impl std::fmt::Debug for ProjectionState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectionState").finish_non_exhaustive()
    }
}

impl ProjectionState {
    pub(crate) fn new(attempt: Option<Arc<EstablishmentAttempt>>) -> Self {
        let state = Self::default();
        if let Some(attempt) = attempt {
            state.emissions.lock().expect("SDK projection lock").window =
                Some(ProjectionWindow::new(attempt, 0, true));
        }
        state
    }

    pub(crate) fn read_only(attempt: Option<Arc<EstablishmentAttempt>>) -> Self {
        let state = Self::default();
        if let Some(attempt) = attempt {
            state.emissions.lock().expect("SDK projection lock").window =
                Some(ProjectionWindow::new(attempt, 0, false));
        }
        state
    }

    pub(crate) fn observe_original_invocation(&self, attempt: Arc<EstablishmentAttempt>) -> bool {
        let mut emissions = self.emissions.lock().expect("SDK projection lock");
        let first = emissions.next;
        let Some(window) = emissions.window.as_mut() else {
            return false;
        };
        if window.sealed
            || window.lost
            || window.raw_first.is_some()
            || window.delivery.is_some()
            || window.receipt.is_some()
            || !window.attempt.same_source(&attempt)
        {
            return false;
        }
        if !window.original {
            window.first = first;
        }
        window.attempt = attempt;
        window.original = true;
        window.rpc_possible = true;
        self.changed.notify_waiters();
        true
    }

    pub(crate) fn rpc_may_be_issued(&self, id: super::establishment::AttemptId) -> bool {
        let mut emissions = self.emissions.lock().expect("SDK projection lock");
        let Some(window) = emissions.window.as_mut() else {
            return false;
        };
        if window.sealed || window.attempt.id != id {
            return false;
        }
        window.rpc_possible = true;
        true
    }

    /// Replace only a positively committed predecessor, never an unknown replay.
    /// The caller has already durably armed this exact Fresh attempt.
    pub(crate) fn begin(
        &self,
        attempt: Arc<EstablishmentAttempt>,
        predecessor: &Arc<EstablishmentDelivery>,
        postimage: &Arc<EstablishmentPostimage>,
        seal: &crate::acp::event_store::EstablishmentSeal,
    ) -> bool {
        use super::establishment::{EstablishmentOrigin, EstablishmentTarget};
        use std::sync::atomic::Ordering;
        let mut emissions = self.emissions.lock().expect("SDK projection lock");
        let Some(window) = emissions.window.as_ref() else {
            return false;
        };
        let (Some(receipt), Some(delivery)) = (&window.receipt, &window.delivery) else {
            return false;
        };
        let applied = matches!(&*delivery.resolution.borrow(),
            EstablishmentResolution::Applied(current) if Arc::ptr_eq(current, postimage));
        if window.lost
            || window.sealed
            || !window.wire_closed
            || !window.wire_forwarded
            || !delivery.complete
            || delivery.failure.is_some()
            || !applied
            || !Arc::ptr_eq(delivery, predecessor)
            || !delivery
                .receipt
                .as_ref()
                .is_some_and(|original| Arc::ptr_eq(original, receipt))
            || delivery.attempt.as_ref() != window.attempt.as_ref()
            || receipt.attempt.as_ref() != window.attempt.as_ref()
            || !receipt.replay.closed
            || receipt.replay.lost
            || receipt.session_id().is_none()
            || !seal.matches(receipt)
            || postimage.attempt_id != window.attempt.id
            || self.received.load(Ordering::Acquire) != emissions.next
            || attempt.id == window.attempt.id
            || attempt.origin != EstablishmentOrigin::Fresh
            || attempt.target != EstablishmentTarget::New
            || attempt.preimage != postimage.choice
            || attempt.source_context != postimage.context
            || attempt.context != postimage.context
            || attempt.profile != window.attempt.profile
            || attempt.session_id != window.attempt.session_id
            || attempt.created_at != window.attempt.created_at
            || attempt.producer.is_none()
        {
            return false;
        }
        *self.replay.lock().expect("original replay progress") = None;
        let first = emissions.next;
        emissions.window = Some(ProjectionWindow::new(attempt, first, true));
        drop(emissions);
        self.changed.notify_waiters();
        true
    }

    pub(crate) fn emission_cut(&self) -> u64 {
        self.emissions.lock().expect("SDK projection lock").next
    }

    /// A processed frontier is not itself a native establishment permission.
    pub(crate) async fn processed_through(&self, cut: u64, deadline: tokio::time::Instant) -> bool {
        use std::sync::atomic::Ordering;
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            if self.received.load(Ordering::Acquire) >= cut {
                return true;
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                return false;
            }
        }
    }

    pub(crate) fn attempt(&self) -> Option<Arc<EstablishmentAttempt>> {
        self.emissions
            .lock()
            .expect("SDK projection lock")
            .window
            .as_ref()
            .map(|window| Arc::clone(&window.attempt))
    }

    pub(crate) fn receipt_snapshot(&self) -> Option<Arc<NativeEstablishmentReceipt>> {
        self.emissions
            .lock()
            .expect("SDK projection lock")
            .window
            .as_ref()
            .and_then(|window| window.receipt.clone())
    }

    pub(crate) fn applied_clear_after_seq(
        &self,
        receipt: &NativeEstablishmentReceipt,
    ) -> anyhow::Result<u64> {
        use super::establishment::EstablishmentRefusal;
        let emissions = self.emissions.lock().expect("SDK projection lock");
        let window = emissions
            .window
            .as_ref()
            .ok_or(EstablishmentRefusal::Unconfirmed)?;
        let delivery = window
            .delivery
            .as_ref()
            .ok_or(EstablishmentRefusal::Unconfirmed)?;
        anyhow::ensure!(
            !window.lost
                && window.wire_closed
                && window.wire_forwarded
                && delivery.complete
                && delivery.failure.is_none()
                && window.receipt.as_deref() == Some(receipt)
                && window.receipt.as_ref().and_then(|known| known
                    .store
                    .as_ref()
                    .and_then(|store| store.exported_default_store.as_ref()))
                    == receipt
                        .store
                        .as_ref()
                        .and_then(|store| store.exported_default_store.as_ref())
                && matches!(&*delivery.resolution.borrow(),
                    EstablishmentResolution::Applied(postimage)
                        if postimage.attempt_id == receipt.attempt.id),
            EstablishmentRefusal::Unconfirmed
        );
        window
            .clear_after_seq
            .ok_or_else(|| EstablishmentRefusal::Unconfirmed.into())
    }

    pub(crate) fn failure_reason(&self) -> Option<EstablishmentBlockReason> {
        let emissions = self.emissions.lock().expect("SDK projection lock");
        match &emissions.window.as_ref()?.receipt.as_ref()?.outcome {
            super::establishment::NativeEstablishmentOutcome::Failed { reason } => Some(*reason),
            super::establishment::NativeEstablishmentOutcome::Succeeded { .. } => None,
        }
    }

    pub(crate) fn original_window(&self) -> Option<(Arc<EstablishmentAttempt>, u64)> {
        self.emissions
            .lock()
            .expect("SDK projection lock")
            .window
            .as_ref()
            .filter(|window| window.original)
            .map(|window| (Arc::clone(&window.attempt), window.first))
    }

    pub(crate) fn receipt(
        &self,
        receipt: Arc<NativeEstablishmentReceipt>,
        original_delivery: bool,
        range: NativeReplayDelivery,
    ) -> bool {
        let mut emissions = self.emissions.lock().expect("SDK projection lock");
        if emissions.window.is_none() {
            let first = emissions.next;
            emissions.window = Some(ProjectionWindow::new(
                Arc::clone(&receipt.attempt),
                first,
                false,
            ));
        }
        let window = emissions.window.as_mut().expect("projection window exists");
        if window.sealed || window.delivery.is_some() {
            return false;
        }
        if !window.attempt.same_source(&receipt.attempt) {
            window.lost = true;
            return false;
        }
        if let Some(original) = &window.receipt {
            let same = original.as_ref() == receipt.as_ref()
                && original
                    .store
                    .as_ref()
                    .and_then(|store| store.exported_default_store.as_ref())
                    == receipt
                        .store
                        .as_ref()
                        .and_then(|store| store.exported_default_store.as_ref())
                && window.wire_range == Some(range);
            window.lost |= !same;
            return same;
        }
        window.attempt = Arc::clone(&receipt.attempt);
        window.original &= original_delivery;
        window.receipt = Some(receipt);
        window.lost |= !range.matches(window.receipt.as_ref().expect("captured receipt").replay);
        window.wire_range = Some(range);
        self.changed.notify_waiters();
        true
    }

    pub(crate) fn frame(&self, frame: &super::control_protocol::NativeReplayFrame) -> bool {
        let mut emissions = self.emissions.lock().expect("SDK projection lock");
        let Some(window) = emissions.window.as_mut() else {
            return false;
        };
        if window.sealed || window.delivery.is_some() {
            return false;
        }
        if frame.attempt_id != window.attempt.id {
            window.lost = true;
            return false;
        }
        if window.raw_first.is_some() {
            window.lost |= window.raw_seen.checked_add(1) != Some(frame.ordinal);
        } else {
            window.raw_first = Some(frame.ordinal);
            window.lost |= frame.ordinal == 0;
        }
        window.lost |= window
            .wire_range
            .is_some_and(|range| frame.ordinal > range.through);
        window.raw_seen = frame.ordinal;
        !window.lost
    }

    pub(crate) fn close_wire(
        &self,
        attempt_id: super::establishment::AttemptId,
        closure: NativeReplayClosure,
        range: NativeReplayDelivery,
    ) {
        let mut emissions = self.emissions.lock().expect("SDK projection lock");
        let Some(window) = emissions.window.as_mut() else {
            return;
        };
        if window.sealed || window.delivery.is_some() {
            return;
        }
        let covered = match window.raw_first {
            Some(first) => {
                first <= range.after.saturating_add(1) && window.raw_seen == range.through
            }
            None => range.after == range.through,
        };
        let matches = window.attempt.id == attempt_id
            && range.matches(closure)
            && window.wire_range == Some(range)
            && covered
            && window
                .receipt
                .as_ref()
                .is_some_and(|receipt| receipt.replay == closure);
        window.lost |= !matches;
        window.wire_closed = matches;
        // Full current-tail delivery cannot manufacture a missing original prefix.
        window.original &= if closure.notifications == 0 {
            window.raw_first.is_none()
        } else {
            window.raw_first == Some(1) && window.raw_seen == closure.notifications
        };
        self.changed.notify_waiters();
    }

    pub(crate) fn lost(&self) {
        let mut emissions = self.emissions.lock().expect("SDK projection lock");
        if let Some(window) = emissions
            .window
            .as_mut()
            .filter(|window| window.delivery.is_none())
        {
            window.lost = true;
        }
        self.changed.notify_waiters();
    }

    fn processing_lost(&self) {
        let mut emissions = self.emissions.lock().expect("SDK projection lock");
        if let Some(window) = emissions.window.as_mut() {
            window.lost = true;
        }
    }

    pub(crate) fn wire_forwarded(&self, marker: &super::control_protocol::SessionReplayed) {
        let mut emissions = self.emissions.lock().expect("SDK projection lock");
        if let Some(window) = emissions.window.as_mut() {
            let matches = marker.attempt_id == window.attempt.id
                && window.wire_range == Some(marker.delivery)
                && window
                    .receipt
                    .as_ref()
                    .is_some_and(|receipt| receipt.replay == marker.closure);
            window.lost |= !matches;
            window.wire_forwarded = matches;
        }
        self.changed.notify_waiters();
    }

    pub(crate) fn control_ended(&self) {
        let mut emissions = self.emissions.lock().expect("SDK projection lock");
        if let Some(window) = emissions
            .window
            .as_mut()
            .filter(|window| window.delivery.is_none() && !window.wire_forwarded)
        {
            window.lost = true;
        }
        self.changed.notify_waiters();
    }

    pub(crate) async fn wait_wire(&self) -> bool {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let emissions = self.emissions.lock().expect("SDK projection lock");
                match emissions.window.as_ref() {
                    Some(window) if window.wire_closed => return !window.lost,
                    Some(window)
                        if !window.lost
                            && window
                                .receipt
                                .as_ref()
                                .is_some_and(|receipt| receipt.replay.closed) => {}
                    _ => return false,
                }
            }
            changed.await;
        }
    }

    pub(crate) fn retirement_checkpoint_pending(&self) -> bool {
        self.emissions
            .lock()
            .expect("SDK projection lock")
            .window
            .as_ref()
            .is_some_and(|window| {
                window.delivery.is_some() && window.retirement_checkpoint.is_none()
            })
    }

    pub(crate) fn finish_retirement_checkpoint(
        &self,
        delivery: &Arc<EstablishmentDelivery>,
    ) -> bool {
        let mut emissions = self.emissions.lock().expect("SDK projection lock");
        let Some(window) = emissions.window.as_mut() else {
            return false;
        };
        if !window.sealed
            || matches!(
                window.retirement_checkpoint,
                Some(RetirementCheckpoint::PermanentForget(_))
            )
            || !window
                .delivery
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, delivery))
        {
            return false;
        }
        window.retirement_checkpoint = Some(RetirementCheckpoint::Native);
        true
    }

    /// A committed permanent purge retires its exact stopped source, not a
    /// Native result or a checkpoint against the row that purge removed.
    pub(crate) fn finish_permanent_forget(
        &self,
        delivery: &Arc<EstablishmentDelivery>,
        proof: &crate::session::purge_owners::PermanentEstablishmentRetirement<'_>,
    ) -> anyhow::Result<()> {
        use super::establishment::EstablishmentRefusal;
        let mut emissions = self.emissions.lock().expect("SDK projection lock");
        let window = emissions
            .window
            .as_mut()
            .ok_or(EstablishmentRefusal::Unconfirmed)?;
        anyhow::ensure!(
            window.sealed
                && window
                    .delivery
                    .as_ref()
                    .is_some_and(|current| Arc::ptr_eq(current, delivery))
                && proof.matches(&window.attempt, window.receipt.as_deref())
                && proof.matches(&delivery.attempt, delivery.receipt.as_deref()),
            EstablishmentRefusal::Superseded
        );
        match window.retirement_checkpoint {
            Some(RetirementCheckpoint::Native) => return Ok(()),
            Some(RetirementCheckpoint::PermanentForget(token)) => {
                anyhow::ensure!(token == proof.token(), EstablishmentRefusal::Superseded);
            }
            None => {
                window.retirement_checkpoint =
                    Some(RetirementCheckpoint::PermanentForget(proof.token()));
            }
        }
        delivery.resolve(EstablishmentResolution::Retiring);
        drop(emissions);
        self.changed.notify_waiters();
        Ok(())
    }

    pub(crate) fn seal_retirement(&self) -> Option<Arc<EstablishmentDelivery>> {
        let mut emissions = self.emissions.lock().expect("SDK projection lock");
        let through = emissions.next;
        let window = emissions.window.as_mut()?;
        window.sealed = true;
        if let Some(delivery) = &window.delivery {
            return Some(Arc::clone(delivery));
        }
        let receipt = window.receipt.clone();
        let failure = match receipt.as_deref().map(|receipt| &receipt.outcome) {
            Some(super::establishment::NativeEstablishmentOutcome::Failed { reason }) => {
                Some(*reason)
            }
            Some(super::establishment::NativeEstablishmentOutcome::Succeeded { .. }) => None,
            None if window.original && !window.rpc_possible => {
                Some(EstablishmentBlockReason::NotIssued)
            }
            None => return None,
        };
        let (resolution, _) = watch::channel(EstablishmentResolution::Pending);
        let delivery = Arc::new(EstablishmentDelivery {
            attempt: Arc::clone(&window.attempt),
            receipt,
            failure,
            first: window.first,
            through,
            original: window.original,
            complete: false,
            resolution,
        });
        window.delivery = Some(Arc::clone(&delivery));
        self.changed.notify_waiters();
        Some(delivery)
    }

    pub(crate) fn has_unsettled_completion(&self) -> bool {
        let emissions = self.emissions.lock().expect("SDK projection lock");
        emissions
            .window
            .as_ref()
            .is_some_and(|window| match &window.delivery {
                Some(delivery) => matches!(
                    &*delivery.resolution.borrow(),
                    EstablishmentResolution::Pending
                ),
                None => window
                    .receipt
                    .as_ref()
                    .is_some_and(|receipt| receipt.replay.closed),
            })
    }

    pub(crate) fn has_completed_original(&self) -> bool {
        self.emissions
            .lock()
            .expect("SDK projection lock")
            .window
            .as_ref()
            .is_some_and(|window| {
                window.delivery.is_none()
                    && window
                        .receipt
                        .as_ref()
                        .is_some_and(|receipt| receipt.replay.closed)
            })
    }

    pub(crate) async fn original_delivered(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !self.has_completed_original() {
                return;
            }
            changed.await;
        }
    }

    pub(crate) async fn settled(&self) -> EstablishmentResolution {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let delivery = {
                let emissions = self.emissions.lock().expect("SDK projection lock");
                match emissions.window.as_ref() {
                    Some(window) if window.delivery.is_some() => window.delivery.clone(),
                    Some(window) if window.lost => return EstablishmentResolution::Unconfirmed,
                    _ => None,
                }
            };
            if let Some(delivery) = delivery {
                return delivery.settled().await;
            }
            changed.await;
        }
    }

    pub(crate) fn checkpoint_under_native_lock(
        &self,
        store: &dyn crate::session::SessionStore,
        sink: &dyn crate::acp::supervisor::BroadcastSink,
        destination: &crate::acp::state::NativeObservationScope,
    ) -> anyhow::Result<(
        EstablishmentResolution,
        crate::acp::state::NativeObservationScope,
        u64,
    )> {
        self.checkpoint(store, sink, destination, false)
    }

    pub(crate) fn retirement_checkpoint_under_native_lock(
        &self,
        store: &dyn crate::session::SessionStore,
        sink: &dyn crate::acp::supervisor::BroadcastSink,
        destination: &crate::acp::state::NativeObservationScope,
    ) -> anyhow::Result<(
        EstablishmentResolution,
        crate::acp::state::NativeObservationScope,
        u64,
    )> {
        self.checkpoint(store, sink, destination, true)
    }

    fn checkpoint(
        &self,
        store: &dyn crate::session::SessionStore,
        sink: &dyn crate::acp::supervisor::BroadcastSink,
        destination: &crate::acp::state::NativeObservationScope,
        retiring: bool,
    ) -> anyhow::Result<(
        EstablishmentResolution,
        crate::acp::state::NativeObservationScope,
        u64,
    )> {
        use super::establishment::{
            AcpEstablishment, EstablishmentRefusal, NativeEstablishmentOutcome,
        };
        let delivery = self.delivery().ok_or(EstablishmentRefusal::Unconfirmed)?;
        let id = &delivery.attempt.session_id;
        let mut replay = self.take_replay().unwrap_or_else(|| {
            crate::acp::event_store::ReplayCommit::new(
                Arc::clone(&delivery.attempt),
                delivery.first,
            )
        });
        if !delivery.complete {
            replay.reject();
        }
        let reason = delivery
            .failure
            .or_else(|| match &delivery.receipt.as_deref()?.outcome {
                NativeEstablishmentOutcome::Failed { reason } => Some(*reason),
                NativeEstablishmentOutcome::Succeeded { .. } => None,
            });
        let receipt = delivery.receipt.as_deref();
        let blocked_reason = reason.unwrap_or(EstablishmentBlockReason::ReplayUnconfirmed);
        let mut resolved = None;
        let mut certified_clear_after_seq = None;
        let mut apply = |rows: &mut Vec<crate::session::Instance>,
                         _: &mut Vec<crate::session::Group>| {
            let row = rows
                .iter_mut()
                .find(|row| row.id == *id)
                .ok_or(crate::session::SessionGone)?;
            anyhow::ensure!(
                destination.context
                    == crate::acp::state::NativeEventContext::capture(row, store.storage())?
                    && delivery
                        .attempt
                        .matches_physical_source(row, store.storage())?,
                EstablishmentRefusal::Superseded
            );
            let already_applied = receipt.is_some_and(|receipt| {
                row.acp_establishment
                    == Some(AcpEstablishment::Applied {
                        attempt_id: receipt.attempt.id,
                    })
            });
            let already_blocked = matches!(&row.acp_establishment,
                Some(AcpEstablishment::Blocked { attempt, reason })
                    if attempt.same_source(&delivery.attempt)
                        && *reason == blocked_reason
                        && attempt.context == destination.context
                        && attempt.destination.matches(row));
            let pending_matches = matches!(&row.acp_establishment,
                Some(AcpEstablishment::Pending { attempt })
                    if attempt.same_source(&delivery.attempt)
                        && attempt.context == destination.context
                        && attempt.generation == row.lifecycle_generation
                        && attempt.destination == delivery.attempt.destination
                        && attempt.destination.matches(row));
            anyhow::ensure!(
                pending_matches || already_applied || already_blocked,
                EstablishmentRefusal::Superseded
            );
            anyhow::ensure!(
                destination.choice.as_ref().is_some_and(|choice| {
                    choice.matches(row) || (already_blocked && choice.selection_matches(row))
                }) || already_applied,
                EstablishmentRefusal::Superseded
            );
            if retiring && already_applied {
                let postimage = receipt
                    .ok_or(EstablishmentRefusal::Unconfirmed)?
                    .postimage(destination.context)?;
                anyhow::ensure!(
                    postimage.matches(row, store.storage())?,
                    EstablishmentRefusal::Superseded
                );
                // Existing Applied authority needs no new replay or readiness grant.
                resolved = Some((
                    EstablishmentResolution::Retiring,
                    crate::acp::state::NativeObservationScope::capture(row, store.storage())?,
                    row.lifecycle_generation,
                ));
                return Ok(());
            }
            let seal = if reason.is_none() && !already_blocked {
                if let (Some(receipt), Some(journal)) = (receipt, sink.establishment_store()) {
                    match journal.establishment_seal(receipt) {
                        Ok(seal) => Some(seal),
                        Err(error) if error.is::<EstablishmentRefusal>() => {
                            if delivery.original && !already_applied {
                                match journal.seal_establishment(&replay, &delivery) {
                                    Ok(seal) => Some(seal),
                                    Err(error) if error.is::<EstablishmentRefusal>() => None,
                                    Err(error) => return Err(error),
                                }
                            } else {
                                None
                            }
                        }
                        Err(error) => return Err(error),
                    }
                } else {
                    None
                }
            } else {
                None
            };
            anyhow::ensure!(
                !already_applied || seal.is_some(),
                EstablishmentRefusal::Unconfirmed
            );
            certified_clear_after_seq = seal.as_ref().and_then(|seal| seal.clear_after_seq().ok());
            let resolution = if let (Some(receipt), Some(seal)) = (receipt, seal.as_ref()) {
                let postimage = row.commit_acp_establishment(
                    store.storage(),
                    receipt,
                    seal,
                    destination.context,
                )?;
                EstablishmentResolution::Applied(Arc::new(postimage))
            } else if already_blocked {
                EstablishmentResolution::Blocked
            } else {
                let current = match &row.acp_establishment {
                    Some(AcpEstablishment::Pending { attempt })
                        if attempt.same_source(&delivery.attempt) =>
                    {
                        attempt.as_ref().clone()
                    }
                    _ => anyhow::bail!(EstablishmentRefusal::Superseded),
                };
                row.block_acp_establishment(store.storage(), &current, blocked_reason)?;
                EstablishmentResolution::Blocked
            };
            let scope = crate::acp::state::NativeObservationScope::capture(row, store.storage())?;
            resolved = Some((resolution, scope, row.lifecycle_generation));
            Ok(())
        };
        let committed = store.commit_metadata_checkpoint(&mut apply);
        self.retain_replay(Some(replay));
        match committed {
            Ok(()) => {
                let resolved = resolved.ok_or(EstablishmentRefusal::Unconfirmed)?;
                if matches!(&resolved.0, EstablishmentResolution::Applied(_)) {
                    let mut emissions = self.emissions.lock().expect("SDK projection lock");
                    let window = emissions
                        .window
                        .as_mut()
                        .ok_or(EstablishmentRefusal::Unconfirmed)?;
                    anyhow::ensure!(
                        window
                            .delivery
                            .as_ref()
                            .is_some_and(|known| Arc::ptr_eq(known, &delivery))
                            && window.receipt.as_deref() == receipt,
                        EstablishmentRefusal::Superseded
                    );
                    window.clear_after_seq = certified_clear_after_seq;
                }
                Ok(resolved)
            }
            Err(error) if error.is::<crate::session::SessionCommitApplied>() => {
                let (_, scope, _) = resolved.ok_or(EstablishmentRefusal::Unconfirmed)?;
                let rows = store.storage().load_instances_complete()?;
                let row = rows
                    .iter()
                    .find(|row| row.id == *id)
                    .ok_or(crate::session::SessionGone)?;
                anyhow::ensure!(
                    scope.context
                        == crate::acp::state::NativeEventContext::capture(row, store.storage())?
                        && scope
                            .choice
                            .as_ref()
                            .is_some_and(|choice| choice.matches(row)),
                    EstablishmentRefusal::Superseded
                );
                Err(error)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn delivery(&self) -> Option<Arc<EstablishmentDelivery>> {
        self.emissions
            .lock()
            .expect("SDK projection lock")
            .window
            .as_ref()
            .and_then(|window| window.delivery.clone())
    }

    pub(crate) fn original_range_contains(&self, ordinal: u64) -> bool {
        self.emissions
            .lock()
            .expect("SDK projection lock")
            .window
            .as_ref()
            .is_some_and(|window| {
                window.original
                    && ordinal >= window.first
                    && window
                        .delivery
                        .as_ref()
                        .is_none_or(|delivery| ordinal < delivery.through)
            })
    }

    pub(crate) fn received_event(&self) -> Option<ProcessedEvent<'_>> {
        let ordinal = self.received.load(std::sync::atomic::Ordering::Acquire);
        if ordinal == u64::MAX {
            self.lost();
            return None;
        }
        Some(ProcessedEvent {
            state: self,
            ordinal,
            complete: false,
        })
    }

    pub(crate) fn take_replay(&self) -> Option<crate::acp::event_store::ReplayCommit> {
        self.replay.lock().expect("original replay progress").take()
    }

    pub(crate) fn retain_replay(&self, replay: Option<crate::acp::event_store::ReplayCommit>) {
        *self.replay.lock().expect("original replay progress") = replay;
    }

    pub(crate) fn applied(&self) -> Option<Arc<EstablishmentPostimage>> {
        let delivery = {
            let emissions = self.emissions.lock().expect("SDK projection lock");
            let window = emissions.window.as_ref()?;
            if window.lost {
                return None;
            }
            window.delivery.clone()?
        };
        let resolution = delivery.resolution.borrow();
        match &*resolution {
            EstablishmentResolution::Applied(postimage) => Some(Arc::clone(postimage)),
            _ => None,
        }
    }
}

/// One consumer owns the SDK receiver. Dropping an interrupted publication
/// rejects its projection before advancing the processed frontier.
pub(crate) struct ProcessedEvent<'a> {
    state: &'a ProjectionState,
    ordinal: u64,
    complete: bool,
}

impl ProcessedEvent<'_> {
    pub(crate) fn ordinal(&self) -> u64 {
        self.ordinal
    }

    pub(crate) fn complete(&mut self) {
        self.complete = true;
    }
}

impl Drop for ProcessedEvent<'_> {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        if !self.complete {
            self.state.processing_lost();
        }
        if self
            .state
            .received
            .compare_exchange(
                self.ordinal,
                self.ordinal + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            self.state.processing_lost();
        }
        self.state.changed.notify_waiters();
    }
}

#[derive(Clone, Debug)]
pub struct EventSender {
    inner: mpsc::Sender<SdkEvent>,
    projection: Option<Arc<ProjectionState>>,
    metadata: EventMetadata,
    current_scope: Option<Arc<NativeRequestContext>>,
    constructor_scope: Option<Arc<crate::acp::establishment::NativeConstructorObservation>>,
}

impl From<mpsc::Sender<SdkEvent>> for EventSender {
    fn from(inner: mpsc::Sender<SdkEvent>) -> Self {
        Self {
            inner,
            projection: None,
            metadata: EventMetadata::history(),
            current_scope: None,
            constructor_scope: None,
        }
    }
}

impl EventSender {
    pub(crate) fn tracked(
        inner: mpsc::Sender<SdkEvent>,
        projection: Arc<ProjectionState>,
        owner: Option<crate::acp::runner_lifecycle::RunnerIdentity>,
    ) -> Self {
        let mut metadata = EventMetadata::history();
        if let Some(origin) = projection
            .attempt()
            .and_then(|attempt| NativeEventOrigin::owned_attempt(&attempt, owner))
        {
            metadata.native_origin = Some(origin);
        }
        Self {
            inner,
            projection: Some(projection),
            metadata,
            current_scope: None,
            constructor_scope: None,
        }
    }

    pub(crate) fn projection(&self) -> Option<&Arc<ProjectionState>> {
        self.projection.as_ref()
    }

    pub(crate) fn with_context(&self, context: Option<Arc<NativeRequestContext>>) -> Self {
        let Some(context) = context else {
            return self.with_metadata(EventMetadata::history());
        };
        let mut sender = self.clone();
        let origin = context.origin();
        let scope = if origin.is_none() {
            EventScope::HistoryOnly
        } else {
            match &context.scope {
                NativeRequestScope::Observation if context.has_current_callback() => {
                    EventScope::SessionCurrent
                }
                NativeRequestScope::Session { .. } => EventScope::SessionCurrent,
                NativeRequestScope::Observation => EventScope::HistoryOnly,
                _ => EventScope::OperationCurrent,
            }
        };
        sender.metadata = EventMetadata {
            scope,
            native_origin: origin,
            native_prompt_id: None,
            reported_session_id: None,
            operation: context
                .correlation()
                .map(|correlation| AcpOperationMetadata {
                    correlation,
                    outcome: None,
                }),
            prompt_observation: None,
            output_uncertainty: None,
            clear_after_seq: None,
            resource_ref: None,
            resource_witness: None,
            availability_denial: false,
            host_action: false,
        };
        sender.current_scope = Some(context);
        sender.constructor_scope = None;
        sender
    }

    pub(crate) fn with_clear_after_seq(&self, seq: u64) -> Self {
        let mut sender = self.clone();
        sender.metadata.clear_after_seq = Some(seq);
        sender
    }

    pub(crate) fn with_resource_witness(
        &self,
        witness: Arc<crate::acp::establishment::NativeResourceWitness>,
    ) -> Self {
        let mut sender = self.with_resource_reference(&witness);
        sender.metadata.resource_witness = Some(witness);
        sender
    }

    pub(crate) fn with_resource_reference(
        &self,
        witness: &crate::acp::establishment::NativeResourceWitness,
    ) -> Self {
        let mut metadata = EventMetadata::history();
        metadata.native_origin = Some(witness.origin);
        metadata.resource_ref = Some(witness.allocation);
        self.with_metadata(metadata)
    }

    pub(crate) fn with_constructor(
        &self,
        context: Arc<crate::acp::establishment::NativeConstructorObservation>,
    ) -> Self {
        let mut sender = self.with_metadata(EventMetadata::history());
        sender.metadata.scope = EventScope::SessionCurrent;
        sender.metadata.native_origin = Some(context.origin());
        sender.constructor_scope = Some(context);
        sender
    }

    pub(crate) fn with_metadata(&self, metadata: EventMetadata) -> Self {
        let mut sender = self.clone();
        sender.metadata = metadata;
        sender.current_scope = None;
        sender.constructor_scope = None;
        sender
    }

    pub(crate) fn with_scoped_metadata(
        &self,
        metadata: EventMetadata,
        context: Option<Arc<NativeRequestContext>>,
    ) -> Self {
        let mut sender = self.clone();
        sender.metadata = metadata;
        sender.current_scope = context;
        sender.constructor_scope = None;
        sender
    }

    pub(crate) fn set_native_completion(
        &mut self,
        evidence: &super::control_protocol::NativePromptCompletionEvidence,
    ) {
        self.metadata.native_prompt_id = Some(evidence.prompt_req_id);
        self.metadata.prompt_observation = Some(AcpPromptObservation::Written {
            window_id: evidence.window_id,
        });
        self.metadata.output_uncertainty = evidence.uncertain_output.then_some(evidence.window_id);
    }

    pub(crate) fn historical(&self) -> Self {
        self.with_metadata(self.metadata.clone().historical())
    }

    pub(crate) fn with_outcome(&self, outcome: AcpOriginalRequestResult) -> Self {
        let mut sender = self.clone();
        if let Some(operation) = sender.metadata.operation.as_mut() {
            operation.outcome = Some(outcome);
        }
        sender
    }

    pub async fn send(&self, event: Event) -> Result<(), mpsc::error::SendError<()>> {
        let permit = self.reserve().await?;
        let current = self
            .current_scope
            .as_deref()
            .map(NativeRequestContext::try_effect_guard)
            .transpose();
        let constructor = self
            .constructor_scope
            .as_deref()
            .map(|context| context.try_publication_guard())
            .transpose();
        let stale = current
            .as_ref()
            .is_err_and(|error| !error.is::<crate::process::worker_registry::RegistryBusy>())
            || constructor
                .as_ref()
                .is_err_and(|error| !error.is::<crate::process::worker_registry::RegistryBusy>());
        let permit = if stale { permit.historical() } else { permit };
        permit.send(event);
        Ok(())
    }

    pub async fn reserve(&self) -> Result<EventPermit<'_>, mpsc::error::SendError<()>> {
        let permit = self.inner.reserve().await.inspect_err(|_| {
            if let Some(state) = &self.projection {
                state.lost()
            }
        })?;
        Ok(EventPermit {
            permit,
            projection: self.projection.as_deref(),
            metadata: self.metadata.clone(),
            current_scope: self.current_scope.as_ref(),
            constructor_scope: self.constructor_scope.as_ref(),
        })
    }

    pub async fn reserve_many(
        &self,
        count: usize,
    ) -> Result<impl Iterator<Item = EventPermit<'_>>, mpsc::error::SendError<()>> {
        let permits = self.inner.reserve_many(count).await.inspect_err(|_| {
            if let Some(state) = &self.projection {
                state.lost()
            }
        })?;
        Ok(permits.map(|permit| EventPermit {
            permit,
            projection: self.projection.as_deref(),
            metadata: self.metadata.clone(),
            current_scope: self.current_scope.as_ref(),
            constructor_scope: self.constructor_scope.as_ref(),
        }))
    }

    pub fn is_closed(&self) -> bool {
        self.inner.is_closed()
    }
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }
    pub async fn closed(&self) {
        self.inner.closed().await
    }

    pub fn downgrade(&self) -> WeakEventSender {
        WeakEventSender {
            inner: self.inner.downgrade(),
            projection: self.projection.as_ref().map(Arc::downgrade),
            metadata: self.metadata.clone(),
            current_scope: self.current_scope.clone(),
            constructor_scope: self.constructor_scope.clone(),
        }
    }

    /// The permit and short emission mutex linearize the closure behind every
    /// required projected value; neither is held across another await.
    pub(crate) async fn send_control_plane_loss(
        &self,
        connection: Arc<crate::acp::acp_client::ConnectionState>,
        observation: crate::acp::state::AcpConnectionUnavailability,
    ) -> Result<(), mpsc::error::SendError<()>> {
        let origin = connection
            .observed_native_origin()
            .or(self.metadata.native_origin);
        let permit = self.inner.reserve().await?;
        permit.send(SdkEvent::ControlPlaneLoss(ControlPlaneLoss {
            connection,
            observation,
            origin,
        }));
        Ok(())
    }

    pub(crate) async fn close_establishment(
        &self,
        failure: Option<EstablishmentBlockReason>,
    ) -> Option<Arc<EstablishmentDelivery>> {
        let state = self.projection.as_ref()?;
        let permit = self.inner.reserve().await.ok();
        let mut emissions = state.emissions.lock().expect("SDK projection lock");
        let through = emissions.next;
        let window = emissions.window.as_mut()?;
        if let Some(delivery) = &window.delivery {
            return Some(Arc::clone(delivery));
        }
        window.lost |= permit.is_none();
        let (resolution, _) = watch::channel(EstablishmentResolution::Pending);
        let delivery = Arc::new(EstablishmentDelivery {
            attempt: Arc::clone(&window.attempt),
            receipt: window.receipt.clone(),
            failure,
            first: window.first,
            through,
            original: window.original,
            complete: window.wire_closed && !window.lost,
            resolution,
        });
        window.delivery = Some(Arc::clone(&delivery));
        if let Some(permit) = permit {
            permit.send(SdkEvent::EstablishmentClosed {
                delivery: Arc::clone(&delivery),
            });
        } else {
            delivery.resolve(EstablishmentResolution::Unconfirmed);
        }
        state.changed.notify_waiters();
        Some(delivery)
    }
}

pub struct EventPermit<'a> {
    permit: mpsc::Permit<'a, SdkEvent>,
    projection: Option<&'a ProjectionState>,
    metadata: EventMetadata,
    current_scope: Option<&'a Arc<NativeRequestContext>>,
    constructor_scope: Option<&'a Arc<crate::acp::establishment::NativeConstructorObservation>>,
}

impl EventPermit<'_> {
    pub(crate) fn historical(mut self) -> Self {
        self.metadata = self.metadata.historical();
        self.current_scope = None;
        self.constructor_scope = None;
        self
    }

    pub fn send(self, event: Event) {
        self.send_data(event, None);
    }

    pub(crate) fn send_logical_admission(self, journal_publication: oneshot::Sender<bool>) {
        self.send_data(Event::ThinkingStarted, Some(journal_publication));
    }

    fn send_data(self, mut event: Event, journal_publication: Option<oneshot::Sender<bool>>) {
        if event.native_subject().is_none() {
            if let Some(origin) = self.metadata.native_origin {
                event.set_native_subject(crate::acp::state::NativeEventSubject::channel(
                    origin.context,
                    origin.owner,
                    &event,
                ));
            }
        }
        let data = AcpEventData {
            event: Arc::new(event),
            metadata: self.metadata,
            current_scope: self.current_scope.cloned(),
            constructor_scope: self.constructor_scope.cloned(),
            journal_publication,
            pending_request: None,
        };
        Self::send_frame(self.permit, self.projection, data);
    }

    pub(crate) fn send_pending_request(
        self,
        publication: Arc<super::acp_client::pending::PendingRequestPublication>,
    ) {
        let data = AcpEventData {
            event: Arc::clone(publication.event()),
            metadata: publication.metadata(),
            current_scope: None,
            constructor_scope: None,
            journal_publication: None,
            pending_request: Some(publication),
        };
        Self::send_frame(self.permit, self.projection, data);
    }

    fn send_frame(
        permit: mpsc::Permit<'_, SdkEvent>,
        projection: Option<&ProjectionState>,
        data: AcpEventData,
    ) {
        let data = SdkEvent::Data(data);
        if let Some(state) = projection {
            let mut emissions = state.emissions.lock().expect("SDK projection lock");
            if let Some(next) = emissions.next.checked_add(1) {
                emissions.next = next;
            } else if let Some(window) = &mut emissions.window {
                window.lost = true;
            }
            permit.send(data);
        } else {
            permit.send(data);
        }
    }
}

#[derive(Clone, Debug)]
pub struct WeakEventSender {
    inner: mpsc::WeakSender<SdkEvent>,
    projection: Option<Weak<ProjectionState>>,
    metadata: EventMetadata,
    current_scope: Option<Arc<NativeRequestContext>>,
    constructor_scope: Option<Arc<crate::acp::establishment::NativeConstructorObservation>>,
}

impl From<mpsc::WeakSender<SdkEvent>> for WeakEventSender {
    fn from(inner: mpsc::WeakSender<SdkEvent>) -> Self {
        Self {
            inner,
            projection: None,
            metadata: EventMetadata::history(),
            current_scope: None,
            constructor_scope: None,
        }
    }
}

impl WeakEventSender {
    pub fn upgrade(&self) -> Option<EventSender> {
        let inner = self.inner.upgrade()?;
        let projection = match &self.projection {
            Some(state) => Some(state.upgrade()?),
            None => None,
        };
        Some(EventSender {
            inner,
            projection,
            metadata: self.metadata.clone(),
            current_scope: self.current_scope.clone(),
            constructor_scope: self.constructor_scope.clone(),
        })
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;
    use crate::acp::state::BackgroundAgentStatus;

    #[test]
    fn host_tracking_end_keeps_allocation_without_claiming_native_outcome() {
        let allocation = uuid::Uuid::new_v4();
        for event in [
            Event::BackgroundTaskFinished {
                task_id: "task".into(),
                ended_at: Some(chrono::Utc::now()),
            },
            Event::BackgroundAgentCompleted {
                agent_id: "agent".into(),
                status: BackgroundAgentStatus::Detached,
                tools: Vec::new(),
                result: None,
                warning: None,
                ended_at: chrono::Utc::now(),
            },
        ] {
            let mut metadata = EventMetadata::host(&event);
            metadata.resource_ref = Some(allocation);
            metadata.validate_for_event(&event).unwrap();
            assert_eq!(metadata.scope, EventScope::HistoryOnly);
            assert!(metadata.native_origin.is_none());
            assert!(metadata.resource_witness.is_none());
            let encoded = metadata.encode_private().unwrap().unwrap();
            assert_eq!(
                EventMetadata::decode_private(EventScope::HistoryOnly as i64, Some(&encoded))
                    .unwrap(),
                metadata
            );
            metadata.scope = EventScope::SessionCurrent;
            assert!(metadata.validate_for_event(&event).is_err());
        }
    }

    #[test]
    fn host_resource_reference_cannot_claim_native_success_or_prompt_authority() {
        let allocation = uuid::Uuid::new_v4();
        let prompt = Event::UserPromptSent {
            text: "input".into(),
            attachments: Vec::new(),
            prompt_id: None,
            synthesized: false,
            origin_plugin: None,
        };
        let mut metadata = EventMetadata::host(&prompt);
        metadata.resource_ref = Some(allocation);
        assert!(metadata.validate_for_event(&prompt).is_err());
        for status in [
            BackgroundAgentStatus::Completed,
            BackgroundAgentStatus::Error,
        ] {
            let event = Event::BackgroundAgentCompleted {
                agent_id: "agent".into(),
                status,
                tools: Vec::new(),
                result: None,
                warning: None,
                ended_at: chrono::Utc::now(),
            };
            assert!(metadata.validate_for_event(&event).is_err());
        }
        let fabricated_result = Event::BackgroundAgentCompleted {
            agent_id: "agent".into(),
            status: BackgroundAgentStatus::Detached,
            tools: Vec::new(),
            result: Some("claimed Native output".into()),
            warning: None,
            ended_at: chrono::Utc::now(),
        };
        assert!(metadata.validate_for_event(&fabricated_result).is_err());
    }
}
