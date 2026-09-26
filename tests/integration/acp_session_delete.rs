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

use agent_of_empires::acp::acp_client::{AcpClient, SpawnConfig};
use agent_of_empires::acp::agent_registry::AgentSpec;
use agent_of_empires::acp::state::{AcpSessionId, Event};

use crate::common::{shim_path, shim_ready};

fn spawn_config_with_shim_env(shim: PathBuf, env: Vec<(String, String)>) -> SpawnConfig {
    SpawnConfig {
        wrapper_substitution: None,
        agent_key: "claude".into(),
        tool: "claude".into(),
        spec: AgentSpec {
            command: crate::common::shim_node()
                .expect("shim prerequisite")
                .to_string_lossy()
                .into_owned(),
            args: vec![shim.to_string_lossy().to_string()],
            description: "session/delete shim".into(),
            env_allowlist: None,
        },
        cwd: std::env::temp_dir(),
        additional_dirs: vec![],
        provider_env: env,
        host_environment: vec![],
        default_effort: None,
        default_effort_explicit: false,
        default_mode: None,
        default_model: None,
        socket_path: None,
        stored_acp_session_id: None,
        fork_from: None,
        seed_history_replay: false,
        generation: 0,
        artifact_dir: None,
        sandbox_info: None,
        source_profile: None,
        mcp_servers: Vec::new(),
        claude_store_pin: None,
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

/// (case, shim env, expected outcome). The slow shim sleeps 3s against the 2s
/// `ACP_SESSION_DELETE_TIMEOUT` plus a 500ms outer guard, so every call must
/// return before the 3.4s bound.
#[tokio::test]
#[serial_test::parallel]
async fn session_delete_outcome_follows_the_adapter() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    let cases: [(&str, &[(&str, &str)], &str); 3] = [
        ("advertised", &[("SHIM_DELETE_CAPABILITY", "1")], "Deleted"),
        // Without the capability the SDK dispatcher returns -32601, the
        // steady-state shape for aoe-agent, codex and opencode.
        ("absent", &[], "UnsupportedMethod"),
        (
            "slow",
            &[
                ("SHIM_DELETE_CAPABILITY", "1"),
                ("SHIM_DELETE_MODE", "slow"),
            ],
            "TimedOut",
        ),
    ];
    for (case, shim_env, expected) in cases {
        let temp = tempfile::tempdir().expect("tempdir");
        let record_path = temp.path().join("delete-calls.log");
        let mut env: Vec<(String, String)> = shim_env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        env.push((
            "SHIM_DELETE_RECORD_FILE".into(),
            record_path.to_string_lossy().to_string(),
        ));
        let config = spawn_config_with_shim_env(shim_path(), env);
        let mut client = AcpClient::spawn(config, AcpSessionId(format!("delete-{case}")))
            .await
            .expect("spawn shim");

        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let acp_id = drive_handshake_and_capture_session_id(&mut client, deadline)
            .await
            .expect("shim should assign an ACP session id during handshake");

        let started = std::time::Instant::now();
        let outcome = client.delete_session(acp_id.clone()).await;
        let elapsed = started.elapsed();
        let _ = client.shutdown().await;

        assert_eq!(format!("{outcome:?}"), expected, "{case}");
        assert!(
            elapsed < Duration::from_millis(3400),
            "{case}: delete_session should bound the wait; took {elapsed:?}"
        );
        if expected == "Deleted" {
            let recorded = std::fs::read_to_string(&record_path).expect("record file");
            assert!(
                recorded.lines().any(|line| line == acp_id),
                "shim should record the deleted id {acp_id}: {recorded:?}"
            );
        }
    }
}
