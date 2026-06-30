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
use agent_of_empires::session::mcp_model::{self, McpLayer, McpProvenance};

use crate::common::{shim_path, shim_ready};

/// Config with no MCP servers; callers set `mcp_servers` afterward so this
/// helper never has to name the schema's `McpServer` type.
fn base_config(cwd: std::path::PathBuf, record_path: &std::path::Path) -> SpawnConfig {
    let shim = shim_path();
    SpawnConfig {
        agent_key: "claude".into(),
        spec: AgentSpec {
            command: "node".into(),
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
        default_effort: None,
        socket_path: None,
        stored_acp_session_id: None,
        fork_from: None,
        seed_history_replay: false,
        artifact_dir: None,
        sandbox_info: None,
        source_profile: None,
        mcp_servers: Vec::new(),
    }
}

/// Read the shim's record file, retrying briefly: the shim writes it during
/// `newSession`, which completes inside `spawn`, but the write is async.
fn read_record(path: &std::path::Path) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(s) = std::fs::read_to_string(path) {
            return s;
        }
        if std::time::Instant::now() >= deadline {
            panic!("shim never wrote {}", path.display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[tokio::test]
async fn configured_mcp_servers_reach_new_session() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }

    // Resolve servers the same way the supervisor does: a global mcp.json.
    let app_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        app_dir.path().join("mcp.json"),
        r#"{ "mcpServers": { "probe": { "command": "echo", "args": ["hi"] } } }"#,
    )
    .unwrap();
    let servers = mcp_model::load_global_mcp_servers(app_dir.path()).unwrap();
    assert_eq!(servers.len(), 1, "fixture should parse one server");

    // A tempdir + plain path, not NamedTempFile: the shim writes this file from
    // a separate process, so we must not hold an open handle to it.
    let record_dir = tempfile::tempdir().unwrap();
    let record_path = record_dir.path().join("record.json");
    let mut config = base_config(std::env::temp_dir(), &record_path);
    config.mcp_servers = mcp_config::project_servers_to_acp(servers);

    let client = AcpClient::spawn(config, AcpSessionId("mcp-forward".into()))
        .await
        .expect("spawn shim agent");

    let body = read_record(&record_path);
    let _ = client.shutdown().await;

    let parsed: serde_json::Value = serde_json::from_str(&body).expect("record is JSON");
    let arr = parsed.as_array().expect("mcp_servers is an array");
    assert_eq!(arr.len(), 1, "expected one forwarded server, got {body}");
    assert_eq!(arr[0]["name"], "probe", "forwarded server name, got {body}");
    assert_eq!(arr[0]["command"], "echo", "forwarded command, got {body}");
}

#[tokio::test]
async fn native_and_global_merge_reaches_new_session() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }

    // Native layer (lowest precedence): the agent's own `~/.claude.json`. It
    // defines "shared" (collides with global) and "native-only".
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join(".claude.json"),
        r#"{ "mcpServers": {
            "shared": { "command": "from-native" },
            "native-only": { "command": "n" }
        } }"#,
    )
    .unwrap();
    let native = mcp_model::load_native_mcp_servers("claude", home.path()).unwrap();

    // Global layer (higher precedence): `<app_dir>/mcp.json`. It overrides
    // "shared" and adds "global-only".
    let app_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        app_dir.path().join("mcp.json"),
        r#"{ "mcpServers": {
            "shared": { "command": "from-global" },
            "global-only": { "command": "g" }
        } }"#,
    )
    .unwrap();
    let global = mcp_model::load_global_mcp_servers(app_dir.path()).unwrap();

    let merged = mcp_model::resolve(vec![
        McpLayer {
            provenance: McpProvenance::AgentNative {
                agent: "claude".into(),
            },
            servers: native,
        },
        McpLayer {
            provenance: McpProvenance::Global,
            servers: global,
        },
    ]);

    let record_dir = tempfile::tempdir().unwrap();
    let record_path = record_dir.path().join("record.json");
    let mut config = base_config(std::env::temp_dir(), &record_path);
    config.mcp_servers =
        mcp_config::project_servers_to_acp(merged.into_iter().map(|s| s.def).collect());

    let client = AcpClient::spawn(config, AcpSessionId("mcp-native-merge".into()))
        .await
        .expect("spawn shim agent");

    let body = read_record(&record_path);
    let _ = client.shutdown().await;

    let parsed: serde_json::Value = serde_json::from_str(&body).expect("record is JSON");
    let arr = parsed.as_array().expect("mcp_servers is an array");
    assert_eq!(
        arr.len(),
        3,
        "expected merged union of three servers, got {body}"
    );

    let shared = arr
        .iter()
        .find(|s| s["name"] == "shared")
        .expect("shared server present");
    assert_eq!(
        shared["command"], "from-global",
        "global must override native on name collision, got {body}"
    );
    assert!(
        arr.iter().any(|s| s["name"] == "native-only"),
        "native-only server must survive the merge, got {body}"
    );
    assert!(
        arr.iter().any(|s| s["name"] == "global-only"),
        "global-only server must survive the merge, got {body}"
    );
}

#[tokio::test]
async fn no_config_forwards_empty_list() {
    if let Err(reason) = shim_ready() {
        eprintln!("skipping: {reason}");
        return;
    }

    // No mcp.json in the app dir => empty list, unchanged from pre-feature.
    let app_dir = tempfile::tempdir().unwrap();
    let servers = mcp_model::load_global_mcp_servers(app_dir.path()).unwrap();
    assert!(servers.is_empty());

    let record_dir = tempfile::tempdir().unwrap();
    let record_path = record_dir.path().join("record.json");
    let mut config = base_config(std::env::temp_dir(), &record_path);
    config.mcp_servers = mcp_config::project_servers_to_acp(servers);

    let client = AcpClient::spawn(config, AcpSessionId("mcp-empty".into()))
        .await
        .expect("spawn shim agent");

    let body = read_record(&record_path);
    let _ = client.shutdown().await;

    let parsed: serde_json::Value = serde_json::from_str(&body).expect("record is JSON");
    assert_eq!(
        parsed.as_array().map(|a| a.len()),
        Some(0),
        "expected empty mcp_servers, got {body}"
    );
}
