//! Experimental `session/delete` RPC dispatch coverage. See #1404.
//!
//! Exercises the round-trip from `AcpClient::delete_session` through
//! the Rust ACP client, the JSON-RPC layer, and the test-shim's
//! `unstable_deleteSession` handler. Tests cover:
//!
//! - Adapter advertising `sessionCapabilities.delete: {}` succeeds and
//!   the shim records the call (matches claude-agent-acp 0.37+).
//! - Adapter NOT advertising the capability returns `-32601`, surfaced
//!   as `DeleteSessionOutcome::Unsupported` (matches aoe-agent, codex,
//!   opencode, older claude-agent-acp).
//! - Adapter handler that exceeds the bounded timeout surfaces as
//!   `TimedOut`; the call does not hang the caller.

use std::path::PathBuf;
use std::time::Duration;

use agent_of_empires::cockpit::acp_client::{AcpClient, DeleteSessionOutcome, SpawnConfig};
use agent_of_empires::cockpit::agent_registry::AgentSpec;
use agent_of_empires::cockpit::state::{CockpitSessionId, Event};

use crate::common::{shim_path, shim_ready};

fn spawn_config_with_shim_env(shim: PathBuf, env: Vec<(String, String)>) -> SpawnConfig {
    SpawnConfig {
        agent_key: "claude".into(),
        spec: AgentSpec {
            command: "node".into(),
            args: vec![shim.to_string_lossy().to_string()],
            description: "session/delete shim".into(),
            env_allowlist: None,
        },
        cwd: std::env::temp_dir(),
        additional_dirs: vec![],
        provider_env: env,
        socket_path: None,
        stored_acp_session_id: None,
        sandbox_info: None,
        source_profile: None,
    }
}

/// Drain events until a `Stopped` arrives or `deadline` elapses; returns
/// the ACP session id assigned during the handshake so subsequent
/// `delete_session` calls can target it.
async fn drive_handshake_and_capture_session_id(
    client: &mut AcpClient,
    deadline: std::time::Instant,
) -> Option<String> {
    client
        .send_prompt("hello", &[])
        .await
        .expect("send_prompt should reach the shim");
    let mut acp_session_id: Option<String> = None;
    while std::time::Instant::now() < deadline {
        let evt = match tokio::time::timeout(Duration::from_millis(200), client.next_event()).await
        {
            Ok(Some(evt)) => evt,
            Ok(None) | Err(_) => continue,
        };
        if let Event::AcpSessionAssigned { acp_session_id: id } = &evt {
            acp_session_id = Some(id.clone());
        }
        if matches!(evt, Event::Stopped { .. }) {
            break;
        }
    }
    acp_session_id
}

#[tokio::test]
async fn session_delete_called_when_capability_advertised() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let record_path = temp.path().join("delete-calls.log");
    let config = spawn_config_with_shim_env(
        shim_path(),
        vec![
            ("SHIM_DELETE_CAPABILITY".into(), "1".into()),
            (
                "SHIM_DELETE_RECORD_FILE".into(),
                record_path.to_string_lossy().to_string(),
            ),
        ],
    );

    let mut client = AcpClient::spawn(config, CockpitSessionId("delete-pos".into()))
        .await
        .expect("spawn shim");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let acp_id = drive_handshake_and_capture_session_id(&mut client, deadline)
        .await
        .expect("shim should assign an ACP session id during handshake");

    let outcome = client.delete_session(acp_id.clone()).await;
    let _ = client.shutdown().await;

    assert!(
        matches!(outcome, DeleteSessionOutcome::Deleted),
        "expected Deleted; got {outcome:?}"
    );
    let recorded =
        std::fs::read_to_string(&record_path).expect("record file should exist after delete RPC");
    assert!(
        recorded.lines().any(|line| line == acp_id),
        "shim should have recorded the deleted session id {acp_id} (file contents: {recorded:?})"
    );
}

#[tokio::test]
async fn session_delete_unsupported_when_capability_absent() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    // Without SHIM_DELETE_CAPABILITY the shim leaves
    // unstable_deleteSession unset, so the SDK's dispatcher returns
    // -32601 for `session/delete`. This is the steady-state shape for
    // aoe-agent, codex, opencode.
    let config = spawn_config_with_shim_env(shim_path(), vec![]);

    let mut client = AcpClient::spawn(config, CockpitSessionId("delete-neg".into()))
        .await
        .expect("spawn shim");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let acp_id = drive_handshake_and_capture_session_id(&mut client, deadline)
        .await
        .expect("shim should assign an ACP session id during handshake");

    let outcome = client.delete_session(acp_id).await;
    let _ = client.shutdown().await;

    assert!(
        matches!(outcome, DeleteSessionOutcome::UnsupportedMethod),
        "expected UnsupportedMethod; got {outcome:?}"
    );
}

#[tokio::test]
async fn session_delete_timeout_does_not_hang() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    let config = spawn_config_with_shim_env(
        shim_path(),
        vec![
            ("SHIM_DELETE_CAPABILITY".into(), "1".into()),
            ("SHIM_DELETE_MODE".into(), "slow".into()),
        ],
    );

    let mut client = AcpClient::spawn(config, CockpitSessionId("delete-slow".into()))
        .await
        .expect("spawn shim");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let acp_id = drive_handshake_and_capture_session_id(&mut client, deadline)
        .await
        .expect("shim should assign an ACP session id during handshake");

    let started = std::time::Instant::now();
    let outcome = client.delete_session(acp_id).await;
    let elapsed = started.elapsed();
    let _ = client.shutdown().await;

    assert!(
        matches!(outcome, DeleteSessionOutcome::TimedOut),
        "expected TimedOut; got {outcome:?}"
    );
    // ACP_SESSION_DELETE_TIMEOUT is 2s; the outer guard adds 500ms.
    // The shim's slow path sleeps 3s, so the wait must surface as
    // TimedOut well before the 3s mark. Cap at 3.4s to keep the
    // assertion robust against scheduler slack.
    assert!(
        elapsed < Duration::from_millis(3400),
        "delete_session should bound the wait; took {:?}",
        elapsed
    );
}
