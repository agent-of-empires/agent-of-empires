//! Server-owned prompt dispatch: whether an incoming prompt is sent now,
//! steered into the running turn, or parked on the server queue.

use super::state::AcpState;

/// What the daemon decided to do with a prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "disposition")]
pub enum PromptDispatch {
    /// No turn in flight: start one.
    Sent,
    /// A steerable turn is running and will take this mid-turn
    /// (`_session/steering`) rather than refusing it.
    Steered,
    /// Park it on the server-owned queue; the turn-end drain delivers it.
    Queued { reason: QueueReason },
}

/// Why a prompt was parked.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueReason {
    /// A non-steerable turn is running.
    TurnActive,
    /// A cancel is pending on the running turn.
    Cancelling,
    /// A `/compact` is running.
    Compacting,
    /// No live worker, and not a case this POST would wake (idle dormancy or a
    /// rate-limit park).
    WorkerDown,
}

/// Worker-liveness inputs the endpoint already computes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerLiveness {
    /// The supervisor holds a live (or mid-respawn) worker for this session.
    pub running: bool,
    /// The session was auto-stopped for inactivity.
    pub idle_dormant: bool,
    /// Any live, unsuperseded rate-limit park, armed or capped. With
    /// `rate_limit_auto_resume` off (the default) the reconciler holds its
    /// respawn indefinitely, so a queued prompt would wait on a worker nothing
    /// starts; sending is the user asking to try again now.
    pub rate_limit_parked: bool,
}

/// Decide what to do with a prompt arriving for `state`.
pub fn decide(state: &AcpState, worker: WorkerLiveness) -> PromptDispatch {
    if !worker.running && !worker.idle_dormant && !worker.rate_limit_parked {
        return PromptDispatch::Queued {
            reason: QueueReason::WorkerDown,
        };
    }
    if !state.turn_active {
        return PromptDispatch::Sent;
    }
    if state.cancelling {
        return PromptDispatch::Queued {
            reason: QueueReason::Cancelling,
        };
    }
    if state.compacting {
        return PromptDispatch::Queued {
            reason: QueueReason::Compacting,
        };
    }
    if state.steering {
        return PromptDispatch::Steered;
    }
    PromptDispatch::Queued {
        reason: QueueReason::TurnActive,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(turn_active, steering, cancelling, compacting)`.
    fn state(flags: (bool, bool, bool, bool)) -> AcpState {
        let mut s = AcpState::new(
            crate::acp::state::AcpSessionId("sess-1".into()),
            crate::acp::state::AgentName("claude".into()),
            None,
        );
        (s.turn_active, s.steering, s.cancelling, s.compacting) = flags;
        s
    }

    /// `(running, idle_dormant, rate_limit_parked)`.
    fn worker(flags: (bool, bool, bool)) -> WorkerLiveness {
        WorkerLiveness {
            running: flags.0,
            idle_dormant: flags.1,
            rate_limit_parked: flags.2,
        }
    }

    /// The decision table, keyed by the incident each row exists for.
    #[test]
    fn dispatch_table_covers_every_incident_by_name() {
        let queued = |r| PromptDispatch::Queued { reason: r };
        const LIVE: (bool, bool, bool) = (true, false, false);
        const IDLE: (bool, bool, bool) = (false, false, false);
        const DORMANT: (bool, bool, bool) = (false, true, false);
        // Armed or capped: `decide` does not tell them apart.
        const PARKED: (bool, bool, bool) = (false, false, true);
        // (incident, turn flags, worker liveness, decision)
        let cases = [
            (
                "idle turn, live worker: ordinary send",
                (false, false, false, false),
                LIVE,
                PromptDispatch::Sent,
            ),
            (
                "#2805 steerable turn takes a mid-turn prompt instead of queueing after it",
                (true, true, false, false),
                LIVE,
                PromptDispatch::Steered,
            ),
            (
                "#2805 a non-steerable turn still parks",
                (true, false, false, false),
                LIVE,
                queued(QueueReason::TurnActive),
            ),
            (
                "#1727 steerable but cancelling: parking is what keeps a \
                 Stop-then-type from restarting the runner",
                (true, true, true, false),
                LIVE,
                queued(QueueReason::Cancelling),
            ),
            (
                "#1727 cancelling outranks compacting, so the reason names the \
                 gate that would have restarted the worker",
                (true, true, true, true),
                LIVE,
                queued(QueueReason::Cancelling),
            ),
            (
                "#3219 steerable but compacting: the adapter would swallow the \
                 message into a turn that never answers it",
                (true, true, false, true),
                LIVE,
                queued(QueueReason::Compacting),
            ),
            (
                "#1689 idle-dormant worker: the POST is the wake path, so a \
                 fresh prompt sends rather than parking on 'not running'",
                (false, false, false, false),
                DORMANT,
                PromptDispatch::Sent,
            ),
            (
                "#1689 a genuinely cold worker (mid-resume, not dormant) parks",
                (false, false, false, false),
                IDLE,
                queued(QueueReason::WorkerDown),
            ),
            (
                "worker liveness is checked before the turn flags: no worker \
                 means no turn can be steered into",
                (true, true, false, false),
                IDLE,
                queued(QueueReason::WorkerDown),
            ),
            (
                "an idle-dormant session with a stale turn_active latch parks \
                 rather than sending into a turn nothing is running",
                (true, false, false, false),
                DORMANT,
                queued(QueueReason::TurnActive),
            ),
            (
                "a rate-limit park sends, armed or capped: by default its respawn \
                 is held indefinitely, so queueing would strand the prompt",
                (false, false, false, false),
                PARKED,
                PromptDispatch::Sent,
            ),
            (
                "#3688 a park does not override the turn gates either, \
                 so a stale turn_active latch still parks",
                (true, false, false, false),
                PARKED,
                queued(QueueReason::TurnActive),
            ),
        ];
        for (name, flags, liveness, expected) in cases {
            assert_eq!(decide(&state(flags), worker(liveness)), expected, "{name}");
        }
    }

    /// The wire shape the clients switch on.
    #[test]
    fn dispatch_serializes_to_the_documented_wire_shape() {
        let cases = [
            (PromptDispatch::Sent, r#"{"disposition":"sent"}"#),
            (PromptDispatch::Steered, r#"{"disposition":"steered"}"#),
            (
                PromptDispatch::Queued {
                    reason: QueueReason::Cancelling,
                },
                r#"{"disposition":"queued","reason":"cancelling"}"#,
            ),
        ];
        for (dispatch, expected) in cases {
            assert_eq!(serde_json::to_string(&dispatch).unwrap(), expected);
        }
    }
}
