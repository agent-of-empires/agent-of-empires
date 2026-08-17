//! Live per-session control state, folded once at the publish choke point.
//!
//! Prompt dispatch (`super::dispatch`) needs the daemon's own `AcpState` on an
//! HTTP request, but the WS folds are per-connection, so there was none to
//! read. Rebuilding one from the event log per request measured 68ms at 20k
//! events and 342ms at 100k, on a store whose retention default is unlimited,
//! and it holds the event store's connection mutex for the whole scan, so it
//! stalls event recording daemon-wide rather than only slowing the one caller.
//!
//! So the daemon keeps the fold hot: `ChannelSink::publish_persisted` applies
//! each event to a cached `AcpState` as it records and broadcasts it, and a
//! reader gets an O(1) clone. The log stays the source of truth; this is a
//! projection of it that can always be thrown away and rebuilt.
//!
//! **A session is only ever cached by a full hydrate from the log.** A fold
//! started mid-stream would miss the handshake events that sit near seq 1 and
//! never repeat (`PromptCapabilities`, which is what tells dispatch the agent
//! is steerable, #2805), so `apply_if_cached` deliberately no-ops on a session
//! nothing has hydrated yet.
//!
//! Locking is per session: the map lock is held only long enough to clone a
//! slot handle, so one session's hydrate never blocks another's publish.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::state::{AcpState, Event};

/// A session's folded control state and how far it has been folded.
#[derive(Debug, Clone)]
struct Cached {
    state: AcpState,
    /// Highest seq folded in. `AcpState::apply_event` is not idempotent
    /// (`ApprovalRequested` pushes unconditionally), so a repeated seq must be
    /// dropped rather than applied twice.
    last_seq: u64,
}

/// Per-session slot. `None` means "not hydrated", which is the state every
/// session starts in and returns to whenever the fold can no longer be trusted.
type Slot = Arc<Mutex<Option<Cached>>>;

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

    /// The slot for `session_id`, creating an empty one if absent. The map
    /// lock is released before the caller touches the slot.
    fn slot(&self, session_id: &str) -> Slot {
        let mut map = lock(&self.sessions);
        Arc::clone(
            map.entry(session_id.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(None))),
        )
    }

    /// Fold `event` into the session's cached state, if it has one.
    ///
    /// Called from the publish choke point, which persists before it
    /// broadcasts and publishes in seq order. The seq guard turns anything
    /// that does not continue the sequence into an eviction rather than a
    /// wrong fold: a forward gap means events were missed, and a backward jump
    /// means the supervisor's per-session counter was reset (an
    /// `acp_disable` / `acp_enable` round trip, or a session recreated under
    /// the same id). Either way the log is authoritative and the next reader
    /// rebuilds from it.
    pub fn apply_if_cached(&self, session_id: &str, seq: u64, event: &Event) {
        let slot = self.slot(session_id);
        let mut guard = lock(&slot);
        let Some(cached) = guard.as_mut() else {
            return;
        };
        if seq == cached.last_seq {
            // Exact repeat of the seq we already folded: a benign publish
            // retry (the store reports these as primary-key collisions).
            return;
        }
        if seq != cached.last_seq + 1 {
            *guard = None;
            return;
        }
        if cached.state.apply_event(event.clone()).is_err() {
            *guard = None;
            return;
        }
        cached.last_seq = seq;
    }

    /// Drop a session's fold. Used when the event log behind it is deleted and
    /// when a persist fails, since a projection of a log that is missing an
    /// event is not a projection of that log.
    pub fn forget(&self, session_id: &str) {
        let mut map = lock(&self.sessions);
        map.remove(session_id);
    }

    /// The session's control state, running `hydrate` on a miss.
    ///
    /// `hydrate` returns the state folded over the whole log plus the highest
    /// seq it saw, and runs under the session's own slot lock, so a publish
    /// for this session waits for it while other sessions carry on. Both
    /// interleavings are safe: a publish that landed before the scan is
    /// already in the returned state and is dropped by the `seq == last_seq`
    /// guard, and one that lands after continues the sequence normally.
    pub fn get_or_hydrate(
        &self,
        session_id: &str,
        hydrate: impl FnOnce() -> (AcpState, u64),
    ) -> AcpState {
        let slot = self.slot(session_id);
        let mut guard = lock(&slot);
        if let Some(cached) = guard.as_ref() {
            return cached.state.clone();
        }
        let (state, last_seq) = hydrate();
        *guard = Some(Cached {
            state: state.clone(),
            last_seq,
        });
        state
    }

    #[cfg(test)]
    fn is_cached(&self, session_id: &str) -> bool {
        let slot = self.slot(session_id);
        let guard = lock(&slot);
        guard.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::state::{AcpSessionId, AgentName};

    fn seed() -> AcpState {
        AcpState::new(AcpSessionId("s-1".into()), AgentName("claude".into()), None)
    }

    fn prompt() -> Event {
        Event::UserPromptSent {
            text: "go".into(),
            attachments: Vec::new(),
            prompt_id: None,
        }
    }

    fn stopped() -> Event {
        Event::Stopped {
            reason: "end_turn".into(),
        }
    }

    /// An un-hydrated session must not start folding mid-stream: the
    /// handshake events that carry `steering` sit near seq 1 and never
    /// repeat, so a partial fold would tell dispatch a steerable agent is not
    /// steerable and reintroduce #2805.
    #[test]
    fn a_session_nothing_hydrated_stays_uncached() {
        let cache = ControlStateCache::new();
        cache.apply_if_cached("s-1", 42, &prompt());
        assert!(!cache.is_cached("s-1"));

        let mut hydrated = 0;
        let state = cache.get_or_hydrate("s-1", || {
            hydrated += 1;
            (seed(), 0)
        });
        assert!(!state.turn_active);
        assert_eq!(hydrated, 1);
    }

    /// The point of the cache: a hydrated session folds live and never scans
    /// the log again.
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
        cache.apply_if_cached("s-1", 1, &prompt());
        let state = cache.get_or_hydrate("s-1", || {
            hydrate_count();
            (seed(), 0)
        });
        assert!(state.turn_active, "the live fold reached the reader");

        cache.apply_if_cached("s-1", 2, &stopped());
        let state = cache.get_or_hydrate("s-1", || {
            hydrate_count();
            (seed(), 0)
        });
        assert!(!state.turn_active);
        assert_eq!(hydrates, 1, "one hydrate for the session's whole life");
    }

    /// Anything that does not continue the sequence evicts rather than folds.
    /// Applying a repeated `UserPromptSent` is harmless, but a repeated
    /// `ApprovalRequested` would leave a second, unresolvable card, so the
    /// guard is on the seq rather than on the event kind.
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
            cache.apply_if_cached("s-1", first, &prompt());
            cache.apply_if_cached("s-1", second, &stopped());
            assert_eq!(cache.is_cached("s-1"), still_cached, "{name}");
        }
    }

    /// A repeat must not double-apply. `ApprovalRequested` is the event that
    /// makes this load-bearing: it pushes unconditionally, so folding it twice
    /// leaves an approval card nothing can resolve.
    #[test]
    fn a_repeated_seq_is_not_folded_twice() {
        let approval = |nonce: &str| crate::acp::approvals::Approval {
            nonce: crate::acp::approvals::Nonce(nonce.to_string()),
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
            requested_at: chrono::Utc::now(),
            resolved: None,
        };
        let cache = ControlStateCache::new();
        cache.get_or_hydrate("s-1", || (seed(), 0));
        let event = Event::ApprovalRequested {
            approval: approval("n-1"),
        };
        cache.apply_if_cached("s-1", 1, &event);
        cache.apply_if_cached("s-1", 1, &event);
        let state = cache.get_or_hydrate("s-1", || (seed(), 0));
        assert_eq!(state.pending_approvals.len(), 1);
    }

    /// Forgetting a session drops the fold, so the id can be reused (a delete
    /// and recreate) without inheriting the old conversation's turn flags.
    #[test]
    fn forget_drops_the_fold_so_a_reused_id_starts_clean() {
        let cache = ControlStateCache::new();
        cache.get_or_hydrate("s-1", || (seed(), 0));
        cache.apply_if_cached("s-1", 1, &prompt());
        assert!(cache.get_or_hydrate("s-1", || (seed(), 0)).turn_active);

        cache.forget("s-1");
        assert!(!cache.is_cached("s-1"));
        assert!(!cache.get_or_hydrate("s-1", || (seed(), 0)).turn_active);
    }
}
