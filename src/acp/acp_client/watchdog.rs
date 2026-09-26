//! The silent-orphan watchdog: deciding when a turn has gone quiet long
//! enough that the agent is presumed gone.

use crate::acp::agent_profiles;
use agent_client_protocol::schema::v1::SessionUpdate;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::Instant;

use super::lifecycle::{
    classify_lifecycle_signal, wakeup_lifecycle_signal_from_update, LifecycleSignal,
    OffProtocolWorkKind,
};

/// Default silent-orphan grace, mirrored by `AcpConfig`.
const SILENT_ORPHAN_GRACE_DEFAULT: Duration = Duration::from_secs(120);

/// Grace floor for work that continues without ACP progress. Finite so a
/// real wedge still recovers.
pub(super) const OFF_PROTOCOL_WORK_GRACE_FLOOR: Duration = Duration::from_secs(30 * 60);

/// Grace after end-of-turn accounting arrives without a PromptResponse.
const SILENT_ORPHAN_FAST_GRACE_DEFAULT: Duration = Duration::from_secs(20);

const SILENT_ORPHAN_CHECK_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy)]
pub(super) struct SilentOrphanWatchdogConfig {
    pub(super) base_grace: Duration,
    pub(super) fast_grace: Duration,
    pub(super) off_protocol_grace_floor: Duration,
}

/// Per-prompt silent-orphan state machine. Time is injected for tests.
///
/// An open tool call or a future wake always suppresses firing. Off-protocol
/// work uses the grace floor; `cost_seen` switches to the fast grace until the
/// next non-accounting signal clears it.
#[derive(Debug, Default)]
pub(super) struct SilentOrphanWatchdog {
    saw_first_progress: bool,
    last_progress_at: Option<Instant>,
    cost_seen: bool,
    /// Tool id to its `run_in_background` flag.
    tool_calls_in_flight: HashMap<String, bool>,
    off_protocol_work_seen: Option<OffProtocolWorkKind>,
    wakeup_suppress_until: Option<Instant>,
    /// Distinguishes a stream that died mid-message from background work
    /// still producing tool activity.
    last_refresh_was_progress: bool,
}

impl SilentOrphanWatchdog {
    fn refresh(&mut self, now: Instant, was_progress: bool) {
        self.saw_first_progress = true;
        self.last_progress_at = Some(now);
        self.cost_seen = false;
        self.last_refresh_was_progress = was_progress;
    }

    pub(super) fn apply_signal(
        &mut self,
        sig: LifecycleSignal,
        now: Instant,
        wall_now: chrono::DateTime<chrono::Utc>,
        cfg: SilentOrphanWatchdogConfig,
    ) {
        match sig {
            LifecycleSignal::Progress => self.refresh(now, true),
            LifecycleSignal::CompactionStarted => {
                self.refresh(now, true);
                self.off_protocol_work_seen = Some(OffProtocolWorkKind::Compaction);
            }
            LifecycleSignal::CompactionCompleted | LifecycleSignal::CompactionFailed => {
                if self.off_protocol_work_seen == Some(OffProtocolWorkKind::Compaction) {
                    self.off_protocol_work_seen = None;
                }
                self.refresh(now, true);
            }
            LifecycleSignal::ToolStarted {
                id,
                is_background_task,
            } => {
                self.refresh(now, false);
                // OR, so a later `InProgress` without raw_input keeps the flag.
                *self.tool_calls_in_flight.entry(id).or_default() |= is_background_task;
            }
            LifecycleSignal::ToolCompleted {
                id,
                succeeded,
                off_protocol_work,
            } => {
                let started_as_background = self.tool_calls_in_flight.remove(&id).unwrap_or(false);
                self.refresh(now, false);
                // The raw_input flag only counts on success: a failed launch
                // leaves nothing running.
                let kind = off_protocol_work.or((succeeded && started_as_background)
                    .then_some(OffProtocolWorkKind::BackgroundCommand));
                if kind.is_some() {
                    self.off_protocol_work_seen = kind;
                }
            }
            LifecycleSignal::TerminalUsage => {
                self.cost_seen = true;
                // Fire-and-forget commands and compaction are bounded by the
                // turn; async agents and wakes legitimately outlive it.
                if matches!(
                    self.off_protocol_work_seen,
                    Some(OffProtocolWorkKind::BackgroundCommand | OffProtocolWorkKind::Compaction)
                ) {
                    self.off_protocol_work_seen = None;
                }
            }
            LifecycleSignal::WakeupPending { at } => {
                self.refresh(now, false);
                self.off_protocol_work_seen = Some(OffProtocolWorkKind::ScheduledWakeup);
                // Monotonic deadline with the floor as a tail so the agent has
                // room to resume after `at`. Later wakes only extend it.
                let until_wakeup = at
                    .signed_duration_since(wall_now)
                    .to_std()
                    .unwrap_or(Duration::ZERO);
                let deadline = now + until_wakeup + cfg.off_protocol_grace_floor;
                self.wakeup_suppress_until = Some(
                    self.wakeup_suppress_until
                        .map_or(deadline, |existing| existing.max(deadline)),
                );
            }
        }
    }

    pub(super) fn effective_grace(&self, cfg: SilentOrphanWatchdogConfig) -> Duration {
        // A background command whose last refresh was stream output is a dead
        // stream, not a quietly running command (#2645).
        let background_stream_stall = self.off_protocol_work_seen
            == Some(OffProtocolWorkKind::BackgroundCommand)
            && self.last_refresh_was_progress;
        if self.off_protocol_work_seen.is_some() && !background_stream_stall {
            cfg.base_grace.max(cfg.off_protocol_grace_floor)
        } else if self.cost_seen && cfg.fast_grace > Duration::ZERO {
            cfg.fast_grace
        } else {
            cfg.base_grace
        }
    }

    /// Clears an expired wake deadline as a side effect.
    pub(super) fn should_fire(&mut self, now: Instant, cfg: SilentOrphanWatchdogConfig) -> bool {
        if self.wakeup_suppress_until.is_some_and(|d| now >= d) {
            self.wakeup_suppress_until = None;
        }
        self.saw_first_progress
            && self.tool_calls_in_flight.is_empty()
            && self.wakeup_suppress_until.is_none()
            && self
                .last_progress_at
                .is_some_and(|t| now.duration_since(t) >= self.effective_grace(cfg))
    }

    pub(super) fn tool_calls_in_flight_len(&self) -> usize {
        self.tool_calls_in_flight.len()
    }

    pub(super) fn off_protocol_work_seen(&self) -> Option<OffProtocolWorkKind> {
        self.off_protocol_work_seen
    }

    /// End-of-turn accounting arrived and nothing has reset progress since.
    pub(super) fn cost_seen(&self) -> bool {
        self.cost_seen
    }

    pub(super) fn saw_progress(&self) -> bool {
        self.saw_first_progress
    }
}

/// Terminal `Stopped` reason for a prompt turn, highest precedence first.
/// `finished_after_orphan_cancel` (#2370) demotes a premature orphan cancel
/// to `prompt_complete` unless the adapter is still RPC-wedged.
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

/// Debug builds read a millisecond override from `var`.
fn debug_ms_override(var: &str) -> Option<u64> {
    if cfg!(debug_assertions) {
        std::env::var(var).ok()?.parse().ok()
    } else {
        None
    }
}

/// `0` disables the watchdog; other configured values clamp up to 120s.
pub(super) fn silent_orphan_grace(profile: Option<&str>) -> Duration {
    if let Some(ms) = debug_ms_override("AOE_SILENT_ORPHAN_GRACE_MS") {
        return Duration::from_millis(ms);
    }
    let acp = match profile {
        Some(p) => Some(crate::session::config::profile_config::resolve_config_or_warn(p).acp),
        None => crate::session::load_config().ok().flatten().map(|c| c.acp),
    };
    match acp.map(|acp| acp.silent_orphan_grace_secs) {
        Some(0) => Duration::ZERO,
        Some(secs) => Duration::from_secs(u64::from(secs).max(120)),
        None => SILENT_ORPHAN_GRACE_DEFAULT,
    }
}

pub(super) fn silent_orphan_fast_grace() -> Duration {
    match debug_ms_override("AOE_SILENT_ORPHAN_FAST_GRACE_MS") {
        Some(0) => Duration::ZERO,
        Some(ms) => Duration::from_millis(ms.max(100)),
        None => SILENT_ORPHAN_FAST_GRACE_DEFAULT,
    }
}

pub(super) fn silent_orphan_check_interval() -> Duration {
    debug_ms_override("AOE_SILENT_ORPHAN_CHECK_INTERVAL_MS")
        .map_or(SILENT_ORPHAN_CHECK_INTERVAL, |ms| {
            Duration::from_millis(ms.max(10))
        })
}

/// `(lifecycle, wakeup)` signals for a notification; none during post-load
/// history replay so stale frames cannot affect a new prompt.
pub(super) fn classify_watchdog_notification_signals(
    update: &SessionUpdate,
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
    use LifecycleSignal as L;
    use OffProtocolWorkKind as K;

    const S: u64 = 1000;

    const CFG: SilentOrphanWatchdogConfig = SilentOrphanWatchdogConfig {
        base_grace: Duration::from_secs(120),
        fast_grace: Duration::from_secs(20),
        off_protocol_grace_floor: OFF_PROTOCOL_WORK_GRACE_FLOOR,
    };

    fn start(id: &str, bg: bool) -> L {
        L::ToolStarted {
            id: id.into(),
            is_background_task: bg,
        }
    }

    fn done(id: &str, succeeded: bool, kind: Option<K>) -> L {
        L::ToolCompleted {
            id: id.into(),
            succeeded,
            off_protocol_work: kind,
        }
    }

    enum Step {
        /// Apply a signal at a millisecond offset.
        At(u64, L),
        /// At an offset, schedule a wake the given seconds past the wall clock.
        Wake(u64, i64),
        /// Assert `should_fire` at an offset.
        Fires(u64, bool),
        Off(Option<K>),
        Cost(bool),
    }
    use Step::*;

    #[tokio::test]
    async fn watchdog_scenarios() {
        let cases: Vec<(&str, Vec<Step>)> = vec![
            (
                "cost then silence fires on fast grace",
                vec![
                    At(0, L::Progress),
                    At(S, L::TerminalUsage),
                    Fires(25 * S, true),
                    Cost(true),
                    Off(None),
                ],
            ),
            // #2898: compaction holds the floor, not the base grace.
            (
                "compaction uses floor",
                vec![
                    At(0, L::CompactionStarted),
                    Off(Some(K::Compaction)),
                    Fires(120_400, false),
                    Fires(30 * 60 * S + S, true),
                ],
            ),
            (
                "terminal usage clears compaction floor",
                vec![
                    At(0, L::CompactionStarted),
                    At(60 * S, L::TerminalUsage),
                    Off(None),
                    Fires(25 * S, true),
                    Cost(true),
                ],
            ),
            (
                "compaction completed restores base grace",
                vec![
                    At(0, L::CompactionStarted),
                    At(180 * S, L::CompactionCompleted),
                    Off(None),
                    Fires(299 * S, false),
                    Fires(301 * S, true),
                ],
            ),
            (
                "compaction failed clears floor",
                vec![
                    At(0, L::CompactionStarted),
                    At(0, L::CompactionFailed),
                    Off(None),
                ],
            ),
            (
                "wakeup keeps off-protocol after cost",
                vec![
                    At(0, L::Progress),
                    Wake(S, 1),
                    At(2 * S, L::TerminalUsage),
                    Cost(true),
                    Off(Some(K::ScheduledWakeup)),
                ],
            ),
            (
                "progress after terminal usage clears fast grace",
                vec![
                    At(0, L::Progress),
                    At(S, L::TerminalUsage),
                    At(2 * S, L::Progress),
                    Cost(false),
                    Fires(30 * S, false),
                    Fires(60 * S, false),
                    Fires(125 * S, true),
                ],
            ),
            // #1858
            (
                "terminal usage clears background command floor",
                vec![
                    At(0, L::Progress),
                    At(S, start("bg", false)),
                    At(2 * S, done("bg", true, Some(K::BackgroundCommand))),
                    Fires(60 * S, false),
                    At(3 * S, L::TerminalUsage),
                    Off(None),
                    Fires(10 * S, false),
                    Fires(25 * S, true),
                ],
            ),
            (
                "background command after terminal usage rearms floor",
                vec![
                    At(0, L::Progress),
                    At(S, done("a", true, Some(K::BackgroundCommand))),
                    At(2 * S, L::TerminalUsage),
                    Off(None),
                    At(3 * S, L::Progress),
                    At(4 * S, done("b", true, Some(K::BackgroundCommand))),
                    Off(Some(K::BackgroundCommand)),
                    Fires(60 * S, false),
                ],
            ),
            (
                "async agent survives terminal usage",
                vec![
                    At(0, L::Progress),
                    At(S, start("a", false)),
                    At(2 * S, done("a", true, Some(K::AsyncAgent))),
                    At(3 * S, L::TerminalUsage),
                    Fires(60 * S, false),
                    Fires(25 * 60 * S, false),
                ],
            ),
            (
                "raw input background flag alone lifts grace",
                vec![
                    At(0, L::Progress),
                    At(S, start("bg", true)),
                    At(2 * S, done("bg", true, None)),
                    Off(Some(K::BackgroundCommand)),
                    Fires(20 * 60 * S, false),
                ],
            ),
            // #2645
            (
                "background then stream stall recovers on base grace",
                vec![
                    At(0, L::Progress),
                    At(S, done("bg", true, Some(K::BackgroundCommand))),
                    At(2 * S, L::Progress),
                    Off(Some(K::BackgroundCommand)),
                    Fires(60 * S, false),
                    Fires(125 * S, true),
                ],
            ),
            (
                "background still polling rides floor",
                vec![
                    At(0, L::Progress),
                    At(S, done("bg", true, Some(K::BackgroundCommand))),
                    At(2 * S, L::Progress),
                    At(3 * S, start("poll", false)),
                    At(4 * S, done("poll", true, None)),
                    Fires(20 * 60 * S, false),
                ],
            ),
            (
                "async agent stream stall rides floor",
                vec![
                    At(0, L::Progress),
                    At(S, done("a", true, Some(K::AsyncAgent))),
                    At(2 * S, L::Progress),
                    Off(Some(K::AsyncAgent)),
                    Fires(200 * S, false),
                    Fires(25 * 60 * S, false),
                ],
            ),
            (
                "wakeup suppresses until at plus floor",
                vec![
                    At(0, L::Progress),
                    Wake(S, 1),
                    Off(Some(K::ScheduledWakeup)),
                    Fires(2 * S, false),
                    Fires(125 * S, false),
                    Fires(1800 * S, false),
                    Fires(1805 * S, true),
                ],
            ),
            (
                "wakeup after cost does not use fast grace",
                vec![
                    At(0, L::Progress),
                    At(S, L::TerminalUsage),
                    Wake(2 * S, 2),
                    Fires(25 * S, false),
                    Fires(200 * S, false),
                ],
            ),
            (
                "wakeup suppression expires and stays cleared",
                vec![
                    At(0, L::Progress),
                    Wake(S, 1),
                    Fires(3600 * S, true),
                    Fires(3601 * S, true),
                ],
            ),
            (
                "later wakeup extends suppression",
                vec![
                    At(0, L::Progress),
                    Wake(S, 10),
                    Wake(2 * S, 100),
                    Fires(50 * S, false),
                    Fires(1900 * S, false),
                    Fires(1905 * S, true),
                ],
            ),
            (
                "shorter followup wakeup does not shorten",
                vec![
                    At(0, L::Progress),
                    Wake(S, 100),
                    Wake(2 * S, 10),
                    Fires(50 * S, false),
                    Fires(1900 * S, false),
                    Fires(1905 * S, true),
                ],
            ),
            (
                "tool in flight suppresses after terminal usage",
                vec![
                    At(0, L::Progress),
                    At(S, start("t", false)),
                    At(2 * S, L::TerminalUsage),
                    Fires(3600 * S, false),
                ],
            ),
            (
                "no fire without first progress",
                vec![At(S, L::TerminalUsage), Fires(3600 * S, false)],
            ),
            (
                "failed background tool does not suppress",
                vec![
                    At(0, L::Progress),
                    At(S, start("bg", true)),
                    At(2 * S, done("bg", false, None)),
                    Off(None),
                    Fires(125 * S, true),
                ],
            ),
            (
                "later in-progress keeps background flag",
                vec![
                    At(0, L::Progress),
                    At(S, start("bg", true)),
                    At(2 * S, start("bg", false)),
                    At(3 * S, done("bg", true, None)),
                    Off(Some(K::BackgroundCommand)),
                ],
            ),
        ];
        let t0 = Instant::now();
        let wall = chrono::Utc::now();
        for (name, steps) in cases {
            let mut w = SilentOrphanWatchdog::default();
            for step in steps {
                match step {
                    At(ms, sig) => w.apply_signal(sig, t0 + Duration::from_millis(ms), wall, CFG),
                    Wake(ms, secs) => w.apply_signal(
                        L::WakeupPending {
                            at: wall + chrono::Duration::seconds(secs),
                        },
                        t0 + Duration::from_millis(ms),
                        wall,
                        CFG,
                    ),
                    Fires(ms, want) => assert_eq!(
                        w.should_fire(t0 + Duration::from_millis(ms), CFG),
                        want,
                        "{name}: should_fire at {ms}ms"
                    ),
                    Off(want) => assert_eq!(w.off_protocol_work_seen(), want, "{name}"),
                    Cost(want) => assert_eq!(w.cost_seen(), want, "{name}"),
                }
            }
        }

        {
            let t0 = Instant::now();
            let mut w = SilentOrphanWatchdog::default();
            let sig = classify_lifecycle_signal(&text_chunk("Compacting...", Some("m1"))).unwrap();
            w.apply_signal(sig, t0, chrono::Utc::now(), CFG);
            assert!(!w.should_fire(t0 + Duration::from_millis(120_400), CFG));
        }
    }

    #[test]
    fn terminal_stop_reason_precedence() {
        let cases = [
            (
                [false, false, false, false, false, false, false],
                "prompt_complete",
            ),
            ([true, true, true, true, true, true, true], "rate_limited"),
            (
                [false, true, true, false, false, false, false],
                "user_forced",
            ),
            (
                [false, false, true, false, true, false, false],
                "prompt_orphaned",
            ),
            (
                [false, false, false, false, false, true, false],
                "cancelled",
            ),
            // #2370: late cost demotes orphan and cancelled...
            (
                [false, false, true, false, false, true, true],
                "prompt_complete",
            ),
            // ...but not an RPC wedge, a rate limit, or a force stop.
            (
                [false, false, true, true, true, true, true],
                "prompt_orphaned",
            ),
            (
                [true, false, true, false, false, true, true],
                "rate_limited",
            ),
            ([false, true, true, false, false, true, true], "user_forced"),
        ];
        for ([rl, fs, po, au, sd, pc, fin], want) in cases {
            assert_eq!(terminal_stop_reason(rl, fs, po, au, sd, pc, fin), want);
        }
    }

    #[test]
    fn classify_watchdog_notification_signals_cases() {
        use agent_client_protocol::schema::v1::{
            AvailableCommand, AvailableCommandsUpdate, ToolCallUpdate, ToolCallUpdateFields,
        };
        let ambient = SessionUpdate::AvailableCommandsUpdate(AvailableCommandsUpdate::new(vec![
            AvailableCommand::new("review", "Review changes"),
        ]));
        let tool =
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new("tc-1", ToolCallUpdateFields::new()));
        for (update, suppressing, want_lifecycle) in [
            (&ambient, false, false),
            (&tool, false, true),
            (&tool, true, false),
        ] {
            let (lifecycle, wakeup) = classify_watchdog_notification_signals(
                update,
                &agent_profiles::CLAUDE,
                suppressing,
            );
            assert_eq!(lifecycle.is_some(), want_lifecycle);
            assert!(wakeup.is_none());
        }
    }
}
