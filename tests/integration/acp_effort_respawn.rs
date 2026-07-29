//! A session's pinned reasoning effort ("thought level") must be applied after
//! every handshake, not just on a fresh `session/new`.
//!
//! A worker respawn resumes the stored ACP session via `session/load`, so an
//! effort applied only in the `session/new` branch silently reverted to the
//! agent default on every restart (crash, `aoe acp restart`, daemon restart).
//! These tests drive the real `AcpClient` against the test shim and assert the
//! `session/set_config_option` RPC actually fired with the pinned value on the
//! load path as well as the fresh path.

use std::path::PathBuf;
use std::time::Duration;

use agent_of_empires::acp::acp_client::{AcpClient, SpawnConfig};
use agent_of_empires::acp::agent_registry::AgentSpec;
use agent_of_empires::acp::state::{AcpSessionId, Event};

use crate::common::{shim_path, shim_ready};

fn spawn_config(
    shim: PathBuf,
    env: Vec<(String, String)>,
    stored_acp_session_id: Option<String>,
    default_effort: Option<String>,
) -> SpawnConfig {
    SpawnConfig {
        agent_key: "claude".into(),
        spec: AgentSpec {
            command: "node".into(),
            args: vec![shim.to_string_lossy().to_string()],
            description: "thought-level shim".into(),
            env_allowlist: None,
        },
        cwd: std::env::temp_dir(),
        additional_dirs: vec![],
        provider_env: env,
        host_environment: vec![],
        default_effort,
        default_mode: None,
        socket_path: None,
        stored_acp_session_id,
        fork_from: None,
        seed_history_replay: false,
        artifact_dir: None,
        sandbox_info: None,
        source_profile: None,
        mcp_servers: Vec::new(),
    }
}

/// A prompt is only dispatched after the handshake completes, so a `Stopped`
/// proves any post-handshake config-option work already ran.
async fn drive_one_turn(client: &mut AcpClient) {
    client
        .send_prompt("hello", &[])
        .await
        .expect("send_prompt should reach the shim");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), client.next_event()).await {
            Ok(Some(Event::Stopped { .. })) => break,
            Ok(_) | Err(_) => continue,
        }
    }
}

fn shim_env(record_path: &std::path::Path, load_session: bool) -> Vec<(String, String)> {
    let mut env = vec![
        ("SHIM_THOUGHT_LEVEL".into(), "1".into()),
        (
            "SHIM_CONFIG_OPTION_RECORD_FILE".into(),
            record_path.to_string_lossy().to_string(),
        ),
    ];
    if load_session {
        env.push(("SHIM_LOAD_SESSION".into(), "1".into()));
    }
    env
}

/// The respawn shape: the agent advertises `loadSession` and we hand it a
/// stored id, so the handshake resumes via `session/load`. The pinned effort
/// must still be applied, or the pick the user made before the restart is gone.
#[tokio::test]
async fn pinned_effort_applied_on_session_load() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let record_path = temp.path().join("config-option-calls.log");
    let config = spawn_config(
        shim_path(),
        shim_env(&record_path, true),
        Some("stored-effort-session".into()),
        Some("high".into()),
    );

    let mut client = AcpClient::spawn(config, AcpSessionId("effort-load".into()))
        .await
        .expect("spawn shim");
    drive_one_turn(&mut client).await;
    let _ = client.shutdown().await;

    let recorded = std::fs::read_to_string(&record_path).unwrap_or_default();
    assert!(
        recorded.lines().any(|line| line == "thought_level=high"),
        "a session/load respawn must re-apply the pinned effort (recorded: {recorded:?})"
    );
}

/// The fresh-session path keeps working, and applies the effort exactly once.
#[tokio::test]
async fn pinned_effort_applied_once_on_session_new() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let record_path = temp.path().join("config-option-calls.log");
    let config = spawn_config(
        shim_path(),
        shim_env(&record_path, false),
        None,
        Some("high".into()),
    );

    let mut client = AcpClient::spawn(config, AcpSessionId("effort-new".into()))
        .await
        .expect("spawn shim");
    drive_one_turn(&mut client).await;
    let _ = client.shutdown().await;

    let recorded = std::fs::read_to_string(&record_path).unwrap_or_default();
    assert_eq!(
        recorded
            .lines()
            .filter(|line| *line == "thought_level=high")
            .count(),
        1,
        "session/new must apply the effort exactly once (recorded: {recorded:?})"
    );
}

/// An unpinned session sends no config-option RPC at all, so it keeps whatever
/// the agent's own default is.
#[tokio::test]
async fn no_config_option_rpc_without_a_pinned_effort() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let record_path = temp.path().join("config-option-calls.log");
    let config = spawn_config(shim_path(), shim_env(&record_path, true), None, None);

    let mut client = AcpClient::spawn(config, AcpSessionId("effort-none".into()))
        .await
        .expect("spawn shim");
    drive_one_turn(&mut client).await;
    let _ = client.shutdown().await;

    let recorded = std::fs::read_to_string(&record_path).unwrap_or_default();
    assert!(
        recorded.trim().is_empty(),
        "no effort pinned means no set_config_option call (recorded: {recorded:?})"
    );
}
