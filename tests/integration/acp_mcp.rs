//! End-to-end: the Rust ACP client forwards configured MCP servers to the
//! agent on `session/new`.
//!
//! The shim records the `mcp_servers` it receives to `SHIM_MCP_RECORD_FILE`
//! (passed through `provider_env`), so these tests assert AoE actually
//! populates the request rather than dropping the config on the floor.
//!
//! Skipped automatically if `node` is not on PATH.

use std::time::Duration;

use agent_of_empires::acp::acp_client::{AcpClient, SpawnConfig};
use agent_of_empires::acp::agent_registry::AgentSpec;
use agent_of_empires::acp::mcp_config;
use agent_of_empires::acp::state::AcpSessionId;
use agent_of_empires::session::mcp::mcp_model;

use crate::common::{shim_path, shim_ready};

/// Config with no MCP servers; callers set `mcp_servers` afterward so this
/// helper never has to name the schema's `McpServer` type.
fn base_config(cwd: std::path::PathBuf, record_path: &std::path::Path) -> SpawnConfig {
    let shim = shim_path();
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
            description: "test shim".into(),
            env_allowlist: None,
        },
        cwd,
        additional_dirs: vec![],
        provider_env: vec![(
            "SHIM_MCP_RECORD_FILE".into(),
            record_path.to_string_lossy().to_string(),
        )],
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

/// Read the shim's record file, retrying briefly: the shim writes it during
/// `newSession`, which completes inside `spawn`, but the write is async.
fn read_record(path: &std::path::Path) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(s) = std::fs::read_to_string(path) {
            if serde_json::from_str::<serde_json::Value>(&s).is_ok() {
                return s;
            }
        }
        if std::time::Instant::now() >= deadline {
            panic!("shim never wrote {}", path.display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[tokio::test]
#[serial_test::parallel]
async fn disabled_codex_server_does_not_reach_new_session() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }

    let home = tempfile::tempdir().unwrap();
    let codex_dir = home.path().join(".codex");
    std::fs::create_dir_all(&codex_dir).unwrap();
    std::fs::write(
        codex_dir.join("config.toml"),
        r#"
[mcp_servers.omitted]
command = "omitted"

[mcp_servers.explicit_true]
command = "true"
enabled = true

[mcp_servers.explicit_false]
command = "false"
enabled = false
"#,
    )
    .unwrap();
    let native = mcp_model::load_native_mcp_servers("codex", home.path()).unwrap();

    let record_dir = tempfile::tempdir().unwrap();
    let record_path = record_dir.path().join("record.json");
    let mut config = base_config(std::env::temp_dir(), &record_path);
    config.agent_key = "codex".into();
    config.mcp_servers = mcp_config::project_servers_to_acp(native);

    let client = AcpClient::spawn(config, AcpSessionId("mcp-codex-disabled".into()))
        .await
        .expect("spawn shim agent");

    let body = read_record(&record_path);
    let _ = client.shutdown().await;
    let parsed: serde_json::Value = serde_json::from_str(&body).expect("record is JSON");
    let arr = parsed.as_array().expect("mcp_servers is an array");
    let names = arr
        .iter()
        .map(|server| server["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["explicit_true", "omitted"], "got {body}");
}
