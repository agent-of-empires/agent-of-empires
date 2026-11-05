//! End-to-end: the Rust ACP client forwards configured MCP servers to the
//! agent on `session/new`.
//!
//! The shim records the `mcp_servers` it receives to `SHIM_MCP_RECORD_FILE`
//! (passed through `provider_env`), so these tests assert AoE actually
//! populates the request rather than dropping the config on the floor.
//!
//! Skipped automatically if `node` is not on PATH.

use std::time::Duration;

use crate::acp::acp_client::SpawnConfig;
use crate::acp::agent_registry::AgentSpec;
use crate::acp::mcp_config;
use crate::session::mcp::mcp_model::{self, McpProvenance, ResolvedMcpServer};
use crate::session::mcp::project_mcp::parse_standard_mcp_servers;

use super::managed::{shim_path, shim_ready};

/// Config with no MCP servers; callers set `mcp_servers` afterward so this
/// helper never has to name the schema's `McpServer` type.
fn base_config(cwd: std::path::PathBuf, record_path: &std::path::Path) -> SpawnConfig {
    let shim = shim_path();
    SpawnConfig {
        launch_lease: None,
        provider_routing: Vec::new(),
        wrapper_substitution: None,
        agent_key: "claude".into(),
        tool: "claude".into(),
        spec: AgentSpec {
            command: super::managed::shim_node()
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
        prior_conversation: false,
        generation: 0,
        artifact_dir: None,
        sandbox_info: None,
        source_profile: None,
        mcp_servers: Vec::new(),
        claude_store_pin: None,
        base_host_environment: vec![],
    }
}

/// Read the shim's record file, retrying briefly: the shim writes it during
/// `newSession`, which completes inside `spawn`, but the write is async.
async fn read_record(path: &std::path::Path) -> String {
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
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
#[serial_test::serial]
async fn disabled_codex_server_does_not_reach_new_session() {
    super::managed::with_managed(|scope| {
        Box::pin(async move {
            if let Err(reason) = shim_ready() {
                eprintln!("skipping: {reason}");
                return;
            }

            let home = scope.tempdir();
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

            let record_dir = scope.tempdir();
            let record_path = record_dir.path().join("record.json");
            let mut config = base_config(std::env::temp_dir(), &record_path);
            config.agent_key = "codex".into();
            config.mcp_servers = mcp_config::project_servers_to_acp(
                native
                    .into_iter()
                    .map(|def| ResolvedMcpServer {
                        def,
                        provenance: McpProvenance::AgentNative {
                            agent: "codex".into(),
                        },
                        shadowed: Vec::new(),
                    })
                    .collect(),
                &[],
            );

            let client = scope
                .spawn(config, "mcp-codex-disabled")
                .await
                .expect("spawn shim agent");
            client.ready().await;

            let body = read_record(&record_path).await;
            let _ = client.shutdown().await;
            let parsed: serde_json::Value = serde_json::from_str(&body).expect("record is JSON");
            let arr = parsed.as_array().expect("mcp_servers is an array");
            let names = arr
                .iter()
                .map(|server| server["name"].as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(names, vec!["explicit_true", "omitted"], "got {body}");
        })
    })
    .await;
}

#[tokio::test]
#[serial_test::serial]
async fn new_session_receives_expanded_and_unresolved_mcp_auth_headers() {
    super::managed::with_managed(|scope| {
        Box::pin(async move {
            if let Err(reason) = shim_ready() {
                eprintln!("skipping: {reason}");
                return;
            }

            let servers = parse_standard_mcp_servers(
                r#"{"mcpServers":{
        "missing":{"type":"http","url":"https://missing.mcp.invalid","headers":{
            "Authorization":"Bearer ${MISSING_TOKEN}",
            "X-Reference":"${MISSING_TOKEN}"
        }},
        "resolved":{"type":"http","url":"https://resolved.mcp.invalid","headers":{
            "Authorization":"Bearer ${GITHUB_TOKEN}"
        }}
    }}"#,
            )
            .unwrap()
            .into_iter()
            .map(|def| ResolvedMcpServer {
                def,
                provenance: McpProvenance::Profile {
                    name: "test".into(),
                },
                shadowed: Vec::new(),
            })
            .collect();

            let record_dir = scope.tempdir();
            let record_path = record_dir.path().join("record.json");
            let session_env = vec![("GITHUB_TOKEN".into(), "synthetic-token".into())];
            let mut config = base_config(std::env::temp_dir(), &record_path);
            config
                .provider_env
                .push(("SHIM_MCP_CAPABILITY".into(), "http".into()));
            config.host_environment = session_env.clone();
            config.mcp_servers = mcp_config::project_servers_to_acp(servers, &session_env);

            let client = scope
                .spawn(config, "mcp-header-expansion")
                .await
                .expect("spawn shim agent");
            client.ready().await;

            let body = read_record(&record_path).await;
            let _ = client.shutdown().await;
            let servers: serde_json::Value =
                serde_json::from_str(&body).expect("mcp servers are JSON");
            let servers = servers.as_array().expect("mcp servers are an array");
            let resolved = servers
                .iter()
                .find(|server| server["name"] == "resolved")
                .expect("resolved server reached session/new");
            assert_eq!(
                resolved["headers"][0]["value"], "Bearer synthetic-token",
                "{body}"
            );
            let missing = servers
                .iter()
                .find(|server| server["name"] == "missing")
                .expect("server with missing auth remains in session/new");
            assert_eq!(missing["headers"].as_array().unwrap().len(), 1, "{body}");
            assert_eq!(missing["headers"][0]["name"], "X-Reference");
            assert_eq!(missing["headers"][0]["value"], "${MISSING_TOKEN}");
        })
    })
    .await;
}
