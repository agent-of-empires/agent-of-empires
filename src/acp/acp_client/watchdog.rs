//! The silent-orphan watchdog: deciding when a turn has gone quiet long
//! enough that the agent is presumed gone.

use crate::acp::agent_profiles;

use super::connection::resolved_acp_config;
use super::lifecycle::{
    classify_lifecycle_signal, wakeup_lifecycle_signal_from_update, LifecycleSignal,
    OffProtocolWorkKind,
};
use super::tool_context::ToolMetadata;

/// Default silent-orphan grace, mirrored by `AcpConfig`.
pub(super) const SILENT_ORPHAN_GRACE_DEFAULT: std::time::Duration =
    std::time::Duration::from_secs(120);

/// Grace floor for work that continues without ACP progress, such as an async
/// agent or background command. It is finite so a real wedge still recovers.
pub(super) const OFF_PROTOCOL_WORK_GRACE_FLOOR: std::time::Duration =
    std::time::Duration::from_secs(30 * 60);

/// Short grace after end-of-turn accounting arrives without PromptResponse.
pub(super) const SILENT_ORPHAN_FAST_GRACE_DEFAULT: std::time::Duration =
    std::time::Duration::from_secs(20);

/// Polling keeps timer ownership in the prompt loop.
pub(super) const SILENT_ORPHAN_CHECK_INTERVAL: std::time::Duration =
    std::time::Duration::from_secs(5);

/// Runtime configuration handed to `SilentOrphanWatchdog::should_fire`
/// and `apply_signal`. Decoupled from `AcpConfig` and the env-var
/// overrides so unit tests can drive synthetic graces deterministically
/// without touching process-global state.
#[derive(Debug, Clone, Copy)]
pub(super) struct SilentOrphanWatchdogConfig {
    pub(super) base_grace: std::time::Duration,
    pub(super) fast_grace: std::time::Duration,
    pub(super) off_protocol_grace_floor: std::time::Duration,
}

/// Per-prompt silent-orphan state machine. Time is injected for deterministic
/// tests.
///
/// Invariants:
///
/// - `tool_calls_in_flight` non-empty → watchdog is always suppressed.
/// - off-protocol work uses its grace floor, except a background-command stream
///   that visibly stalled;
/// - a future wake suppresses the watchdog;
/// - `cost_seen` switches the no-off-protocol case to fast grace; any
///   subsequent `Progress` / `ToolStarted` / `ToolCompleted` /
///   `WakeupPending` clears it.
#[derive(Debug, Default)]
pub(super) struct SilentOrphanWatchdog {
    saw_first_progress: bool,
    last_progress_at: Option<tokio::time::Instant>,
    cost_seen: bool,
    tool_calls_in_flight: std::collections::HashMap<String, ToolMetadata>,
    off_protocol_work_seen: Option<OffProtocolWorkKind>,
    wakeup_suppress_until: Option<tokio::time::Instant>,
    /// Distinguishes a stream that died mid-message from background work still
    /// producing tool activity.
    last_refresh_was_progress: bool,
}

impl SilentOrphanWatchdog {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// Fold a lifecycle signal into the state machine. Called once per
    /// signal received from the notification handler's mpsc.
    pub(super) fn apply_signal(
        &mut self,
        sig: LifecycleSignal,
        now: tokio::time::Instant,
        wall_now: chrono::DateTime<chrono::Utc>,
        cfg: SilentOrphanWatchdogConfig,
    ) {
        match sig {
            LifecycleSignal::Progress => {
                self.saw_first_progress = true;
                self.last_progress_at = Some(now);
                self.cost_seen = false;
                self.last_refresh_was_progress = true;
            }
            LifecycleSignal::CompactionStarted => {
                // Treat the "Compacting..." marker as progress for timer
                // purposes, then latch the off-protocol floor so the quiet
                // summarization window that follows is not read as a wedge.
                // See #2898.
                self.saw_first_progress = true;
                self.last_progress_at = Some(now);
                self.cost_seen = false;
                self.last_refresh_was_progress = true;
                self.off_protocol_work_seen = Some(OffProtocolWorkKind::Compaction);
            }
            // A compaction that failed or was cancelled is just as over as
            // one that completed, so it drops the suppression the same way.
            // Before this, only the success marker cleared the latch, so a
            // cancelled compaction left the 30-minute floor pinned for the
            // rest of the prompt.
            LifecycleSignal::CompactionCompleted | LifecycleSignal::CompactionFailed => {
                // Compaction finished; drop its suppression so any continued
                // work in the same turn recovers on the normal grace. Guard
                // the clear to Compaction so a completion marker cannot erase
                // an unrelated off-protocol kind. See #2898.
                if self.off_protocol_work_seen == Some(OffProtocolWorkKind::Compaction) {
                    self.off_protocol_work_seen = None;
                }
                self.saw_first_progress = true;
                self.last_progress_at = Some(now);
                self.cost_seen = false;
                self.last_refresh_was_progress = true;
            }
            LifecycleSignal::ToolStarted {
                id,
                is_background_task,
            } => {
                self.saw_first_progress = true;
                self.last_progress_at = Some(now);
                self.cost_seen = false;
                self.last_refresh_was_progress = false;
                // OR the new flag with any existing metadata. A late
                // `ToolCallUpdate(InProgress)` lacks `raw_input` and
                // classifies as `is_background_task = false`; without
                // the OR it would erase the `true` captured from the
                // original `ToolCall` and break the raw-input arm of
                // the defense-in-depth detection. See #1401 review.
                self.tool_calls_in_flight
                    .entry(id)
                    .and_modify(|m| m.is_background_task |= is_background_task)
                    .or_insert(ToolMetadata { is_background_task });
            }
            LifecycleSignal::ToolCompleted {
                id,
                succeeded,
                off_protocol_work,
            } => {
                let started_as_background = self
                    .tool_calls_in_flight
                    .remove(&id)
                    .map(|m| m.is_background_task)
                    .unwrap_or(false);
                self.saw_first_progress = true;
                self.last_progress_at = Some(now);
                self.cost_seen = false;
                self.last_refresh_was_progress = false;
                // Defense in depth: trust either the completion-content
                // marker OR the original raw_input flag. Either path
                // alone is enough to mark this prompt as having
                // off-protocol work pending. The raw-input fallback is
                // ONLY trusted on successful completion: a Failed
                // backgrounded Bash means the subprocess never
                // actually started, so suppressing for 30 minutes
                // would create a fresh false-positive class. See
                // #1401 and the post-impl review notes.
                let kind = off_protocol_work.or({
                    if succeeded && started_as_background {
                        Some(OffProtocolWorkKind::BackgroundCommand)
                    } else {
                        None
                    }
                });
                if let Some(kind) = kind {
                    self.off_protocol_work_seen = Some(kind);
                }
            }
            LifecycleSignal::TerminalUsage => {
                self.cost_seen = true;
                // A cost-resolved UsageUpdate is the end-of-turn marker
                // (mid-turn usages carry `cost: null`, see #1360). A
                // backgrounded command is fire-and-forget: the agent
                // launches it and moves on, so it legitimately outlives
                // the turn and its suppression is moot once the turn
                // ends. Drop it here, otherwise `effective_grace` keeps
                // the 30-minute floor and a turn that streamed its final
                // usage but never returned the PromptResponse hangs for
                // half an hour instead of recovering on the fast grace
                // (#1858). Self-correcting: if the turn somehow
                // continues, the next Progress / ToolStarted /
                // ToolCompleted clears `cost_seen` and the next
                // backgrounded tool re-arms suppression. An AsyncAgent
                // await or a ScheduledWakeup blocks the turn (the agent
                // idles waiting and resumes in-band), so their floor is
                // left intact to preserve the #1360 fix and the monitor
                // fix; only the fire-and-forget BackgroundCommand drops.
                //
                // Compaction is likewise turn-bounded: the summarization
                // call ends before the final accounting frame, so dropping
                // it here lets a lost PromptResponse after `/compact` recover
                // on the fast grace instead of holding the floor for 30
                // minutes. See #2898.
                if matches!(
                    self.off_protocol_work_seen,
                    Some(OffProtocolWorkKind::BackgroundCommand | OffProtocolWorkKind::Compaction)
                ) {
                    self.off_protocol_work_seen = None;
                }
            }
            LifecycleSignal::WakeupPending { at } => {
                self.saw_first_progress = true;
                self.last_progress_at = Some(now);
                self.cost_seen = false;
                self.last_refresh_was_progress = false;
                // A scheduled wake is deliberate off-protocol idling, not
                // a wedge: mark the turn so the fast grace (cost_seen)
                // never applies and the post-`at` grace is the generous
                // 30-minute off-protocol floor. Overwrite any prior kind
                // so a later `TerminalUsage` cannot clear it (only
                // `BackgroundCommand` is dropped there). Without this a
                // monitor / `/loop` turn that emitted a cost-bearing
                // `UsageUpdate` was killed ~20s after the wake window
                // lapsed even though the agent intended to keep going.
                self.off_protocol_work_seen = Some(OffProtocolWorkKind::ScheduledWakeup);
                // Convert the wall-clock `at` to a monotonic `Instant`
                // deadline now, so wall-clock jumps between signal
                // receipt and the next firing check can't perturb
                // suppression. Add the off-protocol floor as a tail so
                // the watchdog doesn't snap-fire the instant the sleep
                // ends; the agent needs room after `at` to emit the
                // wake's first progress, and a monitor whose wake `at`
                // is itself further out than the floor stays suppressed
                // the whole time. See #1401 and the monitor regression.
                let until_wakeup = at
                    .signed_duration_since(wall_now)
                    .to_std()
                    .unwrap_or(std::time::Duration::ZERO);
                let deadline = now + until_wakeup + cfg.off_protocol_grace_floor;
                // Multiple wakeups should EXTEND (not shorten)
                // suppression. The agent may re-issue a longer
                // ScheduleWakeup mid-turn; only the later deadline
                // wins.
                self.wakeup_suppress_until = Some(
                    self.wakeup_suppress_until
                        .map_or(deadline, |existing| existing.max(deadline)),
                );
            }
        }
    }

    pub(super) fn effective_grace(&self, cfg: SilentOrphanWatchdogConfig) -> std::time::Duration {
        // A backgrounded Bash whose turn then died mid-message: the last
        // signal that refreshed the timer was model stream output
        // (`Progress`), not a `BashOutput` poll or other tool activity, and
        // nothing has arrived since. That is a dead stream, not a quietly-
        // running bash, so bypass the 30-min floor and recover on the normal
        // per-prompt cascade (~120s base grace). A bash still being polled
        // refreshes the timer via tool activity, which clears
        // `last_refresh_was_progress` and keeps the floor. Scoped to
        // `BackgroundCommand`: an `AsyncAgent` await and a `ScheduledWakeup`
        // are genuinely invisible off-protocol waits and keep their floor
        // (preserves #1360 and the monitor-killed-by-watchdog fix). See #2645.
        let background_stream_stall = self.off_protocol_work_seen
            == Some(OffProtocolWorkKind::BackgroundCommand)
            && self.last_refresh_was_progress;
        if self.off_protocol_work_seen.is_some() && !background_stream_stall {
            cfg.base_grace.max(cfg.off_protocol_grace_floor)
        } else if self.cost_seen && cfg.fast_grace > std::time::Duration::ZERO {
            cfg.fast_grace
        } else {
            cfg.base_grace
        }
    }

    /// Returns `true` iff the watchdog must fire now. Also clears any
    /// expired `wakeup_suppress_until` deadline as a side effect so
    /// subsequent ticks don't re-evaluate stale state.
    pub(super) fn should_fire(
        &mut self,
        now: tokio::time::Instant,
        cfg: SilentOrphanWatchdogConfig,
    ) -> bool {
        if self
            .wakeup_suppress_until
            .is_some_and(|deadline| now >= deadline)
        {
            self.wakeup_suppress_until = None;
        }
        let wakeup_suppressed = self
            .wakeup_suppress_until
            .is_some_and(|deadline| now < deadline);
        let elapsed = self.last_progress_at.map(|t| now.duration_since(t));
        self.saw_first_progress
            && self.tool_calls_in_flight.is_empty()
            && !wakeup_suppressed
            && elapsed
                .map(|d| d >= self.effective_grace(cfg))
                .unwrap_or(false)
    }

    pub(super) fn tool_calls_in_flight_len(&self) -> usize {
        self.tool_calls_in_flight.len()
    }

    pub(super) fn off_protocol_work_seen(&self) -> Option<OffProtocolWorkKind> {
        self.off_protocol_work_seen
    }

    /// True once a cost-populated `UsageUpdate` (the end-of-turn
    /// accounting marker) has arrived and nothing has reset progress
    /// since. At watchdog-fire time this means the turn demonstrably
    /// wrapped up but the adapter never sent the JSON-RPC PromptResponse,
    /// so the right recovery is a clean `prompt_complete`, not a
    /// cancel-and-restart orphan. See #2237.
    pub(super) fn cost_seen(&self) -> bool {
        self.cost_seen
    }

    pub(super) fn saw_progress(&self) -> bool {
        self.saw_first_progress
    }
}

/// Resolve the terminal `Stopped` reason for a prompt turn from the
/// mutually-prioritised end-of-turn flags. Extracted as a pure function
/// so the precedence is unit-testable without the connection loop.
///
/// Precedence (highest first) and why each wins where it does is
/// documented inline at the single call site. The finished-but-unacked
/// recovery (#2237) deliberately sets NONE of these flags and breaks the
/// loop, so it falls through to `prompt_complete`: the turn finished, the
/// adapter just never sent the PromptResponse, so it must NOT collapse
/// into `prompt_orphaned` (which would trigger a worker restart). Its
/// post-cancel sibling `finished_after_orphan_cancel` (#2370) covers the
/// case where the cost marker arrives only AFTER the grace already expired
/// and the orphan cancel fired: the cancel was premature, so the turn is
/// demoted back to `prompt_complete` unless the adapter is still wedged on
/// the RPC (`agent_unresponsive` / `shutdown`), which keeps the restart.
pub(super) fn terminal_stop_reason(
    rate_limited: bool,
    force_stopped: bool,
    prompt_orphaned: bool,
    agent_unresponsive: bool,
    shutdown: bool,
    prompt_cancelled: bool,
    finished_after_orphan_cancel: bool,
) -> &'static str {
    if rate_limited {
        "rate_limited"
    } else if force_stopped {
        "user_forced"
    } else if finished_after_orphan_cancel && !agent_unresponsive && !shutdown {
        // A cost-populated UsageUpdate that lands after a timer-driven orphan
        // cancel proves the turn actually finished; the cancel was premature.
        // End cleanly as prompt_complete (no worker restart, no "didn't notify
        // the daemon" banner), the post-cancel sibling of the pre-grace #2237
        // recovery. The cancel makes the adapter resolve as
        // StopReason::Cancelled, so this must outrank both prompt_orphaned and
        // prompt_cancelled. A genuinely RPC-wedged adapter (cancel-escalation
        // grace fired, so agent_unresponsive / shutdown) is excluded here and
        // still restarts the worker, since the connection may be unusable for
        // the next prompt. See #2370.
        "prompt_complete"
    } else if prompt_orphaned {
        "prompt_orphaned"
    } else if agent_unresponsive {
        "agent_unresponsive"
    } else if shutdown {
        "shutdown"
    } else if prompt_cancelled {
        "cancelled"
    } else {
        "prompt_complete"
    }
}

/// Read the silent-orphan watchdog grace for the given source profile.
/// In debug builds, honors `AOE_SILENT_ORPHAN_GRACE_MS` so the
/// integration test can drive a sub-second cadence without making
/// real failures racy. Otherwise reads
/// `acp.silent_orphan_grace_secs` from the profile-resolved
/// config so per-profile overrides set in the settings TUI take
/// effect. A value of `0` means "disabled" and the caller skips the
/// watchdog entirely; non-zero values smaller than 120s clamp up at
/// runtime to the new production floor so a typo cannot produce an
/// absurdly tight grace that false-positives on healthy turns. The
/// floor was raised from 10s to 120s in #1360 alongside the default
/// bump from 60 to 120; users who explicitly want a shorter grace
/// must set `0` to disable instead.
pub(super) fn silent_orphan_grace(profile: Option<&str>) -> std::time::Duration {
    #[cfg(debug_assertions)]
    if let Ok(raw) = std::env::var("AOE_SILENT_ORPHAN_GRACE_MS") {
        if let Ok(ms) = raw.parse::<u64>() {
            if ms == 0 {
                return std::time::Duration::ZERO;
            }
            return std::time::Duration::from_millis(ms);
        }
    }
    match resolved_acp_config(profile) {
        Some(acp) => {
            let secs = acp.silent_orphan_grace_secs;
            if secs == 0 {
                std::time::Duration::ZERO
            } else {
                std::time::Duration::from_secs(u64::from(secs).max(120))
            }
        }
        None => SILENT_ORPHAN_GRACE_DEFAULT,
    }
}

/// Accelerated silent-orphan grace: `SILENT_ORPHAN_FAST_GRACE_DEFAULT`
/// in production, overridable via env var in debug builds so the
/// integration tests can exercise the accelerator without real
/// 20-second waits.
pub(super) fn silent_orphan_fast_grace() -> std::time::Duration {
    #[cfg(debug_assertions)]
    if let Ok(raw) = std::env::var("AOE_SILENT_ORPHAN_FAST_GRACE_MS") {
        if let Ok(ms) = raw.parse::<u64>() {
            if ms == 0 {
                return std::time::Duration::ZERO;
            }
            return std::time::Duration::from_millis(ms.max(100));
        }
    }
    SILENT_ORPHAN_FAST_GRACE_DEFAULT
}

/// Read the silent-orphan polling cadence. Constant in production;
/// tunable in debug builds via `AOE_SILENT_ORPHAN_CHECK_INTERVAL_MS`
/// so the disabled-path integration test can verify the watchdog
/// stays silent without waiting a full polling tick.
pub(super) fn silent_orphan_check_interval() -> std::time::Duration {
    #[cfg(debug_assertions)]
    if let Ok(raw) = std::env::var("AOE_SILENT_ORPHAN_CHECK_INTERVAL_MS") {
        if let Ok(ms) = raw.parse::<u64>() {
            return std::time::Duration::from_millis(ms.max(10));
        }
    }
    SILENT_ORPHAN_CHECK_INTERVAL
}

/// Classify a notification for both watchdog lanes. Returns
/// `(lifecycle_signal, wakeup_signal)`.
///
/// During post-load history replay suppression we intentionally surface no
/// signal, so stale replay frames cannot suppress or disarm watchdogs for a
/// new prompt epoch.
pub(super) fn classify_watchdog_notification_signals(
    update: &agent_client_protocol::schema::v1::SessionUpdate,
    profile: &agent_profiles::AgentProfile,
    suppressing_history_replay: bool,
) -> (Option<LifecycleSignal>, Option<LifecycleSignal>) {
    if suppressing_history_replay {
        return (None, None);
    }
    (
        classify_lifecycle_signal(update),
        wakeup_lifecycle_signal_from_update(update, profile),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::acp_client::test_helpers::text_chunk;
    use agent_client_protocol::schema::v1::SessionUpdate;

    // -------------------------------------------------------------------
    // SilentOrphanWatchdog: pure-state-machine unit tests
    //
    // The watchdog used to live inline in the prompt loop, where the only
    // way to verify behavior was through real `tokio::time::sleep` and
    // the integration shim. After #1401 the state machine is a free-
    // standing struct that takes synthetic `Instant` / `DateTime<Utc>`
    // inputs, so these tests can step the clock forward in microseconds
    // without flakiness. The covered shapes deliberately overlap the
    // production false-positive class so a regression would be caught
    // before it ever reached the shim.
    // -------------------------------------------------------------------

    fn watchdog_test_cfg() -> SilentOrphanWatchdogConfig {
        SilentOrphanWatchdogConfig {
            base_grace: std::time::Duration::from_secs(120),
            fast_grace: std::time::Duration::from_secs(20),
            off_protocol_grace_floor: std::time::Duration::from_secs(30 * 60),
        }
    }

    #[tokio::test]
    async fn watchdog_fires_on_cost_then_silence() {
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        w.apply_signal(
            LifecycleSignal::TerminalUsage,
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        // Fast grace is 20s; 25s after the last progress with cost_seen
        // and no in-flight work must fire.
        assert!(w.should_fire(t0 + std::time::Duration::from_secs(25), cfg));
    }

    // #2898: /compact emits one "Compacting..." chunk then runs a long
    // silent summarization call. Pre-fix that chunk classified as Progress,
    // so the watchdog fired at the 120s base grace and cancelled the
    // compaction. CompactionStarted must latch the off-protocol floor so a
    // large compaction is never cut short, while still recovering on a true
    // hang past the floor.
    #[tokio::test]
    async fn watchdog_compaction_uses_floor_not_base_grace() {
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::CompactionStarted, t0, wall, cfg);
        assert_eq!(
            w.off_protocol_work_seen(),
            Some(OffProtocolWorkKind::Compaction)
        );
        // The exact failure timing from the issue: 120.4s of silence.
        assert!(
            !w.should_fire(t0 + std::time::Duration::from_millis(120_400), cfg),
            "compaction must not be cancelled at the base grace"
        );
        // Still finite: a genuinely wedged compaction recovers past the floor.
        assert!(
            w.should_fire(t0 + std::time::Duration::from_secs(30 * 60 + 1), cfg),
            "a hung compaction must eventually recover"
        );
    }

    // #2898 reproduce-then-fix: drive the REAL classifier so this test
    // compiles and runs on the pre-fix tree too. Pre-fix, "Compacting..."
    // classifies as Progress and the watchdog fires at the 120s base grace
    // (RED). Post-fix it classifies as CompactionStarted and survives (GREEN).
    #[tokio::test]
    async fn watchdog_does_not_cancel_compaction_via_classifier() {
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        let sig = classify_lifecycle_signal(&text_chunk("Compacting...", Some("m1")))
            .expect("compact chunk must classify");
        w.apply_signal(sig, t0, wall, cfg);
        assert!(
            !w.should_fire(t0 + std::time::Duration::from_millis(120_400), cfg),
            "compaction must survive past the base grace"
        );
    }

    // #2898: dropping Compaction on TerminalUsage preserves the #2237
    // finished-but-unacked fast-grace recovery. Without the drop a lost
    // PromptResponse after /compact would hold the 30-min floor.
    #[tokio::test]
    async fn watchdog_terminal_usage_clears_compaction_floor() {
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::CompactionStarted, t0, wall, cfg);
        w.apply_signal(
            LifecycleSignal::TerminalUsage,
            t0 + std::time::Duration::from_secs(60),
            wall,
            cfg,
        );
        assert!(
            w.off_protocol_work_seen().is_none(),
            "TerminalUsage must clear the compaction floor"
        );
        // Fast grace (20s) now governs, keyed off the last progress at t0.
        // 25s in it fires, and the #2237 clean-completion guard applies
        // because cost_seen holds with no off-protocol work.
        assert!(w.should_fire(t0 + std::time::Duration::from_secs(25), cfg));
        assert!(w.cost_seen());
    }

    // #2898: the explicit completion marker clears the floor even without a
    // cost frame, so continued work in the same turn recovers on the normal
    // grace rather than the 30-min floor.
    #[tokio::test]
    async fn watchdog_compaction_completed_restores_base_grace() {
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::CompactionStarted, t0, wall, cfg);
        let done = t0 + std::time::Duration::from_secs(180);
        w.apply_signal(LifecycleSignal::CompactionCompleted, done, wall, cfg);
        assert!(
            w.off_protocol_work_seen().is_none(),
            "completion marker must clear the compaction floor"
        );
        // Base grace (120s) governs from the completion timestamp.
        assert!(!w.should_fire(done + std::time::Duration::from_secs(119), cfg));
        assert!(w.should_fire(done + std::time::Duration::from_secs(121), cfg));
    }

    // #2237: when the watchdog fires on a turn that already emitted its
    // cost-populated end-of-turn UsageUpdate (and no off-protocol work),
    // the prompt loop ends the turn cleanly instead of cancel+restart.
    // The decision keys on cost_seen() + off_protocol_work_seen(); guard
    // both so the clean-completion branch is reachable only in that exact
    // shape.
    #[tokio::test]
    async fn watchdog_cost_seen_marks_completed_unacked_path() {
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        w.apply_signal(
            LifecycleSignal::TerminalUsage,
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        let fire_at = t0 + std::time::Duration::from_secs(25);
        assert!(w.should_fire(fire_at, cfg));
        // The clean-completion branch is gated on both signals.
        assert!(w.cost_seen(), "cost marker must be observable at fire");
        assert!(
            w.off_protocol_work_seen().is_none(),
            "no off-protocol work, so clean completion (not the monitor floor) applies"
        );
    }

    #[tokio::test]
    async fn watchdog_off_protocol_keeps_orphan_path_even_with_cost() {
        // A backgrounded command before the cost marker is dropped by
        // TerminalUsage (#1858), so cost_seen + no off-protocol holds and
        // the clean path applies. But an async-agent / scheduled wakeup is
        // NOT dropped, so those keep off_protocol set and must stay on the
        // orphan path. Lock that distinction down.
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        w.apply_signal(
            LifecycleSignal::WakeupPending {
                at: wall + chrono::Duration::seconds(1),
            },
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        // Now apply the end-of-turn cost marker. TerminalUsage sets
        // cost_seen, but (unlike a backgrounded command, #1858) a scheduled
        // wakeup is NOT dropped, so off-protocol work stays set. This is the
        // "with cost" case the test name promises: the clean-completion guard
        // (cost_seen && off_protocol none) is still false, so a scheduled-wake
        // turn keeps the orphan path even once its cost usage lands.
        w.apply_signal(
            LifecycleSignal::TerminalUsage,
            t0 + std::time::Duration::from_secs(2),
            wall,
            cfg,
        );
        assert!(w.cost_seen());
        assert!(w.off_protocol_work_seen().is_some());
    }

    // #2237: the finished-but-unacked recovery breaks with no flag set, so
    // it must fall through to prompt_complete (the non-restart reason), NOT
    // prompt_orphaned. Guard the all-false fall-through here; the watchdog
    // test above guards the branch condition (cost_seen + no off-protocol).
    #[test]
    fn terminal_stop_reason_all_false_is_prompt_complete() {
        assert_eq!(
            terminal_stop_reason(false, false, false, false, false, false, false),
            "prompt_complete"
        );
    }

    #[test]
    fn terminal_stop_reason_precedence_is_preserved() {
        // rate_limited wins over everything.
        assert_eq!(
            terminal_stop_reason(true, true, true, true, true, true, true),
            "rate_limited"
        );
        // force_stopped beats a prompt_orphaned flag set earlier.
        assert_eq!(
            terminal_stop_reason(false, true, true, false, false, false, false),
            "user_forced"
        );
        // prompt_orphaned (genuine wedge) wins over a later shutdown/cancel.
        assert_eq!(
            terminal_stop_reason(false, false, true, false, true, false, false),
            "prompt_orphaned"
        );
        assert_eq!(
            terminal_stop_reason(false, false, false, false, false, true, false),
            "cancelled"
        );
    }

    #[test]
    fn terminal_stop_reason_late_cost_demotes_orphan_and_cancelled() {
        // #2370: the orphan cancel fired on the timer, the adapter then
        // resolved the prompt as Cancelled (because we cancelled), but a
        // cost-populated UsageUpdate proved the turn finished. The demotion
        // must outrank BOTH prompt_orphaned and prompt_cancelled.
        assert_eq!(
            terminal_stop_reason(false, false, true, false, false, true, true),
            "prompt_complete"
        );
    }

    #[test]
    fn terminal_stop_reason_late_cost_does_not_mask_rpc_wedge_or_user_intent() {
        // #2370: a cost frame proves the model work finished, but if the
        // adapter is still wedged on the RPC (cancel-escalation grace fired)
        // the worker connection may be unusable for the next prompt, so the
        // genuine-wedge restart must survive.
        assert_eq!(
            terminal_stop_reason(false, false, true, true, true, true, true),
            "prompt_orphaned"
        );
        // rate_limited and user force_stopped still win over the demotion.
        assert_eq!(
            terminal_stop_reason(true, false, true, false, false, true, true),
            "rate_limited"
        );
        assert_eq!(
            terminal_stop_reason(false, true, true, false, false, true, true),
            "user_forced"
        );
    }

    #[tokio::test]
    async fn watchdog_progress_after_terminal_usage_clears_fast_grace() {
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        w.apply_signal(
            LifecycleSignal::TerminalUsage,
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        // A later Progress event must clear cost_seen so the fast grace
        // no longer applies. The watchdog now waits for the full base
        // grace (120s) from the latest progress.
        w.apply_signal(
            LifecycleSignal::Progress,
            t0 + std::time::Duration::from_secs(2),
            wall,
            cfg,
        );
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(30), cfg));
        // Still must not fire well past the old fast grace window.
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(60), cfg));
        // And must eventually fire after the full base grace.
        assert!(w.should_fire(t0 + std::time::Duration::from_secs(125), cfg));
    }

    #[tokio::test]
    async fn watchdog_cost_seen_clears_when_progress_resumes() {
        // #2370: finished_after_orphan_cancel reads cost_seen() directly to
        // demote a premature orphan cancel. If a cost marker arrives but the
        // turn then resumes real work, cost_seen() must flip back to false so a
        // turn that did NOT finish is never demoted to prompt_complete.
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        w.apply_signal(
            LifecycleSignal::TerminalUsage,
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        assert!(
            w.cost_seen(),
            "cost marker must be observable after TerminalUsage"
        );
        w.apply_signal(
            LifecycleSignal::Progress,
            t0 + std::time::Duration::from_secs(2),
            wall,
            cfg,
        );
        assert!(
            !w.cost_seen(),
            "progress after the cost marker means the turn resumed; must not demote"
        );
    }

    #[tokio::test]
    async fn watchdog_terminal_usage_clears_background_command_suppression() {
        // Regression for #1858. A backgrounded command lifts the grace to
        // the 30-minute off-protocol floor mid-turn (so a legit `cmd &`
        // is not killed), but a backgrounded command is fire-and-forget
        // and outlives the turn. Once the cost-resolved UsageUpdate
        // (TerminalUsage, the end-of-turn marker) arrives, the floor must
        // drop so a turn that streamed its final usage but never returned
        // the PromptResponse recovers on the fast grace instead of
        // hanging for 30 minutes.
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        // Tool started without the background flag.
        w.apply_signal(
            LifecycleSignal::ToolStarted {
                id: "tc-bg-1".into(),
                is_background_task: false,
            },
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        // Completion content carries the background marker.
        w.apply_signal(
            LifecycleSignal::ToolCompleted {
                id: "tc-bg-1".into(),
                succeeded: true,
                off_protocol_work: Some(OffProtocolWorkKind::BackgroundCommand),
            },
            t0 + std::time::Duration::from_secs(2),
            wall,
            cfg,
        );
        // Before the terminal usage the floor holds: 60s in must not fire.
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(60), cfg));
        w.apply_signal(
            LifecycleSignal::TerminalUsage,
            t0 + std::time::Duration::from_secs(3),
            wall,
            cfg,
        );
        // TerminalUsage cleared the background-command suppression.
        assert!(w.off_protocol_work_seen().is_none());
        // Now the fast grace (20s) applies, measured from the last
        // progress at t0+2s. Inside the window: no fire.
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(10), cfg));
        // Past the fast grace (elapsed 23s > 20s): the wedge recovers
        // instead of waiting out the 30-minute floor.
        assert!(w.should_fire(t0 + std::time::Duration::from_secs(25), cfg));
    }

    #[tokio::test]
    async fn watchdog_terminal_usage_then_background_command_rearms_floor() {
        // Self-correction: TerminalUsage clearing background suppression
        // must not be permanent. If activity resumes after the terminal
        // usage (cost_seen flips false on Progress) and a new backgrounded
        // command completes, the off-protocol floor re-arms.
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        w.apply_signal(
            LifecycleSignal::ToolCompleted {
                id: "tc-bg-a".into(),
                succeeded: true,
                off_protocol_work: Some(OffProtocolWorkKind::BackgroundCommand),
            },
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        w.apply_signal(
            LifecycleSignal::TerminalUsage,
            t0 + std::time::Duration::from_secs(2),
            wall,
            cfg,
        );
        assert!(w.off_protocol_work_seen().is_none());
        // Turn continues: more progress, then another backgrounded tool.
        w.apply_signal(
            LifecycleSignal::Progress,
            t0 + std::time::Duration::from_secs(3),
            wall,
            cfg,
        );
        w.apply_signal(
            LifecycleSignal::ToolCompleted {
                id: "tc-bg-b".into(),
                succeeded: true,
                off_protocol_work: Some(OffProtocolWorkKind::BackgroundCommand),
            },
            t0 + std::time::Duration::from_secs(4),
            wall,
            cfg,
        );
        assert_eq!(
            w.off_protocol_work_seen(),
            Some(OffProtocolWorkKind::BackgroundCommand),
            "a fresh backgrounded command after terminal usage must re-arm the floor",
        );
        // Floor is back: must not fire well past the fast grace.
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(60), cfg));
    }

    #[tokio::test]
    async fn watchdog_async_agent_lifts_grace_above_fast_grace() {
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        w.apply_signal(
            LifecycleSignal::ToolStarted {
                id: "tc-async-1".into(),
                is_background_task: false,
            },
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        w.apply_signal(
            LifecycleSignal::ToolCompleted {
                id: "tc-async-1".into(),
                succeeded: true,
                off_protocol_work: Some(OffProtocolWorkKind::AsyncAgent),
            },
            t0 + std::time::Duration::from_secs(2),
            wall,
            cfg,
        );
        w.apply_signal(
            LifecycleSignal::TerminalUsage,
            t0 + std::time::Duration::from_secs(3),
            wall,
            cfg,
        );
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(60), cfg));
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(60 * 25), cfg));
    }

    #[tokio::test]
    async fn watchdog_background_via_raw_input_lifts_grace_without_content_marker() {
        // Defense in depth: even if the completion content marker is
        // missing (SDK string drift, content stripped), the
        // `is_background_task` flag captured at ToolStarted should
        // still flip `off_protocol_work_seen`.
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        w.apply_signal(
            LifecycleSignal::ToolStarted {
                id: "tc-bg-2".into(),
                is_background_task: true,
            },
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        w.apply_signal(
            LifecycleSignal::ToolCompleted {
                id: "tc-bg-2".into(),
                succeeded: true,
                off_protocol_work: None,
            },
            t0 + std::time::Duration::from_secs(2),
            wall,
            cfg,
        );
        assert_eq!(
            w.off_protocol_work_seen(),
            Some(OffProtocolWorkKind::BackgroundCommand),
            "raw_input.run_in_background must trip off-protocol suppression alone"
        );
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(60 * 20), cfg));
    }

    #[tokio::test]
    async fn watchdog_background_then_stream_stall_recovers_on_base_grace() {
        // #2645: a per-prompt turn launched a backgrounded Bash (latches the
        // 30-min floor) and then streamed a partial message before the model
        // stream died mid-chunk. Because the last timer refresh was a
        // `Progress` (not a `BashOutput` poll), the watchdog must recover on
        // the normal per-prompt grace (~120s), not ride the 30-min floor.
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        w.apply_signal(
            LifecycleSignal::ToolCompleted {
                id: "tc-bg-stall".into(),
                succeeded: true,
                off_protocol_work: Some(OffProtocolWorkKind::BackgroundCommand),
            },
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        // Model resumes streaming a partial message, then the stream dies.
        w.apply_signal(
            LifecycleSignal::Progress,
            t0 + std::time::Duration::from_secs(2),
            wall,
            cfg,
        );
        assert_eq!(
            w.off_protocol_work_seen(),
            Some(OffProtocolWorkKind::BackgroundCommand),
            "the backgrounded Bash is still latched",
        );
        // Before the base grace lapses: still suppressed.
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(60), cfg));
        // Past the base grace (120s from the last chunk at +2s): fires,
        // instead of waiting the 30-min floor.
        assert!(w.should_fire(t0 + std::time::Duration::from_secs(125), cfg));
    }

    #[tokio::test]
    async fn watchdog_background_still_polling_rides_floor() {
        // #2645 guard: a backgrounded Bash that is genuinely still producing
        // output is polled via `BashOutput`, which surfaces as tool activity
        // (ToolStarted / ToolCompleted) and clears `last_refresh_was_progress`.
        // The watchdog must keep the 30-min floor so a live bash is not cut
        // short even though a stream chunk preceded the last poll.
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        w.apply_signal(
            LifecycleSignal::ToolCompleted {
                id: "tc-bg-live".into(),
                succeeded: true,
                off_protocol_work: Some(OffProtocolWorkKind::BackgroundCommand),
            },
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        // Agent narrates ("still running..."), then polls the bash.
        w.apply_signal(
            LifecycleSignal::Progress,
            t0 + std::time::Duration::from_secs(2),
            wall,
            cfg,
        );
        w.apply_signal(
            LifecycleSignal::ToolStarted {
                id: "tc-bashoutput".into(),
                is_background_task: false,
            },
            t0 + std::time::Duration::from_secs(3),
            wall,
            cfg,
        );
        w.apply_signal(
            LifecycleSignal::ToolCompleted {
                id: "tc-bashoutput".into(),
                succeeded: true,
                off_protocol_work: None,
            },
            t0 + std::time::Duration::from_secs(4),
            wall,
            cfg,
        );
        // Last refresh was tool activity: the floor holds, no early fire.
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(60 * 20), cfg));
    }

    #[tokio::test]
    async fn watchdog_async_agent_stream_stall_still_rides_floor() {
        // #2645 scope lock: the mid-stream-stall bypass is BackgroundCommand
        // only. An AsyncAgent await is a genuinely invisible off-protocol
        // wait, so even a stream chunk followed by silence must keep the
        // 30-min floor (preserves #1360 and the monitor-kill fix).
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        w.apply_signal(
            LifecycleSignal::ToolCompleted {
                id: "tc-async".into(),
                succeeded: true,
                off_protocol_work: Some(OffProtocolWorkKind::AsyncAgent),
            },
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        w.apply_signal(
            LifecycleSignal::Progress,
            t0 + std::time::Duration::from_secs(2),
            wall,
            cfg,
        );
        assert_eq!(
            w.off_protocol_work_seen(),
            Some(OffProtocolWorkKind::AsyncAgent),
        );
        // Well past the base grace: still suppressed on the 30-min floor.
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(200), cfg));
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(60 * 25), cfg));
    }

    #[tokio::test]
    async fn watchdog_wakeup_suppresses_until_at_plus_off_protocol_floor() {
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        // Schedule wakeup 1 second in the future.
        w.apply_signal(
            LifecycleSignal::WakeupPending {
                at: wall + chrono::Duration::seconds(1),
            },
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        // A scheduled wake marks the turn as deliberate off-protocol
        // idling; the fast grace must never apply to it.
        assert_eq!(
            w.off_protocol_work_seen(),
            Some(OffProtocolWorkKind::ScheduledWakeup)
        );
        // At the wakeup `at` itself: suppressed.
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(2), cfg));
        // Well past the old 120s base-grace tail: the monitor turn must
        // still be suppressed now that the tail is the 30-minute
        // off-protocol floor (regression: a monitor used to die ~125s in).
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(125), cfg));
        // Just inside `at + floor` (≈1802s): still suppressed.
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(1800), cfg));
        // Past `at + floor`: watchdog finally rearms (transport-wedge
        // backstop). elapsed since last progress (1805s) > floor (1800s).
        assert!(w.should_fire(t0 + std::time::Duration::from_secs(1805), cfg));
    }

    #[tokio::test]
    async fn watchdog_wakeup_after_cost_does_not_use_fast_grace() {
        // Regression for the monitor-killed-by-watchdog bug: a `/loop`
        // turn emits a cost-bearing `UsageUpdate` (cost_seen → fast
        // grace) and then schedules a wake. Before the fix the watchdog
        // fired ~20s after the wake window lapsed; now the scheduled-wake
        // off-protocol mark must override fast grace entirely.
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        // Terminal accounting frame arrives first (flips cost_seen).
        w.apply_signal(
            LifecycleSignal::TerminalUsage,
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        // Then the agent schedules a wake 2s out.
        w.apply_signal(
            LifecycleSignal::WakeupPending {
                at: wall + chrono::Duration::seconds(2),
            },
            t0 + std::time::Duration::from_secs(2),
            wall,
            cfg,
        );
        // 25s in (well past the 20s fast grace): must NOT fire.
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(25), cfg));
        // 200s in (past the old 120s base grace too): still suppressed.
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(200), cfg));
    }

    #[tokio::test]
    async fn watchdog_wakeup_suppression_eventually_expires() {
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        w.apply_signal(
            LifecycleSignal::WakeupPending {
                at: wall + chrono::Duration::seconds(1),
            },
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        // Step very far past the deadline; should_fire clears the
        // deadline as a side effect and rearms the watchdog.
        assert!(w.should_fire(t0 + std::time::Duration::from_secs(60 * 60), cfg));
        // A subsequent check at any later time without new progress
        // must still fire (the deadline was cleared, so suppression
        // does not re-engage).
        assert!(w.should_fire(t0 + std::time::Duration::from_secs(60 * 60 + 1), cfg));
    }

    #[tokio::test]
    async fn watchdog_later_wakeup_extends_not_shortens_suppression() {
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        // First wakeup: 10s out.
        w.apply_signal(
            LifecycleSignal::WakeupPending {
                at: wall + chrono::Duration::seconds(10),
            },
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        // Second wakeup: 100s out (further). The deadline must extend.
        w.apply_signal(
            LifecycleSignal::WakeupPending {
                at: wall + chrono::Duration::seconds(100),
            },
            t0 + std::time::Duration::from_secs(2),
            wall,
            cfg,
        );
        // At t0 + 50s: the first wakeup's tail (10s + 1800s) is still
        // alive, AND the second wakeup's tail (100s + 1800s = 1902s)
        // is alive. Watchdog must be suppressed by the larger.
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(50), cfg));
        // At t0 + 1900s: still inside the second wakeup's tail (1902s).
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(1900), cfg));
        // At t0 + 1905s: past the second wakeup's tail.
        assert!(w.should_fire(t0 + std::time::Duration::from_secs(1905), cfg));
    }

    #[tokio::test]
    async fn watchdog_shorter_followup_wakeup_does_not_shorten_suppression() {
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        // First wakeup: far in the future.
        w.apply_signal(
            LifecycleSignal::WakeupPending {
                at: wall + chrono::Duration::seconds(100),
            },
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        // Second wakeup: closer (should NOT shorten suppression).
        w.apply_signal(
            LifecycleSignal::WakeupPending {
                at: wall + chrono::Duration::seconds(10),
            },
            t0 + std::time::Duration::from_secs(2),
            wall,
            cfg,
        );
        // First wakeup's tail (100s + 1800s = 1901s) still wins.
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(50), cfg));
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(1900), cfg));
        assert!(w.should_fire(t0 + std::time::Duration::from_secs(1905), cfg));
    }

    #[tokio::test]
    async fn watchdog_tool_in_flight_suppresses_even_after_terminal_usage() {
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        w.apply_signal(
            LifecycleSignal::ToolStarted {
                id: "tc-1".into(),
                is_background_task: false,
            },
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        w.apply_signal(
            LifecycleSignal::TerminalUsage,
            t0 + std::time::Duration::from_secs(2),
            wall,
            cfg,
        );
        // Tool still in flight: watchdog never fires regardless of grace.
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(60 * 60), cfg));
    }

    #[tokio::test]
    async fn watchdog_does_not_fire_without_first_progress() {
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        // No Progress signal ever received: watchdog stays disarmed.
        w.apply_signal(
            LifecycleSignal::TerminalUsage,
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        assert!(!w.should_fire(t0 + std::time::Duration::from_secs(60 * 60), cfg));
    }

    /// A cancelled or failed compaction is as over as a completed one, so
    /// it must drop the 30-minute off-protocol floor. Before this only the
    /// success marker cleared it.
    #[tokio::test]
    async fn compaction_failure_clears_the_off_protocol_floor() {
        let cfg = watchdog_test_cfg();
        for terminal in [
            LifecycleSignal::CompactionCompleted,
            LifecycleSignal::CompactionFailed,
        ] {
            let t0 = tokio::time::Instant::now();
            let wall = chrono::Utc::now();
            let mut w = SilentOrphanWatchdog::new();
            w.apply_signal(LifecycleSignal::CompactionStarted, t0, wall, cfg);
            assert_eq!(
                w.off_protocol_work_seen(),
                Some(OffProtocolWorkKind::Compaction),
                "the start marker must latch the floor"
            );
            w.apply_signal(terminal.clone(), t0, wall, cfg);
            assert_eq!(w.off_protocol_work_seen(), None, "{terminal:?}");
        }
    }

    #[test]
    fn classify_watchdog_notification_signals_ignores_ambient_updates() {
        use agent_client_protocol::schema::v1::{
            AvailableCommand as AcpAvailableCommand, AvailableCommandsUpdate,
        };
        let update = SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![
            AcpAvailableCommand::new("review", "Review changes"),
        ]));
        let (lifecycle, wakeup) =
            classify_watchdog_notification_signals(&update, &agent_profiles::CLAUDE, false);
        assert!(
            lifecycle.is_none() && wakeup.is_none(),
            "ambient updates must not count as watchdog activity"
        );
    }

    #[test]
    fn classify_watchdog_notification_signals_marks_lifecycle_updates() {
        use agent_client_protocol::schema::v1::{ToolCallUpdate, ToolCallUpdateFields};
        let update = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "tc-lifecycle-1",
            ToolCallUpdateFields::new(),
        ));
        let (lifecycle, wakeup) =
            classify_watchdog_notification_signals(&update, &agent_profiles::CLAUDE, false);
        assert!(
            lifecycle.is_some() && wakeup.is_none(),
            "tool lifecycle updates must disarm the resume-idle watchdog"
        );
    }

    #[test]
    fn classify_watchdog_notification_signals_suppresses_during_history_replay() {
        use agent_client_protocol::schema::v1::{ToolCallUpdate, ToolCallUpdateFields};
        let update = SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "tc-suppressed-1",
            ToolCallUpdateFields::new(),
        ));
        let (lifecycle, wakeup) =
            classify_watchdog_notification_signals(&update, &agent_profiles::CLAUDE, true);
        assert!(
            lifecycle.is_none() && wakeup.is_none(),
            "post-load replay suppression must block watchdog signals"
        );
    }

    #[tokio::test]
    async fn watchdog_failed_background_tool_does_not_suppress() {
        // Regression for the post-impl review: a backgrounded Bash that
        // FAILS to launch (e.g., binary not found, raw_input parse
        // error) must not spuriously enable off-protocol suppression
        // via the raw_input fallback. The subprocess never actually
        // started, so the watchdog must keep its base grace.
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        w.apply_signal(
            LifecycleSignal::ToolStarted {
                id: "tc-bg-fail".into(),
                is_background_task: true,
            },
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        w.apply_signal(
            LifecycleSignal::ToolCompleted {
                id: "tc-bg-fail".into(),
                succeeded: false,
                off_protocol_work: None,
            },
            t0 + std::time::Duration::from_secs(2),
            wall,
            cfg,
        );
        assert!(
            w.off_protocol_work_seen().is_none(),
            "Failed background tool must not enable off-protocol suppression",
        );
        // Watchdog uses base grace (120s) and fires after it elapses.
        assert!(w.should_fire(t0 + std::time::Duration::from_secs(125), cfg));
    }

    #[tokio::test]
    async fn watchdog_later_in_progress_does_not_clobber_background_flag() {
        // Regression for the post-impl review: claude-agent-acp emits a
        // `ToolCall` carrying `raw_input.run_in_background` followed by
        // a `ToolCallUpdate { status: InProgress }` that lacks raw_input
        // and classifies as `is_background_task: false`. A blind
        // `insert` would overwrite the sticky `true` and silently
        // disable the raw-input arm of the defense-in-depth detection.
        let cfg = watchdog_test_cfg();
        let t0 = tokio::time::Instant::now();
        let wall = chrono::Utc::now();
        let mut w = SilentOrphanWatchdog::new();
        w.apply_signal(LifecycleSignal::Progress, t0, wall, cfg);
        w.apply_signal(
            LifecycleSignal::ToolStarted {
                id: "tc-bg-3".into(),
                is_background_task: true,
            },
            t0 + std::time::Duration::from_secs(1),
            wall,
            cfg,
        );
        // Later InProgress update with the same id but no background flag.
        w.apply_signal(
            LifecycleSignal::ToolStarted {
                id: "tc-bg-3".into(),
                is_background_task: false,
            },
            t0 + std::time::Duration::from_secs(2),
            wall,
            cfg,
        );
        // Completion without content marker: only the raw-input flag is
        // available, and it must still be `true` after the InProgress
        // re-stamp.
        w.apply_signal(
            LifecycleSignal::ToolCompleted {
                id: "tc-bg-3".into(),
                succeeded: true,
                off_protocol_work: None,
            },
            t0 + std::time::Duration::from_secs(3),
            wall,
            cfg,
        );
        assert_eq!(
            w.off_protocol_work_seen(),
            Some(OffProtocolWorkKind::BackgroundCommand),
            "background flag must survive an intervening ToolStarted without the flag",
        );
    }
}
