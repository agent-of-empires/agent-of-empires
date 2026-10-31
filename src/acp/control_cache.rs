//! Live per-session control state, folded once at the publish choke point.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::state::{AcpState, Event, NativeControlFailure, NativeEventSubject, NativeSubjectEvent};
use crate::session::Status;

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
    native_scope: Option<crate::acp::state::NativeObservationScope>,
    failed_availability: Option<(u64, NativeEventSubject)>,
    failed_control: Option<(u64, NativeEventSubject)>,
}

type Slot = Arc<Mutex<SessionSlot>>;

#[derive(Debug, Default)]
pub struct ControlStateCache {
    sessions: Mutex<HashMap<String, Slot>>,
}

/// Recover a poisoned lock rather than propagating the panic: a poisoned
/// control-state mutex means some other thread panicked mid-fold, and the
/// worst case here is a stale projection, which the seq guard below evicts.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl ControlStateCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// The slot for `session_id`, creating an empty one if absent.
    fn slot(&self, session_id: &str) -> Slot {
        let mut map = lock(&self.sessions);
        Arc::clone(
            map.entry(session_id.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(SessionSlot::default()))),
        )
    }

    /// Fold `event` into the session's cached state, if it has one. Returns the
    /// sidebar status right after this event, so the caller can carry it with
    /// the frame instead of a later reader finding the fold already ahead.
    pub fn apply_if_cached(&self, session_id: &str, seq: u64, event: &Event) -> Option<Status> {
        let slot = self.slot(session_id);
        let mut guard = lock(&slot);
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
        if seq != cached.last_seq + 1 || cached.state.apply_event(seq, event.clone()).is_err() {
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
        let guard = lock(&slot);
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

    /// Drop a session's fold.
    pub fn forget(&self, session_id: &str) {
        let mut map = lock(&self.sessions);
        map.remove(session_id);
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
        let mut guard = lock(&slot);
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
    pub fn get_or_hydrate(
        &self,
        session_id: &str,
        hydrate: impl FnOnce() -> (AcpState, u64),
    ) -> AcpState {
        let slot = self.slot(session_id);
        let mut guard = lock(&slot);
        if let Some(cached) = guard.cached.as_ref() {
            return cached.state.clone();
        }
        let (state, last_seq) = hydrate();
        guard.cached = Some(Cached {
            state: state.clone(),
            last_seq,
        });
        state
    }

    /// Bounded qualifiers only: preflight does not clone warm control payloads.
    pub(crate) fn native_metadata(
        &self,
        session_id: &str,
    ) -> Option<(Option<(u64, NativeEventSubject)>, u64)> {
        let slot = self.slot(session_id);
        let guard = lock(&slot);
        guard.cached.as_ref().map(|cached| {
            (
                cached.state.native_availability,
                cached.state.native_control_seq,
            )
        })
    }

    /// Project current native authority without changing the historical fold.
    pub(crate) fn project_native(
        &self,
        session_id: &str,
        mut state: AcpState,
        identity: Option<(String, chrono::DateTime<chrono::Utc>, u64)>,
        current: Option<crate::acp::state::CurrentNativeAvailability>,
    ) -> AcpState {
        let projection = self.native_projection(
            session_id,
            state.native_availability,
            state.native_control_seq,
            identity,
            current,
        );
        state.apply_native_availability(projection);
        state
    }

    pub(crate) fn native_projection(
        &self,
        session_id: &str,
        mut historical: Option<(u64, NativeEventSubject)>,
        mut control_seq: u64,
        identity: Option<(String, chrono::DateTime<chrono::Utc>, u64)>,
        current: Option<crate::acp::state::CurrentNativeAvailability>,
    ) -> crate::acp::state::NativeAvailabilityProjection {
        let slot = self.slot(session_id);
        let scope = identity.and_then(|(profile, created_at, generation)| {
            let storage = crate::session::Storage::open_reference(&profile).ok()?;
            let context = crate::acp::state::NativeEventContext::capture_identity(
                session_id, created_at, generation, &storage,
            )
            .ok()?;
            let mut guard = lock(&slot);
            if let Some(scope) = guard
                .native_scope
                .as_ref()
                .filter(|scope| scope.context == context)
            {
                return Some(scope.clone());
            }
            let scope = crate::acp::state::NativeObservationScope {
                context,
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
        let native_guard = scope.as_ref().and_then(|scope| {
            scope
                .fence
                .try_lock_observation_matching(|_| true)
                .ok()
                .flatten()
        });
        let context = native_guard
            .as_ref()
            .and(scope.as_ref())
            .map(|scope| scope.context);
        let current = current.and_then(|mut current| {
            (Some(current.context) == context && current.refresh(session_id)).then_some(current)
        });
        let (cached_availability, cached_control_seq, failed_availability, failed_control) = {
            let guard = lock(&slot);
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
            subject.owner_is_current(session_id)
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
        projection
    }

    /// Whether the session has a hydrated fold, with no locking beyond the
    /// check itself. Lets a caller that can hydrate on demand (the live
    /// listener, via `SessionService::fold_control_state`) skip that work on
    /// every event and only pay for it on a cold session.
    pub fn is_hydrated(&self, session_id: &str) -> bool {
        let slot = self.slot(session_id);
        let guard = lock(&slot);
        guard.cached.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::state::test_support::{prompt, stopped};
    use crate::acp::state::{AcpSessionId, AgentName};

    fn seed() -> AcpState {
        AcpState::new(AcpSessionId("s-1".into()), AgentName("claude".into()), None)
    }

    /// An un-hydrated session must not start folding mid-stream: the
    /// handshake events that carry `steering` sit near seq 1 and never
    /// repeat, so a partial fold would tell dispatch a steerable agent is not
    /// steerable and reintroduce #2805.
    #[test]
    fn a_session_nothing_hydrated_stays_uncached() {
        let cache = ControlStateCache::new();
        cache.apply_if_cached("s-1", 42, &prompt("go"));
        assert!(!cache.is_hydrated("s-1"));

        let mut hydrated = 0;
        let state = cache.get_or_hydrate("s-1", || {
            hydrated += 1;
            (seed(), 0)
        });
        assert!(!state.turn_active);
        assert_eq!(hydrated, 1);
    }

    #[test]
    fn a_hydrated_session_folds_live_without_rehydrating() {
        let cache = ControlStateCache::new();
        let mut hydrates = 0;
        let mut hydrate_count = || {
            hydrates += 1;
        };
        cache.get_or_hydrate("s-1", || {
            hydrate_count();
            (seed(), 0)
        });
        cache.apply_if_cached("s-1", 1, &prompt("go"));
        let state = cache.get_or_hydrate("s-1", || {
            hydrate_count();
            (seed(), 0)
        });
        assert!(state.turn_active, "the live fold reached the reader");

        // A repeated seq is a publish retry and must not double-apply.
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
        cache.apply_if_cached("s-1", 2, &approval);
        cache.apply_if_cached("s-1", 2, &approval);
        cache.apply_if_cached("s-1", 3, &stopped("end_turn"));
        let state = cache.get_or_hydrate("s-1", || {
            hydrate_count();
            (seed(), 0)
        });
        assert!(!state.turn_active);
        assert_eq!(state.pending_approvals.len(), 1);
        assert_eq!(hydrates, 1, "one hydrate for the session's whole life");

        // Forget drops the fold so a reused id starts clean.
        cache.forget("s-1");
        assert!(!cache.is_hydrated("s-1"));
    }

    /// Each fold reports the status right after its own event, which is what
    /// rides on the broadcast frame; a cold session or a retry reports none.
    #[test]
    fn apply_if_cached_reports_the_status_after_its_own_event() {
        let cache = ControlStateCache::new();
        assert_eq!(cache.apply_if_cached("s-1", 1, &prompt("go")), None);
        cache.get_or_hydrate("s-1", || (seed(), 0));
        assert_eq!(
            cache.apply_if_cached("s-1", 1, &prompt("go")),
            Some(Status::Running)
        );
        assert_eq!(
            cache.apply_if_cached("s-1", 1, &prompt("go")),
            None,
            "retry"
        );
        assert_eq!(
            cache.apply_if_cached("s-1", 2, &stopped("end_turn")),
            Some(Status::Idle)
        );
    }

    /// Anything that does not continue the sequence evicts rather than folds.
    #[test]
    fn a_break_in_the_sequence_evicts_instead_of_folding_wrong() {
        let cases: [(&str, u64, u64, bool); 4] = [
            // (name, first seq, second seq, still cached after)
            ("consecutive seqs fold", 1, 2, true),
            ("an exact repeat is a benign publish retry", 1, 1, true),
            ("a forward gap means events were missed", 1, 3, false),
            (
                "a backward jump means the seq counter was reset",
                5,
                1,
                false,
            ),
        ];
        for (name, first, second, still_cached) in cases {
            let cache = ControlStateCache::new();
            cache.get_or_hydrate("s-1", || (seed(), first - 1));
            cache.apply_if_cached("s-1", first, &prompt("go"));
            cache.apply_if_cached("s-1", second, &stopped("end_turn"));
            assert_eq!(cache.is_hydrated("s-1"), still_cached, "{name}");
        }
    }
}
