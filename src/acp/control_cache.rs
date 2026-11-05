//! Live per-session control state, folded once at the publish choke point.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::event_store::control::ControlCursor;
use super::state::{AcpState, Event, NativeControlFailure, NativeEventSubject, NativeSubjectEvent};
use crate::acp::event_sender::EventMetadata;
use crate::session::Status;
use anyhow::Result;

/// A session's folded control state and how far it has been folded.
#[derive(Debug, Clone)]
struct Cached {
    state: AcpState,
    /// Highest seq folded in.
    last_seq: u64,
}

/// One slot for the historical fold and bounded, non-durable native facts.
#[derive(Debug, Default)]
struct SessionSlot {
    cached: Option<Cached>,
    control: Option<ControlCursor>,
    retired: bool,
    native_scope: Option<crate::acp::state::NativeObservationScope>,
    failed_availability: Option<(u64, NativeEventSubject)>,
    failed_control: Option<(u64, NativeEventSubject)>,
    original_responders: Option<crate::acp::acp_client::pending::WeakPendingResponders>,
}

type Slot = Arc<Mutex<SessionSlot>>;

#[derive(Debug, Default)]
pub struct ControlStateCache {
    sessions: Mutex<HashMap<String, Slot>>,
}

#[derive(Default)]
pub(crate) struct OriginalDecisions {
    pub(crate) nonces: Vec<crate::acp::approvals::Nonce>,
    pub(crate) approvals: Vec<crate::acp::approvals::Approval>,
    pub(crate) elicitations: Vec<crate::acp::elicitations::Elicitation>,
}

impl OriginalDecisions {
    pub(crate) fn apply(self, state: &mut AcpState) {
        state.original_decision_nonces = self.nonces;
        for approval in self.approvals {
            if !state
                .pending_approvals
                .iter()
                .any(|pending| pending.nonce == approval.nonce)
            {
                state.pending_approvals.push(approval);
            }
        }
        for elicitation in self.elicitations {
            if !state
                .pending_elicitations
                .iter()
                .any(|pending| pending.nonce == elicitation.nonce)
            {
                state.pending_elicitations.push(elicitation);
            }
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

fn lock_slot(slot: &Mutex<SessionSlot>) -> std::sync::MutexGuard<'_, SessionSlot> {
    match slot.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            let mut guard = poisoned.into_inner();
            guard.control = None;
            guard.cached = None;
            slot.clear_poison();
            guard
        }
    }
}

impl ControlStateCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The slot for `session_id`, creating an empty one if absent.
    fn slot(&self, session_id: &str) -> Slot {
        let mut map = lock(&self.sessions);
        match map.get(session_id) {
            Some(slot) => Arc::clone(slot),
            None => {
                let slot = Arc::new(Mutex::new(SessionSlot::default()));
                map.insert(session_id.to_owned(), Arc::clone(&slot));
                slot
            }
        }
    }

    /// Bind only from a checked, committed publication, never historical replay.
    pub(crate) fn bind_original_responders(
        &self,
        session_id: &str,
        responders: crate::acp::acp_client::pending::WeakPendingResponders,
    ) {
        let slot = self.slot(session_id);
        let mut guard = lock_slot(&slot);
        if !guard.retired {
            guard.original_responders = Some(responders);
        }
    }

    pub(crate) fn original_responders(
        &self,
        session_id: &str,
    ) -> Option<crate::acp::acp_client::pending::PendingResponders> {
        let slot = self.slot(session_id);
        let guard = lock_slot(&slot);
        if guard.retired {
            return None;
        }
        guard.original_responders.as_ref()?.upgrade()
    }

    pub(crate) async fn original_decisions(
        &self,
        session_id: &str,
        frontier: u64,
    ) -> Result<OriginalDecisions> {
        let mut decisions = self
            .project_original_requests(
                session_id,
                frontier,
                None,
                |event, decisions: &mut OriginalDecisions| match event {
                    Event::ApprovalRequested { approval } => {
                        decisions.nonces.push(approval.nonce.clone());
                        decisions.approvals.push(approval.clone());
                    }
                    Event::ElicitationRequested { elicitation } => {
                        decisions.nonces.push(elicitation.nonce.clone());
                        decisions.elicitations.push(elicitation.clone());
                    }
                    _ => unreachable!("checked the published request kind"),
                },
            )
            .await?;
        decisions.nonces.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(decisions)
    }

    pub(crate) async fn is_live_original_request(
        &self,
        session_id: &str,
        frontier: u64,
        event: &Arc<Event>,
    ) -> Result<bool> {
        self.project_original_requests(session_id, frontier, Some(event), |_, live| *live = true)
            .await
    }

    async fn project_original_requests<T: Default + Send + 'static>(
        &self,
        session_id: &str,
        frontier: u64,
        event: Option<&Arc<Event>>,
        project: impl Fn(&Event, &mut T) + Send + 'static,
    ) -> Result<T> {
        let Some(responders) = self.original_responders(session_id) else {
            return Ok(T::default());
        };
        let slot = self.slot(session_id);
        let publications: Vec<_> = {
            let responders = responders.lock().await;
            responders
                .values()
                .filter(|responder| responder.is_live())
                .filter_map(|responder| responder.publication.as_ref())
                .filter(|publication| {
                    publication.is_original() && publication.published_at(frontier)
                })
                .filter(|publication| {
                    event.is_none_or(|event| Arc::ptr_eq(publication.event(), event))
                })
                .map(Arc::clone)
                .collect()
        };
        if publications.is_empty() {
            return Ok(T::default());
        }
        let session_id = session_id.to_owned();
        tokio::task::spawn_blocking(move || {
            let mut output = T::default();
            for publication in publications {
                let _guard = loop {
                    match publication.publication_guard() {
                        Ok(guard) => break Some(guard),
                        Err(error)
                            if error.is::<crate::process::worker_registry::RegistryBusy>() =>
                        {
                            drop(crate::process::worker_registry::acquire_registry_lock(
                                &session_id,
                            )?);
                        }
                        Err(_) => break None,
                    }
                };
                if _guard.is_none() {
                    continue;
                }
                let nonce = match publication.event().as_ref() {
                    Event::ApprovalRequested { approval } => &approval.nonce,
                    Event::ElicitationRequested { elicitation } => &elicitation.nonce,
                    _ => continue,
                };
                let map = responders.blocking_lock();
                let still_bound = {
                    let slot = lock_slot(&slot);
                    !slot.retired
                        && slot
                            .original_responders
                            .as_ref()
                            .and_then(|current| current.upgrade())
                            .is_some_and(|current| Arc::ptr_eq(&current, &responders))
                };
                if !still_bound
                    || !map.get(nonce).is_some_and(|responder| {
                        responder.is_live()
                            && responder.publication.as_ref().is_some_and(|current| {
                                Arc::ptr_eq(current, &publication) && current.published_at(frontier)
                            })
                    })
                {
                    continue;
                }
                project(publication.event().as_ref(), &mut output);
            }
            Ok(output)
        })
        .await?
    }

    pub(crate) fn with_control<T>(
        &self,
        session_id: &str,
        operation: impl FnOnce(&mut Option<ControlCursor>) -> T,
    ) -> T {
        loop {
            let slot = self.slot(session_id);
            let mut guard = lock_slot(&slot);
            if guard.retired {
                continue;
            }
            return operation(&mut guard.control);
        }
    }

    /// Hold one observed frontier through mutation; contention stays unknown.
    pub(crate) fn with_frontier<T>(
        &self,
        session_id: &str,
        frontier: u64,
        operation: impl FnOnce() -> T,
    ) -> Option<T> {
        let slot = self.sessions.try_lock().ok()?.get(session_id).cloned()?;
        let guard = slot.try_lock().ok()?;
        if guard.retired
            || guard.cached.as_ref()?.last_seq != frontier
            || guard
                .failed_availability
                .as_ref()
                .is_some_and(|(seq, _)| *seq > frontier)
            || guard
                .failed_control
                .as_ref()
                .is_some_and(|(seq, _)| *seq > frontier)
        {
            return None;
        }
        Some(operation())
    }

    /// Fold `event` into the session's cached state, if it has one. Returns the
    /// sidebar status right after this event, so the caller can carry it with
    /// the frame instead of a later reader finding the fold already ahead.
    pub(crate) fn apply_if_cached(
        &self,
        session_id: &str,
        seq: u64,
        event: &Event,
        metadata: &EventMetadata,
    ) -> Option<Status> {
        let slot = self.slot(session_id);
        let mut guard = lock_slot(&slot);
        // A profile/incarnation scope alone is not a connection verdict. Keep
        // each historical sidebar edge unless actual native facts need projection.
        let conditional_native = guard.failed_availability.is_some()
            || guard.failed_control.is_some()
            || guard.cached.as_ref().is_some_and(|cached| {
                cached.state.native_availability.is_some()
                    || cached.state.connection_unavailable.is_some()
            });
        let cached = guard.cached.as_mut()?;
        if seq == cached.last_seq {
            // Exact repeat of the seq we already folded: a benign publish
            // retry (the store reports these as primary-key collisions).
            return None;
        }
        if seq != cached.last_seq + 1
            || cached
                .state
                .apply_scoped_event(seq, event, metadata)
                .is_err()
        {
            guard.cached = None;
            return None;
        }
        cached.last_seq = seq;
        (!conditional_native).then(|| cached.state.sidebar_status())
    }

    /// The cached fold's sidebar status, or `None` if nothing is cached. Never
    /// hydrates; callers that need a verdict hydrate through
    /// `SessionService::fold_control_state` first.
    pub fn sidebar_status(&self, session_id: &str) -> Option<crate::session::Status> {
        let slot = self.slot(session_id);
        let guard = lock_slot(&slot);
        if guard.failed_availability.is_some()
            || guard.failed_control.is_some()
            || guard.cached.as_ref().is_some_and(|cached| {
                cached.state.native_availability.is_some()
                    || cached.state.connection_unavailable.is_some()
            })
        {
            return None;
        }
        guard.cached.as_ref().map(|c| c.state.sidebar_status())
    }

    /// Discard the session's cached projections.
    pub fn forget(&self, session_id: &str) {
        let slot = lock(&self.sessions).get(session_id).cloned();
        if let Some(slot) = slot {
            let mut guard = lock_slot(&slot);
            *guard = SessionSlot {
                retired: true,
                ..SessionSlot::default()
            };
            let mut map = lock(&self.sessions);
            if map
                .get(session_id)
                .is_some_and(|current| Arc::ptr_eq(current, &slot))
            {
                map.remove(session_id);
            }
        }
    }

    /// An append failure is not durable history. Retain only its original,
    /// bounded semantics, scoped just like a live native observation.
    pub(crate) fn record_failed_observation(
        &self,
        session_id: &str,
        seq: u64,
        subject: Option<NativeEventSubject>,
    ) {
        let slot = self.slot(session_id);
        let mut guard = lock_slot(&slot);
        if guard
            .cached
            .as_ref()
            .is_some_and(|cached| cached.last_seq >= seq)
        {
            return;
        }
        guard.cached = None;
        let Some(subject) = subject else { return };
        let target = match subject.kind {
            NativeSubjectEvent::ChannelReady
            | NativeSubjectEvent::ChannelUnavailable(_)
            | NativeSubjectEvent::AdmissionRefusal(_) => &mut guard.failed_availability,
            NativeSubjectEvent::PromptDiagnostic
            | NativeSubjectEvent::StartupDiagnostic
            | NativeSubjectEvent::TurnEnded => &mut guard.failed_control,
            NativeSubjectEvent::BindingObserved | NativeSubjectEvent::RuntimeObservation => return,
        };
        if target.is_none_or(|(previous, _)| previous < seq) {
            *target = Some((seq, subject));
        }
    }

    /// The session's control state, running `hydrate` on a miss.
    pub(crate) fn get_or_hydrate(
        &self,
        session_id: &str,
        hydrate: impl FnOnce() -> Result<(AcpState, u64)>,
    ) -> Result<AcpState> {
        let slot = self.slot(session_id);
        let mut guard = lock_slot(&slot);
        if let Some(cached) = guard.cached.as_ref() {
            return Ok(cached.state.clone());
        }
        let (state, last_seq) = hydrate()?;
        guard.cached = Some(Cached {
            state: state.clone(),
            last_seq,
        });
        Ok(state)
    }

    /// Bounded qualifiers only: preflight does not clone warm control payloads.
    pub(crate) fn native_metadata(
        &self,
        session_id: &str,
    ) -> Option<(Option<(u64, NativeEventSubject)>, u64)> {
        let slot = self.slot(session_id);
        let guard = lock_slot(&slot);
        guard.cached.as_ref().map(|cached| {
            (
                cached.state.native_availability,
                cached.state.native_control_seq,
            )
        })
    }

    pub(crate) fn native_projection(
        &self,
        session_id: &str,
        mut historical: Option<(u64, NativeEventSubject)>,
        mut control_seq: u64,
        identity: Option<(String, chrono::DateTime<chrono::Utc>, u64)>,
        current: Option<crate::acp::state::CurrentNativeAvailability>,
    ) -> Result<
        crate::acp::state::NativeAvailabilityProjection,
        crate::acp::state::NativeObservationWait,
    > {
        let slot = self.slot(session_id);
        let scope = identity.and_then(|(profile, created_at, generation)| {
            let storage = crate::session::Storage::open_reference(&profile).ok()?;
            let context = crate::acp::state::NativeEventContext::capture_identity(
                session_id, created_at, generation, &storage,
            )
            .ok()?;
            let mut guard = lock_slot(&slot);
            if let Some(scope) = guard
                .native_scope
                .as_ref()
                .filter(|scope| scope.context == context)
            {
                return Some(scope.clone());
            }
            let scope = crate::acp::state::NativeObservationScope {
                context,
                choice: None,
                fence: crate::session::NativeInputFence::capture_route(
                    storage.profile(),
                    session_id,
                    created_at,
                    generation,
                    false,
                    Some(storage.bound_profile_identity().ok()?),
                )
                .ok()?,
            };
            guard.native_scope = Some(scope.clone());
            Some(scope)
        });
        let Some(scope) = scope else {
            return Ok(crate::acp::state::NativeAvailabilityProjection::default());
        };
        let scope = current
            .as_ref()
            .filter(|current| current.scope.context == scope.context)
            .map_or(scope, |current| current.scope.clone());
        let (_native_guard, selected) = match scope.fence.lock_observation_project(
            |row, _| {
                Ok(scope
                    .choice
                    .as_ref()
                    .is_some_and(|choice| choice.matches(row)))
            },
            true,
        ) {
            Ok(Some(observation)) => observation,
            Ok(None) => return Err(crate::acp::state::NativeObservationWait::native(&scope)),
            Err(_) => return Ok(crate::acp::state::NativeAvailabilityProjection::default()),
        };
        let context = Some(scope.context);
        let current = match current {
            Some(mut current) if Some(current.scope.context) == context => {
                let valid =
                    current.refresh(session_id, selected)? && current.qualify_owner(session_id)?;
                valid.then_some(current)
            }
            _ => None,
        };
        let (cached_availability, cached_control_seq, failed_availability, failed_control) = {
            let guard = lock_slot(&slot);
            (
                guard
                    .cached
                    .as_ref()
                    .and_then(|cached| cached.state.native_availability),
                guard
                    .cached
                    .as_ref()
                    .map_or(0, |cached| cached.state.native_control_seq),
                guard.failed_availability,
                guard.failed_control,
            )
        };
        control_seq = control_seq.max(cached_control_seq);
        for observed in [cached_availability, failed_availability]
            .into_iter()
            .flatten()
        {
            if historical.is_none_or(|(previous, _)| observed.0 > previous) {
                historical = Some(observed);
            }
        }
        let historical_owner_current = historical.is_some_and(|(_, subject)| {
            if Some(subject.context) != context {
                return false;
            }
            match subject.kind {
                NativeSubjectEvent::ChannelUnavailable(_)
                | NativeSubjectEvent::AdmissionRefusal(_) => {
                    subject.unavailability_still_applies(session_id)
                }
                _ => subject.owner_is_current(session_id),
            }
        });
        let refusal = if context.is_some() && current.is_none() {
            crate::process::worker_registry::control_admission(session_id).err()
        } else {
            None
        };
        let mut projection = crate::acp::state::NativeAvailabilityProjection::resolve(
            historical,
            context,
            current.as_ref(),
            historical_owner_current,
            refusal,
        );
        projection.control_failure = failed_control
            .filter(|(seq, subject)| {
                *seq > control_seq
                    && Some(subject.context) == context
                    && (current
                        .as_ref()
                        .is_some_and(|current| current.owner == subject.owner)
                        || subject.owner_is_current(session_id))
            })
            .and_then(|(seq, subject)| match subject.kind {
                NativeSubjectEvent::PromptDiagnostic | NativeSubjectEvent::TurnEnded => {
                    Some((seq, NativeControlFailure::Prompt))
                }
                NativeSubjectEvent::StartupDiagnostic => Some((seq, NativeControlFailure::Startup)),
                _ => None,
            });
        Ok(projection)
    }

    /// Whether the session has a hydrated fold, with no locking beyond the
    /// check itself. Lets a caller that can hydrate on demand (the live
    /// listener, via `SessionService::fold_control_state`) skip that work on
    /// every event and only pay for it on a cold session.
    pub fn is_hydrated(&self, session_id: &str) -> bool {
        let slot = self.slot(session_id);
        let guard = lock_slot(&slot);
        guard.cached.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::establishment::NativeOperationCorrelation;
    use crate::acp::event_sender::EventScope;
    use crate::acp::protocol::AcpOperationMetadata;
    use crate::acp::state::{AcpSessionId, AgentName};

    fn seed() -> AcpState {
        AcpState::new(AcpSessionId("s-1".into()), AgentName("claude".into()), None)
    }

    fn operation(correlation: NativeOperationCorrelation) -> EventMetadata {
        EventMetadata {
            scope: EventScope::OperationCurrent,
            operation: Some(AcpOperationMetadata {
                correlation,
                outcome: None,
            }),
            ..EventMetadata::history()
        }
    }

    #[test]
    fn cold_or_failed_hydration_never_creates_a_partial_fold() {
        let current = NativeOperationCorrelation::new(None);
        let cache = ControlStateCache::new();
        assert_eq!(
            cache.apply_if_cached("s-1", 42, &Event::ThinkingStarted, &operation(current)),
            None
        );
        assert!(!cache.is_hydrated("s-1"));
        assert!(cache
            .get_or_hydrate("s-1", || anyhow::bail!("unreadable retained event"))
            .is_err());
        assert!(!cache.is_hydrated("s-1"));
        let state = cache.get_or_hydrate("s-1", || Ok((seed(), 0))).unwrap();
        assert!(!state.turn_active);
        assert_eq!(state.last_seq, 0);
    }

    #[test]
    fn live_fold_preserves_callbacks_and_rejects_another_operations_terminal() {
        let current = NativeOperationCorrelation::new(None);
        let foreign = NativeOperationCorrelation::new(None);
        let cache = ControlStateCache::new();
        cache.get_or_hydrate("s-1", || Ok((seed(), 0))).unwrap();
        assert_eq!(
            cache.apply_if_cached("s-1", 1, &Event::ThinkingStarted, &operation(current)),
            Some(Status::Running)
        );
        let approval = Event::ApprovalRequested {
            approval: crate::acp::approvals::Approval {
                nonce: crate::acp::approvals::Nonce("n-1".into()),
                tool_call: crate::acp::state::ToolCall {
                    id: "tc-1".into(),
                    name: "Edit".into(),
                    kind: "edit".into(),
                    args_preview: String::new(),
                    started_at: chrono::Utc::now(),
                    diffs: Vec::new(),
                    memory_recall: None,
                    parent_tool_call_id: None,
                },
                destructive: false,
                options: Vec::new(),
                choice: false,
                plan: None,
                requested_at: chrono::Utc::now(),
                resolved: None,
            },
        };
        cache.apply_if_cached("s-1", 2, &approval, &operation(current));
        assert_eq!(
            cache.apply_if_cached("s-1", 2, &approval, &operation(current)),
            None
        );
        let stopped = Event::Stopped {
            reason: "end_turn".into(),
        };
        assert_eq!(
            cache.apply_if_cached("s-1", 3, &stopped, &operation(foreign)),
            Some(Status::Waiting)
        );
        let active = cache
            .get_or_hydrate("s-1", || anyhow::bail!("live fold was evicted"))
            .unwrap();
        assert!(active.turn_active);
        assert_eq!(active.pending_approvals.len(), 1);
        assert_eq!(
            cache.apply_if_cached("s-1", 4, &stopped, &operation(current)),
            Some(Status::Idle)
        );
        let ended = cache
            .get_or_hydrate("s-1", || anyhow::bail!("live fold was evicted"))
            .unwrap();
        assert!(!ended.turn_active);
        assert_eq!(ended.pending_approvals.len(), 1);
        cache.forget("s-1");
        assert!(!cache.is_hydrated("s-1"));
    }

    #[test]
    fn a_break_in_the_sequence_evicts_instead_of_folding_wrong() {
        let current = NativeOperationCorrelation::new(None);
        for (first, second, still_cached) in
            [(1, 2, true), (1, 1, true), (1, 3, false), (5, 1, false)]
        {
            let cache = ControlStateCache::new();
            cache
                .get_or_hydrate("s-1", || Ok((seed(), first - 1)))
                .unwrap();
            cache.apply_if_cached("s-1", first, &Event::ThinkingStarted, &operation(current));
            cache.apply_if_cached(
                "s-1",
                second,
                &Event::Stopped {
                    reason: "end_turn".into(),
                },
                &operation(current),
            );
            assert_eq!(
                cache.is_hydrated("s-1"),
                still_cached,
                "{first} -> {second}"
            );
            if still_cached {
                let state = cache
                    .get_or_hydrate("s-1", || anyhow::bail!("continuous fold was evicted"))
                    .unwrap();
                assert_eq!(state.turn_active, first == second);
                assert_eq!(state.last_seq, second);
            }
        }
    }
}
