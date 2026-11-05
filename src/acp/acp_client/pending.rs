//! In-flight approval and elicitation responders, keyed by nonce.

use crate::acp::approvals::{ApprovalDecision, Nonce};
use crate::acp::elicitations::{
    build_response, summarize_answers, Elicitation, ElicitationAnswer, ElicitationOutcome,
    ElicitationResolution,
};
use agent_client_protocol::schema::v1::CreateElicitationResponse;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};
use tokio::sync::{oneshot, Mutex};

/// Resolution channel for a parked agent->client request awaiting a user
/// decision. Stored in the pending-responders map keyed by the structured
/// view's server-generated nonce. One map carries both permission
/// approvals and form elicitations; nonces are unique across both, and
/// the resolver variant records which kind of request is parked.
pub(crate) struct PendingResponder {
    pub(super) resolver: PendingResolver,
    pub(crate) publication: Option<Arc<PendingRequestPublication>>,
}

impl PendingResponder {
    pub(crate) fn is_live(&self) -> bool {
        match &self.resolver {
            PendingResolver::Approval(resolver) => !resolver.is_closed(),
            PendingResolver::Elicitation { resolver, .. } => !resolver.is_closed(),
        }
    }
}

pub(super) enum PendingResolver {
    /// `session/request_permission` awaiting allow/deny.
    Approval(oneshot::Sender<ApprovalResolutionMessage>),
    /// `elicitation/create` awaiting an accept/decline/cancel answer. The
    /// parsed form is kept so `resolve_elicitation` can validate the
    /// submitted answer BEFORE consuming the resolver: a validation
    /// failure then leaves the elicitation pending for a corrected
    /// resubmission instead of permanently cancelling it. The validated
    /// response (and its outcome) ride the oneshot so the parked callback
    /// just forwards them. Boxed to keep the enum small.
    Elicitation {
        elicitation: Box<Elicitation>,
        resolver: oneshot::Sender<ElicitationResolutionMessage>,
    },
}

/// Message sent over the resolver oneshot to unblock the parked
/// `on_receive_request` callback.
pub(crate) enum ApprovalResolutionMessage {
    Decision {
        decision: ApprovalDecision,
        /// An `option_id` the client picked off the agent's own labels,
        /// rather than the Allow / Always / Deny trio.
        option_id: Option<String>,
    },
    Cancelled,
}

/// Message sent over the elicitation resolver oneshot. Carries the
/// validated wire response for the agent, the outcome for status
/// derivation, and the display-ready answers for the transcript
/// (`Event::ElicitationResolved.answers`). See #2209.
pub(super) struct ElicitationResolutionMessage {
    pub(super) response: CreateElicitationResponse,
    pub(super) outcome: ElicitationOutcome,
    pub(super) answers: Vec<ElicitationAnswer>,
}

pub(crate) type PendingResponders = Arc<Mutex<HashMap<Nonce, PendingResponder>>>;
pub(crate) type WeakPendingResponders = Weak<Mutex<HashMap<Nonce, PendingResponder>>>;

/// One real callback's display payload and immutable decision custody.
/// Only the actual journal writer may give this request a publication seq.
#[derive(Debug)]
pub(crate) struct PendingRequestPublication {
    event: Arc<crate::acp::state::Event>,
    callback: Arc<super::session_identity::CapturedCallback>,
    connection: Weak<super::connection::ConnectionState>,
    responders: WeakPendingResponders,
    seq: AtomicU64,
}

impl PendingRequestPublication {
    pub(super) fn new(
        event: Arc<crate::acp::state::Event>,
        callback: Arc<super::session_identity::CapturedCallback>,
        connection: Weak<super::connection::ConnectionState>,
        responders: &PendingResponders,
    ) -> Self {
        Self {
            event,
            callback,
            connection,
            responders: Arc::downgrade(responders),
            seq: AtomicU64::new(0),
        }
    }

    pub(crate) fn event(&self) -> &Arc<crate::acp::state::Event> {
        &self.event
    }

    pub(crate) fn is_original(&self) -> bool {
        self.callback
            .context
            .as_ref()
            .is_some_and(|context| context.is_original_decision())
    }

    pub(crate) fn metadata(&self) -> crate::acp::event_sender::EventMetadata {
        let mut metadata = crate::acp::event_sender::EventMetadata::history();
        if let Some(context) = &self.callback.context {
            metadata.native_origin = context.origin();
            if !self.is_original() {
                metadata.scope = crate::acp::event_sender::EventScope::SessionCurrent;
            }
        }
        metadata
    }

    pub(crate) fn try_publication_guard(
        &self,
    ) -> anyhow::Result<(
        crate::session::StorageFlock,
        crate::process::worker_registry::RegistryGuard<'_>,
    )> {
        self.guard(true)
    }

    pub(crate) fn publication_guard(
        &self,
    ) -> anyhow::Result<(
        crate::session::StorageFlock,
        crate::process::worker_registry::RegistryGuard<'_>,
    )> {
        self.guard(false)
    }

    pub(super) async fn wait_for_publication_guard(self: &Arc<Self>) -> anyhow::Result<()> {
        let publication = Arc::clone(self);
        tokio::task::spawn_blocking(move || loop {
            match publication.publication_guard() {
                Ok(guard) => {
                    drop(guard);
                    return Ok(());
                }
                Err(error) if error.is::<crate::process::worker_registry::RegistryBusy>() => {
                    let id = publication
                        .callback
                        .original_session_id()
                        .ok_or(super::AcpError::UnknownNonce)?;
                    // Failed guards release the native lock before this wait.
                    drop(crate::process::worker_registry::acquire_registry_lock(id)?);
                }
                Err(error) => return Err(error),
            }
        })
        .await?
    }

    pub(super) async fn lock_responders_for_publication<'publication, 'responders>(
        self: &'publication Arc<Self>,
        pending: &'responders PendingResponders,
    ) -> anyhow::Result<(
        tokio::sync::MutexGuard<'responders, HashMap<Nonce, PendingResponder>>,
        crate::session::StorageFlock,
        crate::process::worker_registry::RegistryGuard<'publication>,
    )> {
        loop {
            let responders = pending.lock().await;
            match self.try_publication_guard() {
                Ok((native, registry)) => return Ok((responders, native, registry)),
                Err(error) if error.is::<crate::process::worker_registry::RegistryBusy>() => {
                    drop(responders);
                    self.wait_for_publication_guard().await?;
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn guard(
        &self,
        nonblocking: bool,
    ) -> anyhow::Result<(
        crate::session::StorageFlock,
        crate::process::worker_registry::RegistryGuard<'_>,
    )> {
        anyhow::ensure!(self.callback.is_live(), super::AcpError::UnknownNonce);
        let connection = self
            .connection
            .upgrade()
            .ok_or(super::AcpError::AgentExited)?;
        connection.admit(|| ())?;
        let context = self
            .callback
            .context
            .as_ref()
            .ok_or(super::AcpError::UnknownNonce)?;
        let guard = if nonblocking {
            context.try_decision_guard()
        } else {
            context.decision_guard(false)
        }?;
        anyhow::ensure!(self.callback.is_live(), super::AcpError::UnknownNonce);
        connection.admit(|| ())?;
        Ok(guard)
    }

    pub(crate) fn mark_published(&self, seq: u64) {
        if seq != 0 {
            let _ = self
                .seq
                .compare_exchange(0, seq, Ordering::AcqRel, Ordering::Acquire);
        }
    }

    pub(crate) fn published_at(&self, frontier: u64) -> bool {
        let seq = self.seq.load(Ordering::Acquire);
        seq != 0 && seq <= frontier
    }

    /// Release only this callback when its request never reached the journal.
    /// A replacement or already-published responder keeps its decision custody.
    pub(crate) async fn abandon_unpublished(self: &Arc<Self>) {
        let nonce = match self.event.as_ref() {
            crate::acp::state::Event::ApprovalRequested { approval } => &approval.nonce,
            crate::acp::state::Event::ElicitationRequested { elicitation } => &elicitation.nonce,
            _ => return,
        };
        let Some(responders) = self.responders.upgrade() else {
            return;
        };
        let removed = {
            let mut responders = responders.lock().await;
            if self.published_at(u64::MAX)
                || !responders.get(nonce).is_some_and(|responder| {
                    responder
                        .publication
                        .as_ref()
                        .is_some_and(|publication| Arc::ptr_eq(publication, self))
                })
            {
                return;
            }
            responders.remove(nonce)
        };
        // Wake the real callback only after releasing the map lock.
        drop(removed);
    }

    pub(crate) fn responders(&self) -> WeakPendingResponders {
        self.responders.clone()
    }
}

fn decision_refusal(error: anyhow::Error) -> super::AcpError {
    if error.is::<crate::process::worker_registry::RegistryBusy>() {
        super::AcpError::RegistryBusy
    } else {
        super::AcpError::UnknownNonce
    }
}

pub(crate) async fn resolve_approval(
    pending: &PendingResponders,
    nonce: &Nonce,
    message: ApprovalResolutionMessage,
) -> Result<(), super::AcpError> {
    let mut map = pending.lock().await;
    let responder = map.get(nonce).ok_or(super::AcpError::UnknownNonce)?;
    if !responder.is_live() {
        return Err(super::AcpError::UnknownNonce);
    }
    if !matches!(responder.resolver, PendingResolver::Approval(_)) {
        return Err(super::AcpError::UnknownNonce);
    }
    let publication = responder.publication.clone();
    if publication
        .as_ref()
        .is_some_and(|publication| !publication.published_at(u64::MAX))
    {
        return Err(super::AcpError::UnknownNonce);
    }
    let _guard = match publication
        .as_ref()
        .map(|publication| publication.try_publication_guard())
        .transpose()
    {
        Ok(guard) => guard,
        Err(error) if error.is::<crate::process::worker_registry::RegistryBusy>() => {
            drop(map);
            let publication = publication
                .as_ref()
                .expect("only a publication takes guards");
            let (responders, native, registry) = publication
                .lock_responders_for_publication(pending)
                .await
                .map_err(decision_refusal)?;
            map = responders;
            if !map.get(nonce).is_some_and(|responder| {
                responder.is_live()
                    && matches!(responder.resolver, PendingResolver::Approval(_))
                    && responder
                        .publication
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, publication))
            }) {
                return Err(super::AcpError::UnknownNonce);
            }
            Some((native, registry))
        }
        Err(error) => return Err(decision_refusal(error)),
    };
    let PendingResolver::Approval(resolver) = map.remove(nonce).unwrap().resolver else {
        unreachable!("checked above");
    };
    resolver
        .send(message)
        .map_err(|_| super::AcpError::AgentExited)
}

pub(crate) async fn resolve_elicitation(
    pending: &PendingResponders,
    nonce: &Nonce,
    resolution: ElicitationResolution,
) -> Result<(), super::AcpError> {
    let mut map = pending.lock().await;
    let responder = map.get(nonce).ok_or(super::AcpError::UnknownNonce)?;
    if !responder.is_live() {
        return Err(super::AcpError::UnknownNonce);
    }
    let PendingResolver::Elicitation { elicitation, .. } = &responder.resolver else {
        return Err(super::AcpError::UnknownNonce);
    };
    let outcome = resolution.outcome();
    let answers = match &resolution {
        ElicitationResolution::Accept { answers } => summarize_answers(elicitation, answers),
        ElicitationResolution::Decline | ElicitationResolution::Cancel => Vec::new(),
    };
    let response = build_response(elicitation, resolution)
        .map_err(|error| super::AcpError::InvalidAnswer(error.to_string()))?;
    let publication = responder.publication.clone();
    if publication
        .as_ref()
        .is_some_and(|publication| !publication.published_at(u64::MAX))
    {
        return Err(super::AcpError::UnknownNonce);
    }
    let _guard = match publication
        .as_ref()
        .map(|publication| publication.try_publication_guard())
        .transpose()
    {
        Ok(guard) => guard,
        Err(error) if error.is::<crate::process::worker_registry::RegistryBusy>() => {
            drop(map);
            let publication = publication
                .as_ref()
                .expect("only a publication takes guards");
            let (responders, native, registry) = publication
                .lock_responders_for_publication(pending)
                .await
                .map_err(decision_refusal)?;
            map = responders;
            if !map.get(nonce).is_some_and(|responder| {
                responder.is_live()
                    && matches!(responder.resolver, PendingResolver::Elicitation { .. })
                    && responder
                        .publication
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, publication))
            }) {
                return Err(super::AcpError::UnknownNonce);
            }
            Some((native, registry))
        }
        Err(error) => return Err(decision_refusal(error)),
    };
    let PendingResolver::Elicitation { resolver, .. } = map.remove(nonce).unwrap().resolver else {
        unreachable!("checked above");
    };
    resolver
        .send(ElicitationResolutionMessage {
            response,
            outcome,
            answers,
        })
        .map_err(|_| super::AcpError::AgentExited)
}
