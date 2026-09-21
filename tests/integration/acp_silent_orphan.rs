//! Silent-orphan watchdog (#1240): the adapter stops without returning the
//! `PromptResponse`, and the daemon synthesizes a terminal `Stopped`. What the
//! turn emitted before going quiet decides the reason:
//!   1. wrapped up (cost-bearing usage_update) then silent: the turn finished,
//!      so `prompt_complete` on the fast grace, no cancel, no restart (#2237).
//!   2. never wrapped up: a real wedge, so the base grace expires and the
//!      watchdog cancels with `prompt_orphaned`.
//!   3. off-protocol work pending (async agent, backgrounded Bash, scheduled
//!      wakeup): suppressed until the work can be over.
//!   4. grace = 0: watchdog skipped entirely.
//!
//! Scenarios that depend on a `usage_update` assert the daemon received it, so
//! a fixture the schema rejects cannot pass as case 2 (#3811). Skipped without
//! `node`, and compiled only under `cfg(debug_assertions)` by `main.rs`,
//! because the graces are only tunable there.

use std::time::{Duration, Instant};

use agent_of_empires::acp::acp_client::AcpClient;
use agent_of_empires::acp::state::{AcpSessionId, Event};
use serial_test::serial;

use crate::common::{shim_ready, spawn_runner_with_shim, EnvGuard};

/// Evidence retained across every observation phase of one turn.
#[derive(Default)]
struct TurnOutcome {
    /// None means no usage arrived; Some records whether any update carried cost.
    usage_cost: Option<bool>,
    stopped: Option<String>,
}

impl TurnOutcome {
    async fn await_activity(&mut self, client: &mut AcpClient, marker: Option<&str>) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match client
                    .next_event()
                    .await
                    .expect("ACP stream open before activity")
                {
                    Event::UsageUpdated { usage } => {
                        self.usage_cost =
                            Some(self.usage_cost.unwrap_or(false) || usage.cost.is_some());
                        if marker.is_none() && usage.cost.is_some() {
                            return;
                        }
                    }
                    Event::ToolCallCompleted { content, .. }
                        if marker.is_some_and(|needle| content.contains(needle)) =>
                    {
                        return;
                    }
                    Event::Stopped { reason } => {
                        panic!("turn stopped before qualifying activity: {reason}")
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("qualifying native activity received");
    }

    async fn drain_turn(&mut self, client: &mut AcpClient, deadline: Instant) {
        while Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), client.next_event()).await {
                Ok(Some(Event::UsageUpdated { usage })) => {
                    self.usage_cost =
                        Some(self.usage_cost.unwrap_or(false) || usage.cost.is_some());
                }
                Ok(Some(Event::Stopped { reason })) => {
                    self.stopped = Some(reason);
                    return;
                }
                Ok(Some(_)) => continue,
                Ok(None) => panic!("ACP stream closed during watchdog observation"),
                Err(_) => continue,
            }
        }
    }
}

/// One watchdog scenario: park the shim on `prompt` with the given base and
/// fast graces (ms), optionally wait for qualifying activity first, then drain
/// for `drain_secs` and return what was observed.
///
/// The check interval is pinned at 50ms throughout so the watchdog tracks the
/// configured grace instead of the default 5s tick; without that a regressed
/// grace could slip past a short deadline simply by not having ticked.
async fn observe_parked_turn(
    preseed: &str,
    (base_grace, fast_grace): (&'static str, &'static str),
    prompt: &str,
    await_marker: Option<Option<&str>>,
    drain_secs: u64,
) -> TurnOutcome {
    let _env = EnvGuard::from_pairs(&[
        ("AOE_SILENT_ORPHAN_GRACE_MS", base_grace),
        ("AOE_SILENT_ORPHAN_FAST_GRACE_MS", fast_grace),
        ("AOE_SILENT_ORPHAN_CHECK_INTERVAL_MS", "50"),
    ]);
    let (socket_path, _runner) =
        spawn_runner_with_shim(preseed, &[("SHIM_PRESEED_SESSION_ID", preseed.to_string())]).await;
    let mut client = AcpClient::attach(
        socket_path,
        std::env::temp_dir(),
        vec![],
        preseed.to_string(),
        false,
        AcpSessionId(preseed.into()),
        None,
        "claude".into(),
        None,
    )
    .await
    .expect("attach to the parked runner");
    client.send_prompt(prompt, &[]).await.expect("send prompt");

    let mut outcome = TurnOutcome::default();
    if let Some(marker) = await_marker {
        outcome.await_activity(&mut client, marker).await;
    }
    outcome
        .drain_turn(
            &mut client,
            Instant::now() + Duration::from_secs(drain_secs),
        )
        .await;
    let _ = client.shutdown().await;
    outcome
}

macro_rules! skip_without_shim {
    () => {
        if let Err(reason) = shim_ready() {
            eprintln!("skipping: {reason}");
            return;
        }
    };
}

/// #2237: a turn that emitted its cost-bearing end-of-turn `usage_update` and
/// then never returned the `PromptResponse` finished; the adapter only failed
/// to say so. Base grace sits outside the drain and the fast grace far inside
/// it, so the turn can only end in time if the cost marker armed the fast
/// grace, and it must end as `prompt_complete`, which neither cancels the turn
/// nor restarts the worker over work that succeeded.
#[tokio::test]
#[serial]
async fn cost_bearing_wrap_up_without_response_ends_as_prompt_complete() {
    skip_without_shim!();
    let outcome = observe_parked_turn(
        "silent-orphan-positive",
        ("60000", "300"),
        "COST_THEN_SILENCE trigger",
        None,
        15,
    )
    .await;

    assert_eq!(
        outcome.usage_cost,
        Some(true),
        "the fixture's cost-bearing UsageUpdate must reach the daemon, otherwise this turn is the no-cost wedge instead"
    );
    assert_eq!(
        outcome.stopped.as_deref(),
        Some("prompt_complete"),
        "a turn that wrapped up its accounting must end cleanly, not be cancelled as an orphan"
    );
}

/// The genuine wedge: a chunk and a cost-less mid-turn `usage_update`, then
/// silence. Nothing arms the fast grace (set far longer than the drain here),
/// so the base grace expires and the watchdog cancels the turn.
#[tokio::test]
#[serial]
async fn silent_orphan_fires_when_the_turn_never_wraps_up() {
    skip_without_shim!();
    let outcome = observe_parked_turn(
        "silent-orphan-no-cost",
        ("300", "5000"),
        "SILENCE_NO_COST trigger",
        None,
        15,
    )
    .await;

    assert_eq!(
        outcome.usage_cost,
        Some(false),
        "the fixture's cost-less UsageUpdate must reach the daemon and carry no cost"
    );
    assert_eq!(
        outcome.stopped.as_deref(),
        Some("prompt_orphaned"),
        "a turn that never wrapped up must be cancelled and reported as an orphan"
    );
}

/// A healthy turn (no parking keyword) completes long before either grace, so
/// the natural `prompt_complete` wins and the watchdog stays disarmed.
#[tokio::test]
#[serial]
async fn silent_orphan_suppressed_during_normal_turn() {
    skip_without_shim!();
    let outcome = observe_parked_turn(
        "silent-orphan-negative",
        ("10000", "10000"),
        "normal turn",
        None,
        5,
    )
    .await;

    assert_eq!(
        outcome.stopped.as_deref(),
        Some("prompt_complete"),
        "the watchdog must stay disarmed on a normal turn; saw {:?}",
        outcome.stopped
    );
}

/// `0` disables the watchdog entirely. The fast grace is short enough that a
/// wrongly-armed watchdog would fire inside the drain, so the silence is a real
/// assertion rather than an untested window.
#[tokio::test]
#[serial]
async fn silent_orphan_disabled_by_zero_grace() {
    skip_without_shim!();
    let outcome = observe_parked_turn(
        "silent-orphan-disabled",
        ("0", "200"),
        "COST_THEN_SILENCE trigger",
        Some(None),
        2,
    )
    .await;

    assert_eq!(
        outcome.usage_cost,
        Some(true),
        "the fixture's cost-bearing UsageUpdate must reach the daemon, otherwise a disabled watchdog is not what kept this turn quiet"
    );
    assert!(
        outcome.stopped.is_none(),
        "the watchdog must stay fully disarmed when grace = 0; saw Stopped reason={:?}",
        outcome.stopped
    );
}

/// Off-protocol work promotes the effective grace to
/// `OFF_PROTOCOL_WORK_GRACE_FLOOR` (30 minutes), so a short drain stays silent
/// even with both graces set tight: the Claude SDK async-agent marker (#1360),
/// a backgrounded Bash launch (#1401, the production false positive that killed
/// a legitimate wait), and a `ScheduleWakeup` whose absolute deadline must
/// override the fast grace the trailing cost frame arms (#1401).
#[tokio::test]
#[serial]
async fn silent_orphan_suppressed_while_off_protocol_work_is_pending() {
    skip_without_shim!();
    // (preseed, prompt, awaited marker, expected usage evidence)
    let cases = [
        (
            "silent-orphan-async-agent",
            "ASYNC_AGENT_ORPHAN trigger",
            Some("Async agent launched successfully"),
            None,
        ),
        (
            "silent-orphan-background-bash",
            "BACKGROUND_BASH_ORPHAN trigger",
            Some("Command running in background with ID:"),
            // A usage frame here would drop the off-protocol floor instead.
            Some(None),
        ),
        (
            "silent-orphan-wakeup",
            "WAKEUP_ORPHAN trigger",
            None,
            // The wakeup deadline must beat the fast grace this frame arms.
            Some(Some(true)),
        ),
    ];
    for (preseed, prompt, marker, expected_usage) in cases {
        let outcome = observe_parked_turn(preseed, ("300", "100"), prompt, Some(marker), 2).await;
        if let Some(expected) = expected_usage {
            assert_eq!(outcome.usage_cost, expected, "{preseed}");
        }
        assert!(
            outcome.stopped.is_none(),
            "{preseed}: the watchdog must stay suppressed while off-protocol work is pending; saw Stopped reason={:?}",
            outcome.stopped
        );
    }
}

/// #1858: a backgrounded command outlives its turn, so once the turn emits its
/// cost-bearing end-of-turn `usage_update` the off-protocol floor is dropped
/// and a missing `PromptResponse` recovers on the fast grace instead of holding
/// the connection for 30 minutes. The suppression above therefore has an end.
#[tokio::test]
#[serial]
async fn background_bash_wrap_up_ends_as_prompt_complete() {
    skip_without_shim!();
    let outcome = observe_parked_turn(
        "silent-orphan-background-bash-wrap-up",
        ("60000", "300"),
        "BACKGROUND_BASH_ORPHAN WRAP_UP trigger",
        None,
        15,
    )
    .await;

    assert_eq!(
        outcome.usage_cost,
        Some(true),
        "the fixture's cost-bearing UsageUpdate must reach the daemon, otherwise the off-protocol floor is what kept this turn open"
    );
    assert_eq!(
        outcome.stopped.as_deref(),
        Some("prompt_complete"),
        "a backgrounded command must not hold its turn open past the end-of-turn accounting frame"
    );
}

/// Usage seen before a tool completion, or only during the drain, must survive
/// both observation phases rather than being lost with the phase that saw it.
#[tokio::test]
#[serial]
async fn usage_evidence_survives_activity_and_drain() {
    skip_without_shim!();
    let mut observed = Vec::new();
    for (prompt, marker) in [
        ("normal turn", Some("")),
        ("USAGE_BEFORE_NO_COST", Some("")),
        ("USAGE_BEFORE_COST USAGE_AFTER_NO_COST", Some("")),
        ("USAGE_BEFORE_COST USAGE_AFTER_NO_COST", None),
    ] {
        // Park after the ordered notifications and drain through the watchdog
        // terminal, so an immediate PromptResponse cannot overtake delivery.
        let _env = EnvGuard::from_pairs(&[
            ("AOE_SILENT_ORPHAN_GRACE_MS", "300"),
            ("AOE_SILENT_ORPHAN_FAST_GRACE_MS", "300"),
            ("AOE_SILENT_ORPHAN_CHECK_INTERVAL_MS", "50"),
        ]);
        let preseed = "usage-observation";
        let (socket_path, _runner) =
            spawn_runner_with_shim(preseed, &[("SHIM_PRESEED_SESSION_ID", preseed.to_string())])
                .await;
        let mut client = AcpClient::attach(
            socket_path,
            std::env::temp_dir(),
            vec![],
            preseed.to_string(),
            false,
            AcpSessionId(preseed.into()),
            None,
            "claude".into(),
            None,
        )
        .await
        .expect("attach for usage observation");
        client
            .send_prompt(&format!("USAGE_OBSERVATION {prompt}"), &[])
            .await
            .expect("send prompt");

        let mut outcome = TurnOutcome::default();
        outcome.await_activity(&mut client, marker).await;
        let activity_cost = outcome.usage_cost;
        outcome
            .drain_turn(&mut client, Instant::now() + Duration::from_secs(10))
            .await;
        let _ = client.shutdown().await;
        assert_eq!(outcome.stopped.as_deref(), Some("prompt_orphaned"));
        observed.push((activity_cost, outcome.usage_cost));
    }

    assert_eq!(
        observed,
        vec![
            (None, None),
            (Some(false), Some(false)),
            (Some(true), Some(true)),
            (Some(true), Some(true)),
        ],
        "usage before tool completion or a cost-less drain must not be lost"
    );
}
