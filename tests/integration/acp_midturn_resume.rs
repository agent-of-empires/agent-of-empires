//! Mid-turn `aoe serve` reattach: integration coverage for the daemon
//! side of the fix. Stands up the production runner around a Node ACP shim
//! so the real `AcpClient::attach`, control connection, and ACP
//! `initialize` path are exercised. It asserts:
//!
//! 1. `attach` with `in_flight_turn = true` synthesizes
//!    `Event::Stopped { reason: "reattach_idle" }` after the configured
//!    grace, since the orphaned upstream `session/prompt` response has
//!    no daemon-side request id to land against.
//!
//! 2. `attach` with `in_flight_turn = false` does NOT synthesize one —
//!    the watchdog must stay disarmed when the session was idle.
//!
//! Skipped automatically if `node` is not on PATH.
//!
//! Note: the parent `main.rs` only compiles this module under
//! `cfg(debug_assertions)`. Debug-only because
//! the watchdog grace is tunable via `AOE_RESUME_IDLE_GRACE_MS` only
//! under `cfg(debug_assertions)` (see `resume_idle_grace()` in
//! `src/acp/acp_client/connection.rs`); release builds would wait the full
//! 10s production default and fail the 3s assertion below.

use std::time::{Duration, Instant};

use agent_of_empires::acp::acp_client::AcpClient;
use agent_of_empires::acp::state::{AcpSessionId, Event};

use crate::common::{shim_ready, spawn_runner_with_shim};

async fn drain_for_stopped_reason(client: &mut AcpClient, deadline: Instant) -> Option<String> {
    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), client.next_event()).await {
            Ok(Some(Event::Stopped { reason })) => return Some(reason),
            Ok(Some(_)) => continue,
            Ok(None) => return None,
            Err(_) => continue,
        }
    }
    None
}

#[tokio::test]
#[serial_test::serial]
async fn attach_in_flight_synthesizes_reattach_idle_stopped() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }

    // Shorten the watchdog grace so the test completes inside ~3s
    // instead of the 10s production default.
    std::env::set_var("AOE_RESUME_IDLE_GRACE_MS", "500");

    // The runner must announce the same session id the daemon attaches
    // with; the control handshake verifies it.
    const SESSION: &str = "midturn-true";
    let (socket_path, _runner) = spawn_runner_with_shim(SESSION, &[]).await;

    let mut client = AcpClient::attach(
        socket_path,
        std::env::temp_dir(),
        vec![],
        "test-acp-session-id".into(),
        true, // in_flight_turn
        AcpSessionId("midturn-true".into()),
        None,
        "claude".into(),
        None,
    )
    .await
    .expect("attach in_flight=true");

    let stopped =
        drain_for_stopped_reason(&mut client, Instant::now() + Duration::from_secs(3)).await;
    let _ = client.shutdown().await;

    assert_eq!(
        stopped.as_deref(),
        Some("reattach_idle"),
        "resume-idle watchdog must synthesize a Stopped event"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn attach_idle_session_does_not_synthesize_stopped() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }

    std::env::set_var("AOE_RESUME_IDLE_GRACE_MS", "500");

    // The runner must announce the same session id the daemon attaches
    // with; the control handshake verifies it.
    const SESSION: &str = "midturn-false";
    let (socket_path, _runner) = spawn_runner_with_shim(SESSION, &[]).await;

    let mut client = AcpClient::attach(
        socket_path,
        std::env::temp_dir(),
        vec![],
        "test-acp-session-id".into(),
        false, // NOT in flight
        AcpSessionId("midturn-false".into()),
        None,
        "claude".into(),
        None,
    )
    .await
    .expect("attach in_flight=false");

    let stopped =
        drain_for_stopped_reason(&mut client, Instant::now() + Duration::from_secs(2)).await;
    let _ = client.shutdown().await;

    assert!(
        stopped.is_none(),
        "watchdog must stay disarmed when in_flight_turn=false; got Stopped reason={stopped:?}"
    );
}

/// #1216: once the runner forwards any notification after reattach, the
/// in-flight turn is observable, so normal mid-turn silence (Task
/// subagents, slow Bash, reasoning gaps) must NOT trip the watchdog. The
/// shim emits one unsolicited chunk early, then goes silent well past
/// the grace; the watchdog must disarm on that first notification rather
/// than synthesize a spurious `reattach_idle` Stopped.
#[tokio::test]
#[serial_test::serial]
async fn attach_in_flight_disarms_after_first_inbound_notification() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }

    // Grace 800ms; the shim emits its single chunk at 200ms. Without the
    // disarm-on-first-event fix the watchdog would fire ~800ms after that
    // chunk (around t=1s); we drain for 2.5s to catch it. With the fix it
    // disarms on the chunk and never fires.
    std::env::set_var("AOE_RESUME_IDLE_GRACE_MS", "800");

    let session_id = "test-acp-session-id";
    // The runner must announce the same session id the daemon attaches
    // with; the control handshake verifies it.
    const SESSION: &str = "midturn-disarm";
    let (socket_path, _runner) = spawn_runner_with_shim(
        SESSION,
        &[
            ("SHIM_PRESEED_SESSION_ID", session_id.to_string()),
            ("SHIM_EMIT_UNSOLICITED_NOTIF", "200".to_string()),
        ],
    )
    .await;

    let mut client = AcpClient::attach(
        socket_path,
        std::env::temp_dir(),
        vec![],
        session_id.into(),
        true, // in_flight_turn
        AcpSessionId("midturn-disarm".into()),
        None,
        "claude".into(),
        None,
    )
    .await
    .expect("attach in_flight=true");

    let stopped =
        drain_for_stopped_reason(&mut client, Instant::now() + Duration::from_millis(2500)).await;
    let _ = client.shutdown().await;

    assert!(
        stopped.is_none(),
        "watchdog must disarm after the first inbound notification; mid-turn silence is not an orphan; got Stopped reason={stopped:?}"
    );
}

/// Attach to a production runner around the shim, send a prompt, and
/// confirm the response returns as `AgentMessageChunk` and `Stopped`
/// events through the v3 control transport.
#[tokio::test]
async fn socket_transport_round_trips_prompt_via_attach() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }

    let preseed = "preseed-roundtrip-session";
    // The runner must announce the same session id the daemon attaches
    // with; the control handshake verifies it.
    const SESSION: &str = "roundtrip";
    let (socket_path, _runner) =
        spawn_runner_with_shim(SESSION, &[("SHIM_PRESEED_SESSION_ID", preseed.to_string())]).await;

    let mut client = AcpClient::attach(
        socket_path,
        std::env::temp_dir(),
        vec![],
        preseed.into(),
        false, // not in flight; this is a fresh round-trip
        AcpSessionId("roundtrip".into()),
        None,
        "claude".into(),
        None,
    )
    .await
    .expect("attach to bridge");

    client
        .send_prompt("hello over socket", &[])
        .await
        .expect("send_prompt");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut saw_received = false;
    let mut saw_stopped = false;
    while Instant::now() < deadline && !(saw_received && saw_stopped) {
        match tokio::time::timeout(Duration::from_millis(200), client.next_event()).await {
            Ok(Some(Event::AgentMessageChunk { text })) => {
                if text.contains("received: hello over socket") {
                    saw_received = true;
                }
            }
            Ok(Some(Event::Stopped { .. })) => {
                saw_stopped = true;
            }
            Ok(Some(_)) => continue,
            Ok(None) => break,
            Err(_) => continue,
        }
    }
    let _ = client.shutdown().await;

    assert!(
        saw_received,
        "shim should echo received: hello over socket via the socket transport"
    );
    assert!(saw_stopped, "shim should emit Stopped at end of turn");
}
