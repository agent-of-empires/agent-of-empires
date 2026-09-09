//! Server-owned prompt dispatch: whether an incoming prompt is sent now,
//! steered into the running turn, or parked on the server queue.
//!
//!
//! This is a daemon decision because it depends on daemon control and worker
//! state. Clients render the returned disposition.

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

/// Why a prompt was parked. Named per gate so a client can explain the wait
/// and so the incident table below reads as prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueReason {
    /// A non-steerable turn is running.
    TurnActive,
    /// A cancel is pending on the running turn (#1727).
    Cancelling,
    /// A `/compact` is running (#3219).
    Compacting,
    /// No live worker, and not the idle-dormant case this POST would wake.
    WorkerDown,
}

/// Worker-liveness inputs the endpoint already computes. Kept separate from
/// `AcpState` because liveness is supervisor/instance state, not something the
/// event fold observes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerLiveness {
    /// The supervisor holds a live (or mid-respawn) worker for this session.
    pub running: bool,
    /// The session was auto-stopped for inactivity. The prompt POST is itself
    /// the wake path, so this is emphatically not "worker down".
    pub idle_dormant: bool,
    /// The session is parked on the rate-limit redelivery cap. Same shape as
    /// `idle_dormant`: no worker, but the POST is the documented recovery, so
    /// it must not park on "worker down".
    pub rate_limit_exhausted: bool,
}

/// Decide what to do with a prompt arriving for `state`.
///
/// The four gates, each a fixed incident:
///
/// - **#1689**: an idle-dormant worker must not park on "not running". The
///   POST clears dormancy, the reconciler respawns, and `send_turn` waits for
///   the fresh worker. Parking instead leaves the prompt in a queue whose
///   drain is waiting for the very worker nothing is going to start.
/// - **#3688**: nor may a session parked on the rate-limit redelivery cap.
///   That park is terminal by design, so nothing un-parks it on a timer and
///   the queue drain would wait forever; the banner tells the user a fresh
///   prompt recovers it, and this is the gate that makes that true.
/// - **#2805**: a steerable agent takes a mid-turn prompt directly. Parking it
///   reintroduces the queue-after behavior steering exists to replace.
/// - **#1727**: except while a cancel is pending. The daemon reads a prompt
///   arriving mid-cancel as a wedged agent and **restarts the runner**, so
///   Stop-then-type must park or it respawns the worker.
/// - **#3219**: and except during `/compact`. The adapter answers `Injected`
///   and swallows the message into a turn that never replies to it, with no
///   retry affordance.
///
/// The failure modes are asymmetric: wrongly sending where the old code parked
/// can restart a worker, while wrongly parking only delays a turn. So every
/// path that is not positively classified as sendable falls through to
/// `Queued`.
pub fn decide(state: &AcpState, worker: WorkerLiveness) -> PromptDispatch {
    if !worker.running && !worker.idle_dormant && !worker.rate_limit_exhausted {
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

    fn live() -> WorkerLiveness {
        WorkerLiveness {
            running: true,
            idle_dormant: false,
            rate_limit_exhausted: false,
        }
    }

    fn state(turn_active: bool, steering: bool, cancelling: bool, compacting: bool) -> AcpState {
        let mut s = AcpState::new(
            crate::acp::state::AcpSessionId("sess-1".into()),
            crate::acp::state::AgentName("claude".into()),
            None,
        );
        s.turn_active = turn_active;
        s.steering = steering;
        s.cancelling = cancelling;
        s.compacting = compacting;
        s
    }

    /// The decision table, keyed by the incident each row exists for. A future
    /// edit that reintroduces one of these fails the row that names it.
    #[test]
    fn dispatch_table_covers_every_incident_by_name() {
        let queued = |r| PromptDispatch::Queued { reason: r };
        let cases: [(&str, AcpState, WorkerLiveness, PromptDispatch); 12] = [
            (
                "idle turn, live worker: ordinary send",
                state(false, false, false, false),
                live(),
                PromptDispatch::Sent,
            ),
            (
                "#2805 steerable turn takes a mid-turn prompt instead of queueing after it",
                state(true, true, false, false),
                live(),
                PromptDispatch::Steered,
            ),
            (
                "#2805 a non-steerable turn still parks",
                state(true, false, false, false),
                live(),
                queued(QueueReason::TurnActive),
            ),
            (
                "#1727 steerable but cancelling: parking is what keeps a \
                 Stop-then-type from restarting the runner",
                state(true, true, true, false),
                live(),
                queued(QueueReason::Cancelling),
            ),
            (
                "#1727 cancelling outranks compacting, so the reason names the \
                 gate that would have restarted the worker",
                state(true, true, true, true),
                live(),
                queued(QueueReason::Cancelling),
            ),
            (
                "#3219 steerable but compacting: the adapter would swallow the \
                 message into a turn that never answers it",
                state(true, true, false, true),
                live(),
                queued(QueueReason::Compacting),
            ),
            (
                "#1689 idle-dormant worker: the POST is the wake path, so a \
                 fresh prompt sends rather than parking on 'not running'",
                state(false, false, false, false),
                WorkerLiveness {
                    running: false,
                    idle_dormant: true,
                    rate_limit_exhausted: false,
                },
                PromptDispatch::Sent,
            ),
            (
                "#1689 a genuinely cold worker (mid-resume, not dormant) parks",
                state(false, false, false, false),
                WorkerLiveness {
                    running: false,
                    idle_dormant: false,
                    rate_limit_exhausted: false,
                },
                queued(QueueReason::WorkerDown),
            ),
            (
                "worker liveness is checked before the turn flags: no worker \
                 means no turn can be steered into",
                state(true, true, false, false),
                WorkerLiveness {
                    running: false,
                    idle_dormant: false,
                    rate_limit_exhausted: false,
                },
                queued(QueueReason::WorkerDown),
            ),
            (
                "an idle-dormant session with a stale turn_active latch parks \
                 rather than sending into a turn nothing is running",
                state(true, false, false, false),
                WorkerLiveness {
                    running: false,
                    idle_dormant: true,
                    rate_limit_exhausted: false,
                },
                queued(QueueReason::TurnActive),
            ),
            (
                "#3688 a session parked on the redelivery cap sends: nothing \
                 un-parks it on a timer, so queueing strands the prompt the \
                 banner asked for",
                state(false, false, false, false),
                WorkerLiveness {
                    running: false,
                    idle_dormant: false,
                    rate_limit_exhausted: true,
                },
                PromptDispatch::Sent,
            ),
            (
                "#3688 the cap park does not override the turn gates either, \
                 so a stale turn_active latch still parks",
                state(true, false, false, false),
                WorkerLiveness {
                    running: false,
                    idle_dormant: false,
                    rate_limit_exhausted: true,
                },
                queued(QueueReason::TurnActive),
            ),
        ];
        for (name, st, worker, expected) in cases {
            assert_eq!(decide(&st, worker), expected, "{name}");
        }
    }

    /// The wire shape the clients switch on. Externally visible contract, so
    /// pin it rather than let a serde attribute change it silently.
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
