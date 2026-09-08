//! Daemon-side coverage for the silent-orphan watchdog (#1240). Spawns the
//! Node test shim behind a real `__acp-runner`, prompts it into one of the
//! parked shapes of the upstream
//! `agentclientprotocol/claude-agent-acp#688` failure mode (the adapter
//! stops without returning the `PromptResponse`), and asserts the terminal
//! `Stopped` the daemon synthesizes.
//!
//! What the turn emitted before going quiet decides the outcome, so the
//! cost-bearing and no-cost shapes are separate scenarios:
//!   1. wrapped up (cost-populated usage_update) then silent: the turn
//!      demonstrably finished, so it ends as `prompt_complete` with no
//!      cancel and no worker restart (#2237).
//!   2. never wrapped up (usage without cost, or none at all): a genuine
//!      wedge, so the base grace expires and the watchdog cancels and
//!      reports `prompt_orphaned`.
//!   3. off-protocol work pending (async agent, backgrounded Bash,
//!      scheduled wakeup): suppressed until the work can be over.
//!   4. disabled (grace = 0): watchdog skipped entirely, no Stopped.
//!
//! A scenario that depends on a `usage_update` asserts the daemon received
//! it, so a fixture the schema rejects cannot quietly pass as case 2 (#3811).
//!
//! Skipped automatically if `node` is missing.
//!
//! Note: the parent `main.rs` only compiles this module under
//! `cfg(debug_assertions)`. Debug-only because
//! the watchdog grace is tunable via `AOE_SILENT_ORPHAN_GRACE_MS` /
//! `AOE_SILENT_ORPHAN_FAST_GRACE_MS` only under `cfg(debug_assertions)`;
//! release builds would wait the full 60s production default.

use std::time::{Duration, Instant};

use agent_of_empires::acp::acp_client::AcpClient;
use agent_of_empires::acp::state::{AcpSessionId, Event};
use serial_test::serial;

use crate::common::{shim_ready, spawn_runner_with_shim};

/// RAII helper that snapshots env-var values on construction and
/// restores them on drop. The watchdog tests are `#[serial]` but the
/// env mutations leak across test order regardless; the guard keeps
/// each test hermetic so adding or reordering cases can't break the
/// next one. See #1401 and CodeRabbit feedback on PR #1364.
///
/// The crate's own `session::test_support::EnvGuard` is `pub(crate)` and
/// so out of reach from this integration-test crate; the snapshot logic
/// is duplicated here rather than widening that helper's visibility.
///
/// Snapshots are `Option<OsString>` read via [`std::env::var_os`], not
/// `Option<String>` via `env::var(..).ok()`. `env::var` returns
/// `Err(NotUnicode(_))` for a non-UTF-8 prior value, which `.ok()` would
/// collapse to `None`, making `Drop` *remove* the var instead of
/// restoring its bytes and leaking the removal into every later
/// `#[serial]` test in this binary. See issue #2751.
struct EnvGuard {
    vars: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl EnvGuard {
    fn set(pairs: &[(&'static str, &'static str)]) -> Self {
        let vars: Vec<_> = pairs
            .iter()
            .map(|(k, _)| (*k, std::env::var_os(k)))
            .collect();
        for (k, v) in pairs {
            std::env::set_var(k, v);
        }
        Self { vars }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (k, old) in self.vars.drain(..) {
            match old {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}

/// What one drained turn produced.
struct TurnOutcome {
    /// `None` while no `UsageUpdated` has arrived, then whether any of them
    /// carried a cost. A fixture whose payload the ACP schema rejects never
    /// reaches the daemon, so this stays `None` and a scenario that means to
    /// exercise the cost-bearing path cannot pass on the no-cost one (#3811).
    usage_cost: Option<bool>,
    /// The turn's terminal reason, or `None` if the deadline came first.
    stopped: Option<String>,
}

async fn drain_turn(client: &mut AcpClient, deadline: Instant) -> TurnOutcome {
    let mut usage_cost = None;
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), client.next_event()).await {
            Ok(Some(Event::UsageUpdated { usage })) => {
                usage_cost = Some(usage_cost.unwrap_or(false) || usage.cost.is_some());
            }
            Ok(Some(Event::Stopped { reason })) => {
                return TurnOutcome {
                    usage_cost,
                    stopped: Some(reason),
                }
            }
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(_) => continue,
        }
    }
    TurnOutcome {
        usage_cost,
        stopped: None,
    }
}

/// #2237: a turn that emitted its cost-populated end-of-turn
/// `usage_update` and then never returned the `PromptResponse` finished;
/// the adapter only failed to say so. The watchdog must end it on the fast
/// grace as `prompt_complete`, the reason that neither cancels the turn nor
/// restarts the worker over work that succeeded.
#[tokio::test]
#[serial]
async fn cost_bearing_wrap_up_without_response_ends_as_prompt_complete() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }

    // Base grace outside the drain below, fast grace far inside it: the
    // turn can only end in time if the cost marker armed the fast grace,
    // so a regression that stops arming it fails here instead of reaching
    // the same reason later on the base grace. Polling cadence dropped to
    // 50ms so the watchdog evaluation tracks the configured grace closely
    // instead of waiting up to the default 5s tick.
    let _env = EnvGuard::set(&[
        ("AOE_SILENT_ORPHAN_GRACE_MS", "60000"),
        ("AOE_SILENT_ORPHAN_FAST_GRACE_MS", "300"),
        ("AOE_SILENT_ORPHAN_CHECK_INTERVAL_MS", "50"),
    ]);

    let preseed = "silent-orphan-positive";
    let (socket_path, _tmp) =
        spawn_runner_with_shim(preseed, &[("SHIM_PRESEED_SESSION_ID", preseed.to_string())]).await;

    let client = AcpClient::attach(
        socket_path,
        std::env::temp_dir(),
        vec![],
        preseed.to_string(),
        false,
        AcpSessionId("silent-orphan-positive".into()),
        None,
        "claude".into(),
        None,
    )
    .await
    .expect("attach for silent-orphan positive test");

    let mut client = client;
    client
        .send_prompt("COST_THEN_SILENCE trigger", &[])
        .await
        .expect("send prompt");

    // 15s budget rather than 1s: the watchdog acts at FAST_GRACE (300ms)
    // after the cost-populated usage_update, but ubuntu-latest under full
    // cargo-test load occasionally schedules the shim's prompt body or the
    // daemon's lifecycle signal pump late enough that the Stopped emission
    // slips past a tight drain. Still well under the 60s base grace, so a
    // regression that never arms the fast grace fails here (drain returns
    // None).
    let outcome = drain_turn(&mut client, Instant::now() + Duration::from_secs(15)).await;
    let _ = client.shutdown().await;

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

/// The genuine wedge: the adapter streams a chunk and a cost-less
/// mid-turn `usage_update`, then goes silent without ever wrapping up.
/// Nothing arms the fast grace, so the base grace expires and the
/// watchdog cancels the turn and reports `prompt_orphaned`.
#[tokio::test]
#[serial]
async fn silent_orphan_fires_when_the_turn_never_wraps_up() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }

    // Base grace tight, fast grace far longer: only the no-cost path can
    // fire inside the drain, so a usage frame that wrongly armed the fast
    // grace would not be mistaken for this one.
    let _env = EnvGuard::set(&[
        ("AOE_SILENT_ORPHAN_GRACE_MS", "300"),
        ("AOE_SILENT_ORPHAN_FAST_GRACE_MS", "5000"),
        ("AOE_SILENT_ORPHAN_CHECK_INTERVAL_MS", "50"),
    ]);

    let preseed = "silent-orphan-no-cost";
    let (socket_path, _tmp) =
        spawn_runner_with_shim(preseed, &[("SHIM_PRESEED_SESSION_ID", preseed.to_string())]).await;

    let client = AcpClient::attach(
        socket_path,
        std::env::temp_dir(),
        vec![],
        preseed.to_string(),
        false,
        AcpSessionId("silent-orphan-no-cost".into()),
        None,
        "claude".into(),
        None,
    )
    .await
    .expect("attach for no-cost silent-orphan test");

    let mut client = client;
    client
        .send_prompt("SILENCE_NO_COST trigger", &[])
        .await
        .expect("send prompt");

    let outcome = drain_turn(&mut client, Instant::now() + Duration::from_secs(15)).await;
    let _ = client.shutdown().await;

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

#[tokio::test]
#[serial]
async fn silent_orphan_suppressed_during_normal_turn() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }

    // Generous enough grace that the shim's healthy tool round-trip
    // completes long before the watchdog could fire; we then assert
    // the only Stopped we see is prompt_complete, not prompt_orphaned.
    // Tight polling cadence so a regressed grace would fire within the
    // assertion window instead of waiting for the default 5s tick.
    let _env = EnvGuard::set(&[
        ("AOE_SILENT_ORPHAN_GRACE_MS", "10000"),
        ("AOE_SILENT_ORPHAN_FAST_GRACE_MS", "10000"),
        ("AOE_SILENT_ORPHAN_CHECK_INTERVAL_MS", "50"),
    ]);

    let preseed = "silent-orphan-negative";
    let (socket_path, _tmp) =
        spawn_runner_with_shim(preseed, &[("SHIM_PRESEED_SESSION_ID", preseed.to_string())]).await;

    let client = AcpClient::attach(
        socket_path,
        std::env::temp_dir(),
        vec![],
        preseed.to_string(),
        false,
        AcpSessionId("silent-orphan-negative".into()),
        None,
        "claude".into(),
        None,
    )
    .await
    .expect("attach for silent-orphan negative test");

    let mut client = client;
    // No parking keyword: the shim's default prompt() runs the
    // healthy chunk + tool_call + tool_call_update + chunk sequence
    // and returns stopReason=end_turn. The watchdog must stay silent
    // and the natural prompt_complete must win.
    client
        .send_prompt("normal turn", &[])
        .await
        .expect("send prompt");

    let outcome = drain_turn(&mut client, Instant::now() + Duration::from_secs(5)).await;
    let _ = client.shutdown().await;

    assert_eq!(
        outcome.stopped.as_deref(),
        Some("prompt_complete"),
        "silent-orphan watchdog must stay disarmed on a normal turn; saw {:?}",
        outcome.stopped
    );
}

#[tokio::test]
#[serial]
async fn silent_orphan_disabled_by_zero_grace() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }

    // `0` disables the watchdog entirely. With the shim parked on
    // COST_THEN_SILENCE we'd otherwise see prompt_complete within a few
    // hundred milliseconds; instead we should see no Stopped frame at
    // all within the deadline, because nothing else fires.
    //
    // Override the polling cadence too: the default 5s tick would let
    // a regressed "disabled" knob slip past a 2s deadline simply
    // because the watchdog hadn't ticked yet. Forcing a 50ms cadence
    // means a wrongly-armed watchdog WOULD fire within the deadline,
    // turning a silent assertion into a real one.
    let _env = EnvGuard::set(&[
        ("AOE_SILENT_ORPHAN_GRACE_MS", "0"),
        ("AOE_SILENT_ORPHAN_FAST_GRACE_MS", "200"),
        ("AOE_SILENT_ORPHAN_CHECK_INTERVAL_MS", "50"),
    ]);

    let preseed = "silent-orphan-disabled";
    let (socket_path, _tmp) =
        spawn_runner_with_shim(preseed, &[("SHIM_PRESEED_SESSION_ID", preseed.to_string())]).await;

    let client = AcpClient::attach(
        socket_path,
        std::env::temp_dir(),
        vec![],
        preseed.to_string(),
        false,
        AcpSessionId("silent-orphan-disabled".into()),
        None,
        "claude".into(),
        None,
    )
    .await
    .expect("attach for silent-orphan disabled test");

    let mut client = client;
    client
        .send_prompt("COST_THEN_SILENCE trigger", &[])
        .await
        .expect("send prompt");

    let outcome = drain_turn(&mut client, Instant::now() + Duration::from_secs(2)).await;
    let _ = client.shutdown().await;

    assert_eq!(
        outcome.usage_cost,
        Some(true),
        "the fixture's cost-bearing UsageUpdate must reach the daemon, otherwise a disabled watchdog is not what kept this turn quiet"
    );
    assert!(
        outcome.stopped.is_none(),
        "silent-orphan watchdog must stay fully disarmed when grace = 0; saw Stopped reason={:?}",
        outcome.stopped
    );
}

/// #1360: a `ToolCallUpdate` whose completion content carries the Claude
/// SDK marker `"Async agent launched successfully"` must flip the prompt
/// loop's sticky off-protocol state so the watchdog promotes its effective
/// grace to at least `OFF_PROTOCOL_WORK_GRACE_FLOOR` (30 minutes). Without
/// the fix, the watchdog would fire ~300ms after the completion; with it,
/// the test window stays silent.
#[tokio::test]
#[serial]
async fn silent_orphan_suppressed_during_async_agent_wait() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }

    // Base grace 300ms; if the async detection works, effective grace
    // jumps to OFF_PROTOCOL_WORK_GRACE_FLOOR (30 minutes), so a 2s drain
    // must see no `prompt_orphaned`. The fast grace is set tight so a
    // wrongly ordered effective_grace branch (cost-seen > off-protocol)
    // would still false-fire and fail the assertion.
    let _env = EnvGuard::set(&[
        ("AOE_SILENT_ORPHAN_GRACE_MS", "300"),
        ("AOE_SILENT_ORPHAN_FAST_GRACE_MS", "100"),
        ("AOE_SILENT_ORPHAN_CHECK_INTERVAL_MS", "50"),
    ]);

    let preseed = "silent-orphan-async-agent";
    let (socket_path, _tmp) =
        spawn_runner_with_shim(preseed, &[("SHIM_PRESEED_SESSION_ID", preseed.to_string())]).await;

    let client = AcpClient::attach(
        socket_path,
        std::env::temp_dir(),
        vec![],
        preseed.to_string(),
        false,
        AcpSessionId("silent-orphan-async-agent".into()),
        None,
        "claude".into(),
        None,
    )
    .await
    .expect("attach for async-agent silent-orphan test");

    let mut client = client;
    client
        .send_prompt("ASYNC_AGENT_ORPHAN trigger", &[])
        .await
        .expect("send prompt");

    let outcome = drain_turn(&mut client, Instant::now() + Duration::from_secs(2)).await;
    let _ = client.shutdown().await;

    assert!(
        outcome.stopped.is_none(),
        "silent-orphan watchdog must stay suppressed while async-agent is running; saw Stopped reason={:?}",
        outcome.stopped
    );
}

/// #1401: a backgrounded Bash launch (`run_in_background: true` plus the
/// `"Command running in background with ID:"` completion marker) must NOT
/// trigger the watchdog while the turn is still open. This reproduces the
/// production false-positive shape from session `65c7bd0f22424242` where
/// npm install / cargo build were backgrounded and the watchdog killed the
/// legitimate wait via the fast-grace path.
#[tokio::test]
#[serial]
async fn silent_orphan_suppressed_during_background_bash() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }

    // Tight grace and fast grace; if either marker (content text or
    // raw_input.run_in_background) feeds the off-protocol path, the
    // watchdog stays armed-but-suppressed and the 2s drain sees no
    // Stopped.
    let _env = EnvGuard::set(&[
        ("AOE_SILENT_ORPHAN_GRACE_MS", "300"),
        ("AOE_SILENT_ORPHAN_FAST_GRACE_MS", "100"),
        ("AOE_SILENT_ORPHAN_CHECK_INTERVAL_MS", "50"),
    ]);

    let preseed = "silent-orphan-background-bash";
    let (socket_path, _tmp) =
        spawn_runner_with_shim(preseed, &[("SHIM_PRESEED_SESSION_ID", preseed.to_string())]).await;

    let client = AcpClient::attach(
        socket_path,
        std::env::temp_dir(),
        vec![],
        preseed.to_string(),
        false,
        AcpSessionId("silent-orphan-background-bash".into()),
        None,
        "claude".into(),
        None,
    )
    .await
    .expect("attach for backgrounded-bash silent-orphan test");

    let mut client = client;
    client
        .send_prompt("BACKGROUND_BASH_ORPHAN trigger", &[])
        .await
        .expect("send prompt");

    let outcome = drain_turn(&mut client, Instant::now() + Duration::from_secs(2)).await;
    let _ = client.shutdown().await;

    assert_eq!(
        outcome.usage_cost, None,
        "this scenario must not wrap up its accounting; a usage frame here would drop the off-protocol floor instead"
    );
    assert!(
        outcome.stopped.is_none(),
        "silent-orphan watchdog must stay suppressed while a backgrounded Bash task is running; saw Stopped reason={:?}",
        outcome.stopped
    );
}

/// #1858: a backgrounded command is fire-and-forget, so it legitimately
/// outlives its turn. Once the turn emits its cost-populated end-of-turn
/// `usage_update` the off-protocol floor is dropped, and a missing
/// `PromptResponse` recovers on the fast grace as `prompt_complete`
/// instead of holding the connection for the 30-minute floor. The
/// suppression above therefore has an end, and this is where it ends.
#[tokio::test]
#[serial]
async fn background_bash_wrap_up_ends_as_prompt_complete() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }

    // Base grace outside the 15s drain below and the off-protocol floor
    // (30 min) further still, so only the fast grace the dropped floor
    // uncovers can end this turn in time.
    let _env = EnvGuard::set(&[
        ("AOE_SILENT_ORPHAN_GRACE_MS", "60000"),
        ("AOE_SILENT_ORPHAN_FAST_GRACE_MS", "300"),
        ("AOE_SILENT_ORPHAN_CHECK_INTERVAL_MS", "50"),
    ]);

    let preseed = "silent-orphan-background-bash-wrap-up";
    let (socket_path, _tmp) =
        spawn_runner_with_shim(preseed, &[("SHIM_PRESEED_SESSION_ID", preseed.to_string())]).await;

    let client = AcpClient::attach(
        socket_path,
        std::env::temp_dir(),
        vec![],
        preseed.to_string(),
        false,
        AcpSessionId("silent-orphan-background-bash-wrap-up".into()),
        None,
        "claude".into(),
        None,
    )
    .await
    .expect("attach for wrapped-up backgrounded-bash test");

    let mut client = client;
    client
        .send_prompt("BACKGROUND_BASH_ORPHAN WRAP_UP trigger", &[])
        .await
        .expect("send prompt");

    let outcome = drain_turn(&mut client, Instant::now() + Duration::from_secs(15)).await;
    let _ = client.shutdown().await;

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

/// #1401: `ScheduleWakeup` registers an absolute wake timestamp. The
/// watchdog must suppress firing until `at + base_grace`, not snap-fire
/// the moment the sleep ends. A cost-populated `usage_update` is sent
/// after the wakeup tool completes so the test exercises the fast-grace
/// path; a regression where the wakeup deadline didn't override fast
/// grace would false-fire inside the 2s drain.
#[tokio::test]
#[serial]
async fn silent_orphan_suppressed_during_scheduled_wakeup() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }

    let _env = EnvGuard::set(&[
        ("AOE_SILENT_ORPHAN_GRACE_MS", "300"),
        ("AOE_SILENT_ORPHAN_FAST_GRACE_MS", "100"),
        ("AOE_SILENT_ORPHAN_CHECK_INTERVAL_MS", "50"),
    ]);

    let preseed = "silent-orphan-wakeup";
    let (socket_path, _tmp) =
        spawn_runner_with_shim(preseed, &[("SHIM_PRESEED_SESSION_ID", preseed.to_string())]).await;

    let client = AcpClient::attach(
        socket_path,
        std::env::temp_dir(),
        vec![],
        preseed.to_string(),
        false,
        AcpSessionId("silent-orphan-wakeup".into()),
        None,
        "claude".into(),
        None,
    )
    .await
    .expect("attach for wakeup silent-orphan test");

    let mut client = client;
    client
        .send_prompt("WAKEUP_ORPHAN trigger", &[])
        .await
        .expect("send prompt");

    let outcome = drain_turn(&mut client, Instant::now() + Duration::from_secs(2)).await;
    let _ = client.shutdown().await;

    assert_eq!(
        outcome.usage_cost,
        Some(true),
        "the fixture's cost-bearing UsageUpdate must reach the daemon, otherwise the fast grace this scenario overrides is never armed"
    );
    assert!(
        outcome.stopped.is_none(),
        "silent-orphan watchdog must stay suppressed until ScheduleWakeup `at + base_grace`; saw Stopped reason={:?}",
        outcome.stopped
    );
}
