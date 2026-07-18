//! Background status polling for TUI performance
//!
//! This module provides non-blocking status updates for sessions by running
//! tmux subprocess calls in a background thread. Two optimizations reduce
//! per-cycle overhead:
//!
//! 1. **Batched metadata**: A single `tmux list-panes -a` call fetches pane
//!    metadata (dead flag, current command) for all sessions at once, replacing
//!    O(3N) per-instance `display-message` subprocesses with O(1).
//!
//! 2. **Adaptive polling tiers**: Sessions are polled at different frequencies
//!    based on their status. Hot (Running/Waiting/Starting) every cycle, Warm
//!    (Idle/Unknown) every 5 cycles, Cold (Error) every 60 cycles, Frozen
//!    (Stopped/Deleting) never.

use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::session::{Instance, Status};
use crate::tui::worker::Worker;

/// Adaptive polling intervals (in cycles). 0 = never poll.
const TIER_HOT: u64 = 1;
const TIER_WARM: u64 = 5;
const TIER_COLD: u64 = 60;

fn polling_tier(status: Status) -> u64 {
    match status {
        Status::Running | Status::Waiting | Status::Starting => TIER_HOT,
        Status::Idle | Status::Unknown => TIER_WARM,
        Status::Error => TIER_COLD,
        Status::Stopped | Status::Deleting | Status::Creating => 0,
    }
}

/// A producer's report of what to do with an `Instance`'s
/// `idle_entered_at` field. Encodes three distinct intents that
/// `Option<DateTime<Utc>>` conflates:
///
/// * `Set(ts)`: producer observed a transition into `Idle` at `ts`.
/// * `Clear`: producer observed a transition out of `Idle`; the disk
///   value must be reset to `None`. Also emitted by the sandbox-dead
///   branch of [`poll_statuses_once`] as a synthesized transition
///   (container health flipped false without a user action).
/// * `Keep`: producer did not observe a transition (e.g. an
///   `attached_status_hooks` snapshot from a watcher clone that never
///   polled its own session); the disk value must not be touched, or a
///   real transition observed on a different path can be silently
///   clobbered by an unseeded snapshot.
///
/// Locked by `apply_status_update_preserves_idle_entered_at_on_keep`
/// in `src/tui/home/tests.rs` (a `#[cfg(test)]` item, so the reference
/// is kept as a code-span rather than an intra-doc link that would
/// silently degrade to literal text under `cargo doc`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum IdleIntent {
    /// Producer observed `Idle` at the carried timestamp; consumer sets
    /// the field to `Some(ts)`.
    Set(DateTime<Utc>),
    /// Producer observed a non-`Idle` status; consumer sets the field to
    /// `None`.
    Clear,
    /// Producer has no observation; consumer preserves the current value.
    #[default]
    Keep,
}

/// Result of a status check for a single session.
///
/// `Default` is derived so test fixtures can construct `StatusUpdate` with
/// `..Default::default()` and only set the fields under test, instead of
/// re-spelling every field at every call site. All field defaults resolve
/// through the standard chain: `Status` defaults to `Idle`, `IdleIntent` to
/// `Keep`, `Option::None`, `bool::false`, and `String::new`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct StatusUpdate {
    pub id: String,
    pub status: Status,
    pub last_error: Option<String>,
    /// Producer's intent for the real `Instance`'s `idle_entered_at`.
    /// See [`IdleIntent`] for the three-variant contract that replaces the
    /// original `Option<DateTime<Utc>>` (which conflated "clear this on a
    /// transition out of Idle" with "I have no observation, preserve").
    pub idle_entered_at: IdleIntent,
    /// Pulled from tmux `#{session_activity}` via
    /// `update_status_with_metadata`. Carried back so the main thread can
    /// persist it to the real Instance; the poller mutates a clone, so any
    /// fields not plumbed through here are dropped on the floor.
    pub last_accessed_at: Option<DateTime<Utc>>,
    /// Cached pane-dead reading from `tmux::PaneMetadata.pane_dead`. The
    /// main thread writes this onto `Instance.pane_dead_observed` so the
    /// Attention sort can treat dead panes as tier 99 without re-querying
    /// tmux per sort.
    pub pane_dead: bool,
    /// Snapshot of the polled clone's `live_status_baseline` after
    /// `update_status_with_metadata` ran. `None` from a producer that has
    /// no baseline yet (e.g. an `attached_status_hooks` snapshot whose
    /// watcher clone never polled) must not clear an already-established
    /// baseline, so the consumer applies this conditionally. `Some(_)` is
    /// unambiguous: apply it. See #2690.
    pub live_status_baseline: Option<Status>,
}

pub(super) struct StatusPollState {
    container_check_interval: Duration,
    last_container_check: Instant,
    container_states: HashMap<String, bool>,
    credential_refresh_interval: Duration,
    last_credential_refresh: Instant,
    cycle_count: u64,
}

impl StatusPollState {
    pub(super) fn new() -> Self {
        let container_check_interval = Duration::from_secs(5);
        let credential_refresh_interval = Duration::from_secs(1800);

        Self {
            container_check_interval,
            last_container_check: Instant::now() - container_check_interval,
            container_states: HashMap::new(),
            credential_refresh_interval,
            last_credential_refresh: Instant::now(),
            cycle_count: TIER_COLD - 1,
        }
    }
}

pub(super) fn poll_statuses_once(
    instances: Vec<Instance>,
    state: &mut StatusPollState,
) -> Vec<StatusUpdate> {
    state.cycle_count = state.cycle_count.wrapping_add(1);

    // Pre-scan: check if any instance would actually be polled this cycle.
    // If not, skip the batch subprocess calls entirely.
    let any_pollable = instances.iter().any(|inst| {
        let tier = polling_tier(inst.status);
        tier != 0 && state.cycle_count % tier == 0
    });

    let pane_metadata = if any_pollable {
        crate::tmux::refresh_session_cache();
        crate::tmux::batch_pane_metadata().unwrap_or_default()
    } else {
        HashMap::new()
    };

    // Refresh container health if any sandboxed session exists and interval elapsed
    let has_sandboxed = if any_pollable {
        let sandboxed = instances.iter().any(|i| i.is_sandboxed());
        if sandboxed && state.last_container_check.elapsed() >= state.container_check_interval {
            state.container_states = crate::containers::batch_container_health();
            state.last_container_check = Instant::now();
        }
        sandboxed
    } else {
        false
    };

    // Periodically re-sync sandbox credentials from the macOS Keychain
    // so long-lived sessions don't lose auth mid-run.
    if has_sandboxed && state.last_credential_refresh.elapsed() >= state.credential_refresh_interval
    {
        state.last_credential_refresh = Instant::now();
        let profiles: BTreeSet<String> = instances
            .iter()
            .filter(|inst| inst.is_sandboxed())
            .map(|inst| inst.effective_profile())
            .collect();
        for profile in profiles {
            crate::session::container_config::refresh_agent_configs_for_profile(&profile);
        }
    }

    instances
        .into_iter()
        .filter_map(|mut inst| {
            // Adaptive polling: skip instances whose tier interval hasn't elapsed
            let tier = polling_tier(inst.status);
            if tier == 0 || state.cycle_count % tier != 0 {
                return None;
            }

            // For sandboxed sessions, check if the container is dead before
            // falling through to tmux-based status detection.
            if inst.is_sandboxed()
                && !matches!(
                    inst.status,
                    Status::Stopped | Status::Deleting | Status::Starting | Status::Creating
                )
            {
                if let Some(sandbox) = &inst.sandbox_info {
                    if let Some(&running) = state.container_states.get(&sandbox.container_name) {
                        if !running {
                            return Some(StatusUpdate {
                                id: inst.id,
                                status: Status::Error,
                                last_error: Some("Container is not running".to_string()),
                                idle_entered_at: IdleIntent::Clear,
                                last_accessed_at: inst.last_accessed_at,
                                // Sandboxed sessions don't have a tmux pane in the
                                // usual sense; the Error tier itself sinks the row.
                                pane_dead: false,
                                live_status_baseline: Some(Status::Error),
                            });
                        }
                    }
                }
            }

            // Look up pre-fetched metadata for this instance's tmux session
            let session_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
            let metadata = pane_metadata.get(&session_name);
            let pane_dead = metadata.map(|m| m.pane_dead).unwrap_or(false);

            let prev_status = inst.status;
            inst.update_status_with_metadata(metadata);
            // On the first turn's `Running -> Idle` edge, best-effort auto-name a
            // still-default-named terminal session from its first turn. Detached
            // and self-gating, so this is cheap for the common (ineligible) case.
            if prev_status == Status::Running && inst.status == Status::Idle {
                crate::session::smart_rename::maybe_spawn_terminal_smart_rename(&inst);
            }

            Some(StatusUpdate {
                id: inst.id,
                status: inst.status,
                last_error: inst.last_error,
                // This producer is authoritative on `idle_entered_at`
                // for both the sandbox-dead branch above and the tmux
                // branch reached via `update_status_with_metadata`, and
                // never emits `IdleIntent::Keep`:
                // `attached_status_hooks::snapshot` is the sole
                // `Keep`-emitter (see its docstring). The asymmetry is
                // load-bearing: a future consolidation that adds `Keep`
                // to this producer would erase the baseline seed that
                // `update_status_with_metadata` writes.
                idle_entered_at: match inst.idle_entered_at {
                    Some(ts) => IdleIntent::Set(ts),
                    None => IdleIntent::Clear,
                },
                last_accessed_at: inst.last_accessed_at,
                pane_dead,
                live_status_baseline: inst.live_status_baseline,
            })
        })
        .collect()
}

/// Background thread that polls session status without blocking the UI
pub struct StatusPoller {
    worker: Worker<Vec<Instance>, Vec<StatusUpdate>>,
}

impl StatusPoller {
    pub fn new() -> Self {
        // The adaptive-tier state lives in the handler closure so it carries
        // across refresh cycles for the lifetime of the worker thread.
        let mut state = StatusPollState::new();
        Self {
            worker: Worker::spawn("aoe-status-poller", move |instances| {
                poll_statuses_once(instances, &mut state)
            }),
        }
    }

    /// Request a status refresh for all given instances (non-blocking).
    pub fn request_refresh(&self, instances: Vec<Instance>) {
        self.worker.request(instances);
    }

    /// Try to receive status updates without blocking. Surfaces
    /// `Disconnected` (see `Worker::try_recv`) so the caller can respawn the
    /// worker: swallowing it would leave `pending_status_refresh` set
    /// forever, silently freezing every session's live status.
    pub fn try_recv_updates(&self) -> Result<Vec<StatusUpdate>, std::sync::mpsc::TryRecvError> {
        self.worker.try_recv()
    }
}

impl Default for StatusPoller {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_update_carries_idle_entered_at() {
        // Regression: the polling loop runs `update_status_with_metadata`
        // on a clone, then projects the result into a `StatusUpdate`. If
        // `idle_entered_at` falls off the projection (the original bug),
        // the breathe rattle + fresh-idle color never fire in the TUI
        // even though the wrapper sets the timestamp on the clone
        // correctly.
        let ts = Utc::now();
        let update = StatusUpdate {
            id: "abc".into(),
            status: Status::Idle,
            last_error: None,
            idle_entered_at: IdleIntent::Set(ts),
            last_accessed_at: None,
            pane_dead: false,
            live_status_baseline: None,
        };
        assert_eq!(update.idle_entered_at, IdleIntent::Set(ts));
    }

    /// #2690 follow-up. `StatusUpdate::default()` must be a semantic
    /// no-op so test fixtures can use `..Default::default()` and only
    /// override the fields under test. A future field with a non-trivial
    /// default (e.g. an id defaulting to empty string that a consumer
    /// treats as "match all") would silently corrupt fixture-based
    /// tests. This lock catches such a field addition at review time.
    #[test]
    fn test_status_update_default_is_no_op() {
        let default = StatusUpdate::default();
        assert_eq!(default.id, String::new(), "id defaults to empty string");
        assert_eq!(default.status, Status::Idle, "status defaults to Idle");
        assert_eq!(
            default.last_error, None,
            "last_error defaults to None (no error observed)"
        );
        assert_eq!(
            default.idle_entered_at,
            IdleIntent::Keep,
            "idle_entered_at defaults to Keep (no observation)"
        );
        assert_eq!(
            default.last_accessed_at, None,
            "last_accessed_at defaults to None (no observation to carry back)"
        );
        assert!(
            !default.pane_dead,
            "pane_dead defaults to false (no dead-pane observation)"
        );
        assert_eq!(
            default.live_status_baseline, None,
            "live_status_baseline defaults to None (no baseline observed yet)"
        );
    }

    #[test]
    fn test_polling_tier_hot() {
        assert_eq!(polling_tier(Status::Running), TIER_HOT);
        assert_eq!(polling_tier(Status::Waiting), TIER_HOT);
        assert_eq!(polling_tier(Status::Starting), TIER_HOT);
    }

    #[test]
    fn test_polling_tier_warm() {
        assert_eq!(polling_tier(Status::Idle), TIER_WARM);
        assert_eq!(polling_tier(Status::Unknown), TIER_WARM);
    }

    #[test]
    fn test_polling_tier_cold() {
        assert_eq!(polling_tier(Status::Error), TIER_COLD);
    }

    #[test]
    fn test_polling_tier_frozen() {
        assert_eq!(polling_tier(Status::Stopped), 0);
        assert_eq!(polling_tier(Status::Deleting), 0);
    }

    #[test]
    fn test_tier_cycle_alignment() {
        // Hot sessions are polled every cycle: TIER_HOT must stay at 1.
        assert_eq!(TIER_HOT, 1);
        // Warm sessions are polled every 5 cycles
        assert_ne!(1u64 % TIER_WARM, 0);
        assert_ne!(2u64 % TIER_WARM, 0);
        assert_eq!(5u64 % TIER_WARM, 0);
        assert_eq!(10u64 % TIER_WARM, 0);
        // Cold sessions are polled every 60 cycles
        assert_ne!(1u64 % TIER_COLD, 0);
        assert_eq!(60u64 % TIER_COLD, 0);
        assert_eq!(120u64 % TIER_COLD, 0);
    }

    #[test]
    fn test_first_cycle_polls_all_tiers() {
        // cycle_count starts at TIER_COLD - 1, first cycle wraps to TIER_COLD
        let first_cycle = (TIER_COLD - 1).wrapping_add(1);
        // TIER_HOT == 1 (see test_tier_cycle_alignment), so any cycle trivially
        // polls hot; just verify the warm and cold alignments here.
        assert_eq!(first_cycle % TIER_WARM, 0, "first cycle must poll warm");
        assert_eq!(first_cycle % TIER_COLD, 0, "first cycle must poll cold");
    }

    #[test]
    #[serial_test::serial]
    fn poll_statuses_once_never_emits_idle_intent_keep() {
        // Regression guard for #2690: the asymmetry that only
        // `attached_status_hooks::snapshot` produces `IdleIntent::Keep`
        // and `poll_statuses_once` never does is load-bearing (see the
        // comment above the `idle_entered_at` projection). A future
        // consolidation that adds `Keep` to this producer would erase
        // the baseline seed that `update_status_with_metadata` writes
        // on its first observation, silently reintroducing the #2690
        // restamp bug.
        //
        // Structural today (both emit sites hardcode Set/Clear), so
        // this test tightens against a refactor that would relax the
        // projection into a match arm capable of returning Keep.
        let mut running = Instance::new("running", "/tmp/running");
        running.status = Status::Running;
        let mut idle = Instance::new("idle", "/tmp/idle");
        idle.status = Status::Idle;
        idle.idle_entered_at = Some(Utc::now() - chrono::Duration::minutes(5));
        let mut error = Instance::new("error", "/tmp/error");
        error.status = Status::Error;

        let mut state = StatusPollState::new();
        let updates = poll_statuses_once(vec![running, idle, error], &mut state);

        assert!(
            !updates.is_empty(),
            "hot/warm/cold instances all align on the first cycle; at least one update expected"
        );
        for update in updates {
            assert!(
                !matches!(update.idle_entered_at, IdleIntent::Keep),
                "poll_statuses_once must never emit IdleIntent::Keep (session {}); got {:?}",
                update.id,
                update.idle_entered_at
            );
        }
    }
}
