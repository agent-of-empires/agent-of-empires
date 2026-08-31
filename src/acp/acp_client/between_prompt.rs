//! Between-prompt idle detection: deciding when a turn that produced no
//! terminal update has nonetheless finished.

use super::lifecycle::LifecycleSignal;

/// Grace for an agent-initiated turn after its end-of-turn accounting marker.
pub(super) const BETWEEN_PROMPT_IDLE_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// Grace for an agent-initiated turn that stalled without accounting or a
/// scheduled wake. Progress continually resets the timer.
pub(super) const BETWEEN_PROMPT_STALL_GRACE: std::time::Duration =
    std::time::Duration::from_secs(120);

/// Faster cadence used only while the command loop is between prompts.
pub(super) const BETWEEN_PROMPT_IDLE_CHECK_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(1);

/// Decide whether the between-prompt idle watchdog should synthesize a
/// terminal `Stopped` for an agent-initiated turn that ran with no
/// aoe-issued `session/prompt`. A claude-code Monitor (or any backgrounded
/// task) can fire AFTER the prompt that armed it already completed,
/// resuming the agent into a fresh turn the per-prompt watchdog never
/// saw; without this the turn never ends and the UI stays "running"
/// forever. See #2325.
///
/// Pure so the precedence is unit-testable without the connection loop.
/// Times are wall-clock millis (`chrono::Utc::now().timestamp_millis()`),
/// matching the resume-idle watchdog. Mirrors the per-prompt watchdog's
/// grace policy: the cost-bearing `UsageUpdate` is claude-agent-acp's
/// end-of-turn marker, so once it has arrived the fast grace applies;
/// untracked off-protocol work (backgrounded Bash) holds the 30-minute
/// floor; a turn that streamed but did neither (a stalled stream) recovers
/// on the intermediate stall grace instead of the floor (#2573). A pending
/// scheduled wake (`wake_at` in the future) suppresses firing so a
/// legitimately-sleeping monitor is never killed early; once `wake_at` is
/// in the past the turn is treated as finished and self-heals fast (#2371).
/// State update the between-prompt watchdog should apply for one inbound
/// notification's classified signals. `None` when neither a lifecycle nor a
/// wakeup signal is present (ambient updates do not touch the watchdog).
///
/// Extracted as a pure function so the cost / progress / wake bookkeeping is
/// unit-testable without the notification closure. Every tracked signal
/// refreshes `last_lifecycle_at` to `now_ms`, including `TerminalUsage`: the
/// cost-bearing `UsageUpdate` is the end-of-turn marker, and the fast grace
/// must measure from it (when the turn wrapped up) rather than from a
/// possibly-stale earlier progress event. See #2325.
#[derive(Debug, PartialEq)]
pub(super) struct BetweenPromptUpdate {
    pub(super) cost_seen: bool,
    pub(super) last_lifecycle_at: i64,
    /// Absolute wake timestamp (ms) of the most recent pending scheduled
    /// wake, `0` when none. The watchdog suppresses while `now < wake_at`
    /// (a future wake the agent is legitimately sleeping toward) and, once
    /// the wake `at` has passed with no agent resume, treats the turn as
    /// finished and fires on the fast grace instead of the 30-minute floor.
    /// Stored as the wake `at` directly (not `at + floor`) so an expired
    /// wake self-heals promptly. See #2371.
    pub(super) wake_at: i64,
}

pub(super) fn between_prompt_signal_update(
    lifecycle: Option<&LifecycleSignal>,
    wakeup: Option<&LifecycleSignal>,
    now_ms: i64,
    prev_wake_at: i64,
) -> Option<BetweenPromptUpdate> {
    let mut update = match lifecycle {
        Some(LifecycleSignal::TerminalUsage) => Some(BetweenPromptUpdate {
            cost_seen: true,
            last_lifecycle_at: now_ms,
            wake_at: prev_wake_at,
        }),
        // The tail end of a compaction, not the start of anything. The
        // adapter emits the failure marker AFTER the turn's own terminal
        // when the user cancels a `/compact`, so arming here claimed a
        // turn that had already ended and left the session Running for the
        // whole stall grace with nothing running. A compaction that is
        // genuinely still going has already armed on its start marker.
        Some(LifecycleSignal::CompactionCompleted | LifecycleSignal::CompactionFailed) => None,
        Some(_) => Some(BetweenPromptUpdate {
            cost_seen: false,
            last_lifecycle_at: now_ms,
            wake_at: prev_wake_at,
        }),
        None => None,
    };
    // A scheduled wake (a re-armed monitor or /loop fallback) suppresses
    // firing until its `at`. Multiple wakes extend, never shorten,
    // suppression, so keep the latest deadline.
    if let Some(LifecycleSignal::WakeupPending { at }) = wakeup {
        update = Some(BetweenPromptUpdate {
            cost_seen: false,
            last_lifecycle_at: now_ms,
            wake_at: at.timestamp_millis().max(prev_wake_at),
        });
    }
    update
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

pub(super) fn between_prompt_work_state(
    tools: &std::sync::Mutex<std::collections::HashMap<String, bool>>,
    background_agents: &std::sync::Mutex<std::collections::HashSet<String>>,
) -> BetweenPromptWorkState {
    let tool_calls = !tools
        .lock()
        .expect("between-prompt tools mutex poisoned")
        .is_empty();
    let background_agents = !background_agents
        .lock()
        .expect("between-prompt bg-agents mutex poisoned")
        .is_empty();
    BetweenPromptWorkState {
        tool_calls,
        background_agents,
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
    fast_grace: std::time::Duration,
    floor: std::time::Duration,
) -> bool {
    if !active {
        return false;
    }
    // Work in flight (an open ACP tool: npm install, Playwright, a Task
    // subagent; or a tracked async background agent) means the turn is
    // legitimately busy; never fire while any is running. See #1401, #2573.
    if work_in_flight {
        return false;
    }
    // A future wake is the agent legitimately sleeping toward `at`; suppress
    // until it passes so a parked /loop or monitor is not killed early.
    if wake_at_ms.is_some_and(|at| now_ms < at) {
        return false;
    }
    let expired_wake = wake_at_ms.is_some_and(|at| now_ms >= at);
    // Off-protocol work (backgrounded Bash) completes on the protocol while
    // the real work keeps running with no completion signal, so hold the
    // conservative floor even though no tool is "in flight". See #1401,
    // #1858. A cost-resolved end-of-turn marker OR an expired wake (the agent
    // should have resumed and did not) means the turn is done: self-heal
    // fast. Otherwise the turn streamed but never finished cleanly or parked
    // a wake, a stalled stream: recover on the stall grace (minutes) rather
    // than the 30-minute floor. See #2573.
    let grace = if off_protocol_work_seen {
        floor
    } else if cost_seen || expired_wake {
        fast_grace
    } else {
        BETWEEN_PROMPT_STALL_GRACE
    };
    now_ms - last_lifecycle_ms >= grace.as_millis() as i64
}

/// Terminal `Stopped` reason for a between-prompt idle-watchdog fire.
///
/// An adopted turn (`Resume { in_flight_turn: true }`, #2899) has no owning
/// `prompt_fut`, so this watchdog emits its terminal event. If it reached its
/// cost-populated end-of-turn `UsageUpdate` (`cost_seen`), the turn completed
/// cleanly: emit `prompt_complete`, the reason the owning connection would have
/// emitted, which stays out of the supervisor's kill+respawn set so a pending
/// build respawn proceeds on the idle boundary. If it fired without the cost
/// marker, the adopted turn stalled mid-stream: route to `reattach_idle` for
/// recovery. A non-adopted agent-initiated turn (Monitor / scheduled wake, #2325)
/// keeps `agent_idle`.
pub(super) fn between_prompt_stop_reason(adopted: bool, cost_seen: bool) -> &'static str {
    match (adopted, cost_seen) {
        (false, _) => "agent_idle",
        (true, true) => "prompt_complete",
        (true, false) => "reattach_idle",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::acp_client::watchdog::OFF_PROTOCOL_WORK_GRACE_FLOOR;

    // Between-prompt idle watchdog fire decision (#2325). Wall-clock millis.
    // Bind to the production constants so the test tracks the real grace.
    const FAST: std::time::Duration = BETWEEN_PROMPT_IDLE_GRACE;

    const FLOOR: std::time::Duration = OFF_PROTOCOL_WORK_GRACE_FLOOR;

    const STALL: std::time::Duration = BETWEEN_PROMPT_STALL_GRACE;

    #[test]
    fn between_prompt_inactive_never_fires() {
        // No agent-initiated turn tracked, even long past any grace.
        assert!(!between_prompt_should_fire(
            false, 10_000_000, 0, None, true, false, false, FAST, FLOOR
        ));
    }

    #[test]
    fn between_prompt_fires_after_fast_grace_when_cost_seen() {
        let last = 1_000_000;
        let grace_ms = FAST.as_millis() as i64;
        // Just under the fast grace: still waiting.
        assert!(!between_prompt_should_fire(
            true,
            last + grace_ms - 500,
            last,
            None,
            true,
            false,
            false,
            FAST,
            FLOOR
        ));
        // Past the fast grace: the completed turn ends.
        assert!(between_prompt_should_fire(
            true,
            last + grace_ms + 500,
            last,
            None,
            true,
            false,
            false,
            FAST,
            FLOOR
        ));
    }

    #[test]
    fn between_prompt_suppressed_while_work_in_flight() {
        // #2573: a tracked async background agent is folded into the
        // work_in_flight input, so it must suppress the idle watchdog even
        // when the grace has long elapsed and a cost frame was seen. Before
        // the fix the call site passed only ACP tools, so a running bg agent
        // left this false and the watchdog fired mid-work.
        let last = 1_000_000;
        let well_past = last + FLOOR.as_millis() as i64 + 10_000;
        assert!(!between_prompt_should_fire(
            true, well_past, last, None, true, true, false, FAST, FLOOR
        ));
        // Once the work drains (set empty -> work_in_flight false), the
        // already-elapsed grace lets the completed turn end on the next tick.
        assert!(between_prompt_should_fire(
            true, well_past, last, None, true, false, false, FAST, FLOOR
        ));
    }

    #[test]
    fn between_prompt_stalled_stream_fires_on_stall_grace() {
        // #2573: a turn that streamed but never reported a cost marker, never
        // scheduled a wake, and is not off-protocol is a stalled stream. It
        // must recover on the stall grace (minutes), not the 30-minute floor.
        let last = 1_000_000;
        let stall_ms = STALL.as_millis() as i64;
        // Under the stall grace: normal inter-chunk gap, no fire.
        assert!(!between_prompt_should_fire(
            true,
            last + stall_ms - 1000,
            last,
            None,
            false,
            false,
            false,
            FAST,
            FLOOR
        ));
        // Past the stall grace: recover. Before the fix this waited the full
        // 30-minute floor, so the session sat "running" for half an hour.
        assert!(between_prompt_should_fire(
            true,
            last + stall_ms + 1000,
            last,
            None,
            false,
            false,
            false,
            FAST,
            FLOOR
        ));
    }

    #[test]
    fn between_prompt_stop_reason_maps_adopted_and_agent_initiated() {
        // #2899: a non-adopted agent-initiated turn (Monitor / scheduled wake,
        // #2325) keeps agent_idle regardless of the cost marker.
        assert_eq!(between_prompt_stop_reason(false, false), "agent_idle");
        assert_eq!(between_prompt_stop_reason(false, true), "agent_idle");
        // An adopted turn that reached its cost-populated end-of-turn
        // UsageUpdate completed cleanly: prompt_complete, which stays out of
        // the supervisor's kill+respawn set so a pending build respawn proceeds.
        assert_eq!(between_prompt_stop_reason(true, true), "prompt_complete");
        // An adopted turn that fired without the cost marker stalled
        // mid-stream: route to reattach_idle for recovery, NOT a false
        // prompt_complete that would tell the supervisor the turn succeeded.
        assert_eq!(between_prompt_stop_reason(true, false), "reattach_idle");
    }

    #[test]
    fn between_prompt_adopted_turn_completes_after_terminal_barrier_clears_stuck_tool() {
        // #2899: a tool in flight across the adopt boundary leaks a stuck
        // between_prompt_tools entry (its terminal frame went to the old
        // connection), pinning work_in_flight true so the watchdog never fires
        // and the session sits "Running" forever. The cost-populated
        // end-of-turn UsageUpdate barrier clears that stale bookkeeping; the
        // adopted turn then ends cleanly as prompt_complete.
        let last = 1_000_000;
        let past = last + FAST.as_millis() as i64 + 500;
        // Stuck tool present -> work_in_flight true -> suppressed forever (bug).
        assert!(!between_prompt_should_fire(
            true, past, last, None, true, true, false, FAST, FLOOR
        ));
        // Barrier cleared the stale tools -> work_in_flight false -> fires.
        assert!(between_prompt_should_fire(
            true, past, last, None, true, false, false, FAST, FLOOR
        ));
        // The adopted, cost-seen completion is labeled prompt_complete.
        assert_eq!(between_prompt_stop_reason(true, true), "prompt_complete");
    }

    #[test]
    fn between_prompt_off_protocol_still_uses_floor() {
        // Untracked backgrounded Bash (off_protocol_work_seen) has no
        // completion signal, so it keeps the conservative 30-minute floor
        // even well past the stall grace. See #1401, #1858, #2573.
        let last = 1_000_000;
        assert!(!between_prompt_should_fire(
            true,
            last + STALL.as_millis() as i64 + 60_000,
            last,
            None,
            false,
            false,
            true, // off_protocol_work_seen
            FAST,
            FLOOR
        ));
        assert!(between_prompt_should_fire(
            true,
            last + FLOOR.as_millis() as i64 + 1,
            last,
            None,
            false,
            false,
            true,
            FAST,
            FLOOR
        ));
    }

    #[test]
    fn between_prompt_suppressed_while_future_wake_pending() {
        // A future wake (#1401): the agent is deliberately asleep toward `at`.
        // Suppressed even long past the floor.
        let last = 1_000_000;
        let now = last + 60_000;
        let wake_at = now + 5_000; // still in the future relative to `now`
        assert!(!between_prompt_should_fire(
            true,
            now,
            last,
            Some(wake_at),
            false,
            false,
            false,
            FAST,
            FLOOR
        ));
    }

    #[test]
    fn between_prompt_expired_wake_fires_on_fast_grace() {
        // The #2371 bug: the agent scheduled a wake for `wake_at`, kept working
        // past it, then went quiet without resuming. Once the wake `at` is in
        // the past and no tool / off-protocol work protects the turn, it must
        // self-heal on the fast grace, NOT the 30-minute floor.
        let last = 1_000_000;
        let wake_at = last - 10_000; // already expired when the turn went quiet
                                     // Just under the fast grace: still waiting.
        assert!(!between_prompt_should_fire(
            true,
            last + FAST.as_millis() as i64 - 500,
            last,
            Some(wake_at),
            false,
            false,
            false,
            FAST,
            FLOOR
        ));
        // Past the fast grace: fire, instead of holding the floor for 30 min.
        assert!(between_prompt_should_fire(
            true,
            last + FAST.as_millis() as i64 + 500,
            last,
            Some(wake_at),
            false,
            false,
            false,
            FAST,
            FLOOR
        ));
    }

    #[test]
    fn between_prompt_expired_wake_suppressed_while_tool_in_flight() {
        // Wake expired AND a tool is still open (a fired wake that resumed and
        // launched a long tool): never fire while a tool runs. Preserves #1401.
        let last = 1_000_000;
        let wake_at = last - 10_000;
        assert!(!between_prompt_should_fire(
            true,
            last + 60_000,
            last,
            Some(wake_at),
            false,
            true, // tool in flight
            false,
            FAST,
            FLOOR
        ));
    }

    #[test]
    fn between_prompt_expired_wake_with_off_protocol_holds_floor() {
        // Wake expired and no tool open, but a backgrounded Bash / async agent
        // is still running off-protocol: hold the floor, do not fast-fire and
        // kill the background work. Preserves #1401 / #1858.
        let last = 1_000_000;
        let wake_at = last - 10_000;
        // 21s in: fast grace would have fired, but off-protocol holds the floor.
        assert!(!between_prompt_should_fire(
            true,
            last + 21_000,
            last,
            Some(wake_at),
            false,
            false,
            true, // off-protocol work latched
            FAST,
            FLOOR
        ));
        // Past the floor it still fires (the floor, not a permanent pin).
        assert!(between_prompt_should_fire(
            true,
            last + 30 * 60 * 1000 + 1,
            last,
            Some(wake_at),
            false,
            false,
            true,
            FAST,
            FLOOR
        ));
    }

    #[test]
    fn between_prompt_signal_update_terminal_usage_refreshes_timestamp() {
        // TerminalUsage marks cost_seen AND refreshes last_lifecycle_at to
        // `now`, so the fast grace measures from the cost marker, not a
        // stale earlier progress event. See #2325 review.
        let u =
            between_prompt_signal_update(Some(&LifecycleSignal::TerminalUsage), None, 500_000, 0)
                .expect("TerminalUsage is a tracked signal");
        assert_eq!(
            u,
            BetweenPromptUpdate {
                cost_seen: true,
                last_lifecycle_at: 500_000,
                wake_at: 0,
            }
        );
    }

    #[test]
    fn between_prompt_signal_update_progress_clears_cost_and_refreshes() {
        let u = between_prompt_signal_update(Some(&LifecycleSignal::Progress), None, 500_000, 0)
            .expect("Progress is a tracked signal");
        assert_eq!(
            u,
            BetweenPromptUpdate {
                cost_seen: false,
                last_lifecycle_at: 500_000,
                wake_at: 0,
            }
        );
    }

    #[test]
    fn between_prompt_signal_update_ambient_is_none() {
        // No lifecycle and no wakeup signal: ambient update, no state change.
        assert!(between_prompt_signal_update(None, None, 500_000, 42).is_none());
    }

    #[test]
    fn between_prompt_signal_update_wakeup_stores_at_and_never_shortens() {
        let at = chrono::DateTime::from_timestamp_millis(600_000).unwrap();
        // The wake `at` is stored directly (not `at + floor`), so an expired
        // wake can self-heal on the fast grace. See #2371.
        let u = between_prompt_signal_update(
            None,
            Some(&LifecycleSignal::WakeupPending { at }),
            500_000,
            1_000,
        )
        .expect("WakeupPending is a tracked signal");
        assert_eq!(
            u,
            BetweenPromptUpdate {
                cost_seen: false,
                last_lifecycle_at: 500_000,
                wake_at: 600_000,
            }
        );
        // A later pending wake does not shorten suppression: keep the max.
        let u2 = between_prompt_signal_update(
            None,
            Some(&LifecycleSignal::WakeupPending { at }),
            500_000,
            900_000,
        )
        .unwrap();
        assert_eq!(u2.wake_at, 900_000);
    }

    #[test]
    fn between_prompt_stale_progress_plus_cost_marker_does_not_fire_early() {
        // Regression for the state-update path (#2325 review): a progress
        // event 10s ago, then a cost-bearing UsageUpdate now. The cost marker
        // refreshes last_lifecycle_at, so 2s later (under the 3s grace) the
        // watchdog must NOT fire even though cost_seen is true and the prior
        // progress is older than the grace.
        let cost_now = 1_000_000;
        let stale_progress = cost_now - 10_000;
        let u =
            between_prompt_signal_update(Some(&LifecycleSignal::TerminalUsage), None, cost_now, 0)
                .unwrap();
        // The refresh, not the stale progress, governs the grace window.
        assert_eq!(u.last_lifecycle_at, cost_now);
        assert_ne!(u.last_lifecycle_at, stale_progress);
        assert!(!between_prompt_should_fire(
            true,
            cost_now + 2_000,
            u.last_lifecycle_at,
            None,
            u.cost_seen,
            false,
            false,
            FAST,
            FLOOR,
        ));
        // After the full grace it does fire.
        assert!(between_prompt_should_fire(
            true,
            cost_now + FAST.as_millis() as i64 + 1,
            u.last_lifecycle_at,
            None,
            u.cost_seen,
            false,
            false,
            FAST,
            FLOOR,
        ));
    }

    /// The reported bug: on a cancel the adapter emits its
    /// "Compacting failed" marker AFTER the turn's own terminal, so the
    /// between-prompt watchdog claimed a turn that had already ended and
    /// the session read Running until the stall grace expired.
    #[test]
    fn compaction_terminals_do_not_arm_the_between_prompt_watchdog() {
        // (signal, arms a between-prompt turn)
        let cases = [
            (LifecycleSignal::CompactionFailed, false),
            (LifecycleSignal::CompactionCompleted, false),
            // A compaction genuinely starting between prompts is real work
            // and still has to claim a terminal.
            (LifecycleSignal::CompactionStarted, true),
            (LifecycleSignal::Progress, true),
        ];
        for (sig, expected_arm) in cases {
            let armed = between_prompt_signal_update(Some(&sig), None, 1_000, 0).is_some();
            assert_eq!(armed, expected_arm, "{sig:?}");
        }
    }
}
