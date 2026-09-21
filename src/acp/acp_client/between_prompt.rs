//! Between-prompt idle detection: ending agent-initiated turns (a Monitor
//! or scheduled wake resuming the agent) that no aoe prompt owns (#2325).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::lifecycle::{LifecycleSignal, OffProtocolWorkKind, TerminalClaim};
use super::watchdog::OFF_PROTOCOL_WORK_GRACE_FLOOR;

/// Grace for an agent-initiated turn after its end-of-turn accounting marker.
pub(super) const BETWEEN_PROMPT_IDLE_GRACE: Duration = Duration::from_secs(3);

/// Grace for a turn that stalled without accounting or a scheduled wake.
const BETWEEN_PROMPT_STALL_GRACE: Duration = Duration::from_secs(120);

pub(super) const BETWEEN_PROMPT_IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(1);

/// State update for one notification's classified signals; `None` for ambient
/// updates. Every tracked signal refreshes `last_lifecycle_at`, so the fast
/// grace measures from the end-of-turn marker itself.
#[derive(Debug, PartialEq)]
pub(super) struct BetweenPromptUpdate {
    pub(super) cost_seen: bool,
    pub(super) last_lifecycle_at: i64,
    /// Latest pending wake `at` in ms, `0` when none. Stored without the
    /// floor so an expired wake self-heals on the fast grace (#2371).
    pub(super) wake_at: i64,
}

pub(super) fn between_prompt_signal_update(
    lifecycle: Option<&LifecycleSignal>,
    wakeup: Option<&LifecycleSignal>,
    now_ms: i64,
    prev_wake_at: i64,
) -> Option<BetweenPromptUpdate> {
    let tracked = |cost_seen| BetweenPromptUpdate {
        cost_seen,
        last_lifecycle_at: now_ms,
        wake_at: prev_wake_at,
    };
    if let Some(LifecycleSignal::WakeupPending { at }) = wakeup {
        return Some(BetweenPromptUpdate {
            wake_at: at.timestamp_millis().max(prev_wake_at),
            ..tracked(false)
        });
    }
    match lifecycle? {
        LifecycleSignal::TerminalUsage => Some(tracked(true)),
        // The tail of a compaction, which on cancel arrives after the turn's
        // own terminal; arming here would claim a turn that already ended.
        LifecycleSignal::CompactionCompleted | LifecycleSignal::CompactionFailed => None,
        _ => Some(tracked(false)),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct BetweenPromptWorkState {
    pub(super) tool_calls: bool,
    pub(super) background_agents: bool,
}

impl BetweenPromptWorkState {
    pub(super) fn is_busy(self) -> bool {
        self.tool_calls || self.background_agents
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn between_prompt_should_fire(
    active: bool,
    now_ms: i64,
    last_lifecycle_ms: i64,
    wake_at_ms: Option<i64>,
    cost_seen: bool,
    work_in_flight: bool,
    off_protocol_work_seen: bool,
    fast_grace: Duration,
    floor: Duration,
) -> bool {
    if !active || work_in_flight || wake_at_ms.is_some_and(|at| now_ms < at) {
        return false;
    }
    // Untracked off-protocol work has no completion signal and holds the
    // floor. Accounting or an expired wake means the turn is done. Anything
    // else is a stalled stream.
    let grace = if off_protocol_work_seen {
        floor
    } else if cost_seen || wake_at_ms.is_some() {
        fast_grace
    } else {
        BETWEEN_PROMPT_STALL_GRACE
    };
    now_ms - last_lifecycle_ms >= grace.as_millis() as i64
}

/// An adopted turn (#2899) has no owning prompt, so this watchdog emits its
/// terminal: `prompt_complete` after accounting, else `reattach_idle` for
/// recovery. Agent-initiated turns keep `agent_idle`.
pub(super) fn between_prompt_stop_reason(adopted: bool, cost_seen: bool) -> &'static str {
    match (adopted, cost_seen) {
        (false, _) => "agent_idle",
        (true, true) => "prompt_complete",
        (true, false) => "reattach_idle",
    }
}

#[derive(Debug, Default)]
struct TrackerState {
    active: bool,
    cost_seen: bool,
    last_lifecycle_at: i64,
    wake_at: i64,
    /// Latched by a successful untracked background launch.
    off_protocol: bool,
    /// Tool id to its `run_in_background` flag.
    tools: HashMap<String, bool>,
}

impl TrackerState {
    /// Clears everything except the timestamp, which only matters once armed.
    fn reset(&mut self) {
        *self = Self {
            last_lifecycle_at: self.last_lifecycle_at,
            ..Self::default()
        };
    }
}

/// Between-prompt turn state, shared by the notification handler and the
/// command loop's idle tick.
#[derive(Debug, Default)]
pub(super) struct BetweenPromptTracker {
    state: Mutex<TrackerState>,
    /// Async background agents with a live tailer, which removes its id on
    /// completion. Tracked precisely, so they never latch the floor.
    pub(super) bg_agents: Arc<Mutex<HashSet<String>>>,
}

impl BetweenPromptTracker {
    fn state(&self) -> std::sync::MutexGuard<'_, TrackerState> {
        self.state
            .lock()
            .expect("between-prompt state mutex poisoned")
    }

    /// Fold one notification's signals while no aoe prompt is in flight.
    /// Returns true when this arms a new agent-initiated turn.
    pub(super) fn observe(
        &self,
        lifecycle: Option<&LifecycleSignal>,
        wakeup: Option<&LifecycleSignal>,
        now_ms: i64,
        adopted_turn_active: bool,
        terminal_claim: &TerminalClaim,
    ) -> bool {
        let mut state = self.state();
        let mut armed = false;
        if let Some(u) = between_prompt_signal_update(lifecycle, wakeup, now_ms, state.wake_at) {
            if !state.active {
                state.active = true;
                terminal_claim.begin_turn();
                armed = true;
            }
            state.cost_seen = u.cost_seen;
            state.last_lifecycle_at = u.last_lifecycle_at;
            state.wake_at = u.wake_at;
        }
        match lifecycle {
            Some(LifecycleSignal::ToolStarted {
                id,
                is_background_task,
            }) => *state.tools.entry(id.clone()).or_default() |= *is_background_task,
            Some(LifecycleSignal::ToolCompleted {
                id,
                succeeded,
                off_protocol_work,
            }) => {
                let was_background = state.tools.remove(id).unwrap_or(false);
                let tracked_async = *off_protocol_work == Some(OffProtocolWorkKind::AsyncAgent);
                if *succeeded && !tracked_async && (off_protocol_work.is_some() || was_background) {
                    state.off_protocol = true;
                }
            }
            // A tool open across a reattach never reports its terminal frame
            // here, so an adopted turn's accounting marker drops the inherited
            // tool bookkeeping (#2899). Wakes and async agents keep theirs.
            Some(LifecycleSignal::TerminalUsage) if adopted_turn_active => {
                state.tools.clear();
                state.off_protocol = false;
            }
            _ => {}
        }
        armed
    }

    pub(super) fn work_state(&self) -> BetweenPromptWorkState {
        BetweenPromptWorkState {
            tool_calls: !self.state().tools.is_empty(),
            background_agents: !self
                .bg_agents
                .lock()
                .expect("between-prompt bg-agents mutex poisoned")
                .is_empty(),
        }
    }

    pub(super) fn deactivate(&self) {
        self.state().active = false;
    }

    /// A real prompt supersedes any tracked agent-initiated turn.
    pub(super) fn reset_for_prompt(&self) {
        self.state().reset();
    }

    /// When the idle watchdog should fire, clear all tracking (so a stale wake
    /// or latch cannot skew the next turn) and return whether accounting was
    /// seen.
    pub(super) fn take_idle_fire(&self, now_ms: i64) -> Option<bool> {
        let busy = self.work_state().is_busy();
        let mut state = self.state();
        let fire = between_prompt_should_fire(
            state.active,
            now_ms,
            state.last_lifecycle_at,
            (state.wake_at != 0).then_some(state.wake_at),
            state.cost_seen,
            busy,
            state.off_protocol,
            BETWEEN_PROMPT_IDLE_GRACE,
            OFF_PROTOCOL_WORK_GRACE_FLOOR,
        );
        if !fire {
            return None;
        }
        let cost_seen = state.cost_seen;
        state.reset();
        drop(state);
        self.bg_agents
            .lock()
            .expect("between-prompt bg-agents mutex poisoned")
            .clear();
        Some(cost_seen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAST: Duration = BETWEEN_PROMPT_IDLE_GRACE;
    const FLOOR: Duration = OFF_PROTOCOL_WORK_GRACE_FLOOR;
    const STALL: Duration = BETWEEN_PROMPT_STALL_GRACE;

    fn ms(d: Duration) -> i64 {
        d.as_millis() as i64
    }

    #[test]
    fn between_prompt_should_fire_cases() {
        let last = 1_000_000;
        // (name, active, now, wake_at, cost_seen, work_in_flight, off_protocol, fires)
        let cases = [
            (
                "inactive", false, 10_000_000, None, true, false, false, false,
            ),
            (
                "cost, under fast",
                true,
                last + ms(FAST) - 500,
                None,
                true,
                false,
                false,
                false,
            ),
            (
                "cost, past fast",
                true,
                last + ms(FAST) + 500,
                None,
                true,
                false,
                false,
                true,
            ),
            // #2573, #2899: open tools or background agents suppress.
            (
                "work in flight",
                true,
                last + ms(FLOOR) + 10_000,
                None,
                true,
                true,
                false,
                false,
            ),
            (
                "work drained",
                true,
                last + ms(FLOOR) + 10_000,
                None,
                true,
                false,
                false,
                true,
            ),
            (
                "stall, under",
                true,
                last + ms(STALL) - 1000,
                None,
                false,
                false,
                false,
                false,
            ),
            (
                "stall, past",
                true,
                last + ms(STALL) + 1000,
                None,
                false,
                false,
                false,
                true,
            ),
            (
                "off-protocol past stall",
                true,
                last + ms(STALL) + 60_000,
                None,
                false,
                false,
                true,
                false,
            ),
            (
                "off-protocol past floor",
                true,
                last + ms(FLOOR) + 1,
                None,
                false,
                false,
                true,
                true,
            ),
            (
                "future wake",
                true,
                last + 60_000,
                Some(last + 65_000),
                false,
                false,
                false,
                false,
            ),
            // #2371: an expired wake self-heals on the fast grace.
            (
                "expired wake, under fast",
                true,
                last + ms(FAST) - 500,
                Some(last - 10_000),
                false,
                false,
                false,
                false,
            ),
            (
                "expired wake, past fast",
                true,
                last + ms(FAST) + 500,
                Some(last - 10_000),
                false,
                false,
                false,
                true,
            ),
            (
                "expired wake, tool open",
                true,
                last + 60_000,
                Some(last - 10_000),
                false,
                true,
                false,
                false,
            ),
            (
                "expired wake, off-protocol",
                true,
                last + 21_000,
                Some(last - 10_000),
                false,
                false,
                true,
                false,
            ),
            (
                "expired wake, off-protocol past floor",
                true,
                last + ms(FLOOR) + 1,
                Some(last - 10_000),
                false,
                false,
                true,
                true,
            ),
        ];
        for (name, active, now, wake, cost, work, off, want) in cases {
            assert_eq!(
                between_prompt_should_fire(active, now, last, wake, cost, work, off, FAST, FLOOR),
                want,
                "{name}"
            );
        }
    }

    #[test]
    fn between_prompt_stop_reason_maps_adopted_and_agent_initiated() {
        assert_eq!(between_prompt_stop_reason(false, false), "agent_idle");
        assert_eq!(between_prompt_stop_reason(false, true), "agent_idle");
        assert_eq!(between_prompt_stop_reason(true, true), "prompt_complete");
        assert_eq!(between_prompt_stop_reason(true, false), "reattach_idle");
    }

    #[test]
    fn between_prompt_signal_update_cases() {
        let tracked = |cost_seen, wake_at| {
            Some(BetweenPromptUpdate {
                cost_seen,
                last_lifecycle_at: 500_000,
                wake_at,
            })
        };
        let at = chrono::DateTime::from_timestamp_millis(600_000).unwrap();
        let wake = LifecycleSignal::WakeupPending { at };
        let cases = [
            (
                Some(LifecycleSignal::TerminalUsage),
                None,
                0,
                tracked(true, 0),
            ),
            (Some(LifecycleSignal::Progress), None, 0, tracked(false, 0)),
            (None, None, 42, None),
            (None, Some(wake.clone()), 1_000, tracked(false, 600_000)),
            // A later wake never shortens suppression.
            (None, Some(wake), 900_000, tracked(false, 900_000)),
            // Compaction terminals must not arm a turn; its start does.
            (Some(LifecycleSignal::CompactionFailed), None, 0, None),
            (Some(LifecycleSignal::CompactionCompleted), None, 0, None),
            (
                Some(LifecycleSignal::CompactionStarted),
                None,
                0,
                tracked(false, 0),
            ),
        ];
        for (lifecycle, wakeup, prev_wake, want) in cases {
            assert_eq!(
                between_prompt_signal_update(
                    lifecycle.as_ref(),
                    wakeup.as_ref(),
                    500_000,
                    prev_wake
                ),
                want,
                "{lifecycle:?} {wakeup:?}"
            );
        }
    }

    #[test]
    fn tracker_adopted_turn_accounting_clears_stuck_tool() {
        // #2899: a tool open across the reattach boundary pins the watchdog
        // until the adopted turn's accounting marker drops it.
        let tracker = BetweenPromptTracker::default();
        let claim = TerminalClaim::new();
        let started = LifecycleSignal::ToolStarted {
            id: "t".into(),
            is_background_task: false,
        };
        assert!(tracker.observe(Some(&started), None, 1_000, true, &claim));
        assert!(tracker.work_state().is_busy());
        let usage = LifecycleSignal::TerminalUsage;
        assert!(!tracker.observe(Some(&usage), None, 2_000, true, &claim));
        assert!(!tracker.work_state().is_busy());
        assert_eq!(tracker.take_idle_fire(2_000 + ms(FAST) - 1), None);
        assert_eq!(tracker.take_idle_fire(2_000 + ms(FAST)), Some(true));
        assert_eq!(tracker.take_idle_fire(10_000_000), None);
    }
}
