//! A session's provider pick has to reach the adapter process on every
//! establish path, and it has to win.
//!
//! The pick travels as the Claude routing flags `CLAUDE_CODE_USE_BEDROCK` and
//! `CLAUDE_CODE_USE_VERTEX`. Two layers already claim those names: the host
//! environment aoe inherits, and trusted `Config.environment`, which
//! `apply_stdio_env` applies last precisely so it outranks request-sourced
//! env. A pick carried on either of those loses, so it rides its own
//! `SpawnConfig::provider_routing` layer applied after both. These tests drive
//! the real `AcpClient` against the test shim and read back the variables the
//! shim process actually received.

use std::collections::HashMap;
use std::time::Duration;

use agent_of_empires::acp::acp_client::{AcpClient, SpawnConfig};
use agent_of_empires::acp::agent_registry::AgentSpec;
use agent_of_empires::acp::state::{AcpSessionId, Event};

use crate::common::{shim_path, shim_ready, EnvGuard};

/// The flags `provider_override_env` produces, restated here because the
/// crate does not export it. Its own table test covers the mapping; what this
/// file is for is where the pairs land in the env layering.
fn routing_for(provider: Option<&str>) -> Vec<(String, String)> {
    let (bedrock, vertex) = match provider {
        Some("api") => ("", ""),
        Some("bedrock") => ("1", ""),
        Some("vertex") => ("", "1"),
        _ => return Vec::new(),
    };
    vec![
        ("CLAUDE_CODE_USE_BEDROCK".to_string(), bedrock.to_string()),
        ("CLAUDE_CODE_USE_VERTEX".to_string(), vertex.to_string()),
    ]
}

fn spawn_config(
    record_path: &std::path::Path,
    load_session: bool,
    provider: Option<&str>,
    host_environment: Vec<(String, String)>,
) -> SpawnConfig {
    let mut env = vec![(
        "SHIM_ENV_RECORD_FILE".to_string(),
        record_path.to_string_lossy().into_owned(),
    )];
    if load_session {
        env.push(("SHIM_LOAD_SESSION".into(), "1".into()));
    }
    SpawnConfig {
        wrapper_substitution: None,
        agent_key: "claude".into(),
        tool: "claude".into(),
        spec: AgentSpec {
            command: crate::common::shim_node()
                .expect("shim prerequisite")
                .to_string_lossy()
                .into_owned(),
            args: vec![shim_path().to_string_lossy().to_string()],
            description: "provider-routing shim".into(),
            env_allowlist: None,
        },
        cwd: std::env::temp_dir(),
        additional_dirs: vec![],
        provider_env: env,
        host_environment,
        default_effort_explicit: false,
        default_effort: None,
        default_mode: None,
        default_model: None,
        socket_path: None,
        stored_acp_session_id: load_session.then(|| "stored-provider".to_string()),
        fork_from: None,
        seed_history_replay: false,
        generation: 0,
        artifact_dir: None,
        sandbox_info: None,
        source_profile: None,
        mcp_servers: Vec::new(),
        claude_store_pin: None,
        base_host_environment: vec![],
        provider_routing: routing_for(provider),
    }
}

/// The handshake is complete once a prompt round-trips, and the shim records
/// its environment at startup, so the record is written by then.
async fn run(config: SpawnConfig, label: &str) {
    let mut client = AcpClient::spawn(config, AcpSessionId(label.into()))
        .await
        .unwrap_or_else(|e| panic!("{label}: spawn shim: {e}"));
    client
        .send_prompt("hello", &[])
        .await
        .expect("send_prompt should reach the shim");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(200), client.next_event()).await {
            Ok(Some(Event::Stopped { .. })) => {
                let _ = client.shutdown().await;
                return;
            }
            Ok(None) => panic!("{label}: ACP event stream closed before prompt completion"),
            Ok(Some(_)) => continue,
            Err(_) => continue,
        }
    }
    panic!("{label}: prompt did not complete after the handshake");
}

/// `None` for a variable the shim never saw, so "unset" stays distinguishable
/// from "set to empty", which is how a pick turns a provider off.
fn recorded(path: &std::path::Path) -> HashMap<String, Option<String>> {
    let raw = std::fs::read_to_string(path).expect("shim should record its environment");
    let first = raw.lines().next().expect("at least one record line");
    serde_json::from_str(first).expect("record line is JSON")
}

fn pair(value: &str) -> Vec<(String, String)> {
    vec![("CLAUDE_CODE_USE_VERTEX".to_string(), value.to_string())]
}

/// Every pick sets both flags on the fresh `session/new` path and on the
/// `session/load` resume a respawn takes, and an unpinned session is left
/// alone so it keeps today's host-driven behavior.
#[tokio::test]
#[serial_test::serial]
async fn provider_pick_reaches_the_adapter_on_load_and_new() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    // A profile with `inherit_host_environment` forwards the host's own
    // routing flags, so the unpinned case would read whatever the developer's
    // shell exports. Cleared here and restored on drop, which is why this test
    // is serial.
    let _env = EnvGuard::new(&["CLAUDE_CODE_USE_BEDROCK", "CLAUDE_CODE_USE_VERTEX"]);
    std::env::remove_var("CLAUDE_CODE_USE_BEDROCK");
    std::env::remove_var("CLAUDE_CODE_USE_VERTEX");
    // (pick, expected bedrock flag, expected vertex flag)
    let picks = [
        (Some("api"), Some(""), Some("")),
        (Some("bedrock"), Some("1"), Some("")),
        (Some("vertex"), Some(""), Some("1")),
        (None, None, None),
    ];
    for load in [false, true] {
        for (pick, bedrock, vertex) in picks {
            let label = format!(
                "{}-{}",
                pick.unwrap_or("unpinned"),
                if load { "load" } else { "new" }
            );
            let temp = tempfile::tempdir().expect("tempdir");
            let record_path = temp.path().join("env.json");
            run(spawn_config(&record_path, load, pick, vec![]), &label).await;

            let env = recorded(&record_path);
            assert_eq!(
                env["CLAUDE_CODE_USE_BEDROCK"].as_deref(),
                bedrock,
                "{label}: {env:?}"
            );
            assert_eq!(
                env["CLAUDE_CODE_USE_VERTEX"].as_deref(),
                vertex,
                "{label}: {env:?}"
            );
        }
    }
}

/// Trusted `Config.environment` is applied after the request's own env
/// precisely so it outranks it, which is why a pick carried there would be
/// beaten by an operator who pinned Vertex globally. The pick has to win, or a
/// session can never be moved off whatever the config says.
#[tokio::test]
#[serial_test::parallel]
async fn provider_pick_beats_configured_host_environment() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }
    // (pick, configured CLAUDE_CODE_USE_VERTEX, what the adapter must see)
    let cases = [
        (Some("api"), "1", ""),
        (Some("vertex"), "", "1"),
        (None, "1", "1"),
    ];
    for (pick, configured, expected) in cases {
        let label = format!("{}-over-{configured:?}", pick.unwrap_or("unpinned"));
        let temp = tempfile::tempdir().expect("tempdir");
        let record_path = temp.path().join("env.json");
        run(
            spawn_config(&record_path, false, pick, pair(configured)),
            &label,
        )
        .await;

        let env = recorded(&record_path);
        assert_eq!(
            env["CLAUDE_CODE_USE_VERTEX"].as_deref(),
            Some(expected),
            "{label}: {env:?}"
        );
    }
}
