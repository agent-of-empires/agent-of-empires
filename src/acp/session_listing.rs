//! Native sessions a built-in ACP agent can list, for the import picker. The list spawn runs on
//! the host with the same `[environment]` and before_session env as a session spawn, so it reads
//! the store a later `session/load` will use.

use crate::acp::acp_client::{list_native_sessions, ListSessionsError, SpawnConfig};
use crate::acp::{AgentRegistry, AgentSpec};
use crate::session::import::{ImportableList, Owned};

pub async fn list_agent_sessions(
    agent: &str,
    profile: &str,
    owned: &Owned,
) -> Result<ImportableList, ListSessionsError> {
    let Some(mut spec) = AgentRegistry::with_defaults().get(agent).cloned() else {
        return Err(ListSessionsError::UnknownAgent);
    };
    if !crate::cli::acp::command_present(&spec.command) {
        return Err(ListSessionsError::NotInstalled);
    }
    if spec.command.contains("${aoe_data_dir}") {
        let data_dir = crate::session::get_app_dir()
            .map_err(|e| ListSessionsError::Failed(format!("app dir: {e}")))?;
        spec.command = spec
            .command
            .replace("${aoe_data_dir}", &data_dir.to_string_lossy());
    }
    let profile_owned = profile.to_string();
    let cfg = tokio::task::spawn_blocking(move || {
        crate::session::config::profile_config::resolve_config_or_warn(&profile_owned)
    })
    .await
    .map_err(|e| ListSessionsError::Failed(format!("config load task failed: {e}")))?;
    list_with_spec(agent, spec, &cfg, profile, owned).await
}

async fn list_with_spec(
    agent: &str,
    spec: AgentSpec,
    cfg: &crate::session::Config,
    profile: &str,
    owned: &Owned,
) -> Result<ImportableList, ListSessionsError> {
    // `session/list` touches no cwd; the spawn only needs an existing one.
    let tmp = tempfile::tempdir().map_err(|e| ListSessionsError::Failed(e.to_string()))?;
    let (base_host_environment, host_environment) = crate::acp::supervisor::host_spawn_environment(
        cfg,
        &format!("__list__{agent}"),
        agent,
        profile.to_string(),
        tmp.path().to_path_buf(),
    )
    .await
    .map_err(|e| ListSessionsError::Failed(e.to_string()))?;
    let config = SpawnConfig {
        wrapper_substitution: None,
        agent_key: agent.to_string(),
        tool: agent.to_string(),
        spec,
        cwd: tmp.path().to_path_buf(),
        additional_dirs: Vec::new(),
        provider_env: Vec::new(),
        provider_routing: Vec::new(),
        host_environment,
        default_effort: None,
        default_effort_explicit: false,
        default_mode: None,
        default_model: None,
        socket_path: None,
        stored_acp_session_id: None,
        fork_from: None,
        sandbox_info: None,
        source_profile: Some(profile.to_string()),
        mcp_servers: Vec::new(),
        seed_history_replay: false,
        generation: 0,
        artifact_dir: None,
        claude_store_pin: None,
        base_host_environment,
    };
    list_native_sessions(config, owned).await
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn list_spawn_sees_configured_host_environment() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("agent.sh");
        std::fs::write(
            &script,
            r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -En 's/.*"id":("[^"]*"|[0-9]+).*/\1/p')
  case $line in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true,"sessionCapabilities":{"list":{}}}}}\n' "$id" ;;
    *'"method":"session/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"sessions":[{"sessionId":"%s","cwd":"%s"}]}}\n' "$id" "$PI_ACP_PI_COMMAND" "$PI_CODING_AGENT_DIR" ;;
  esac
done
"#,
        )
        .unwrap();
        let spec = AgentSpec {
            command: "/bin/sh".into(),
            args: vec![script.to_string_lossy().into_owned()],
            description: "list env stub".into(),
            env_allowlist: None,
        };
        let cfg = crate::session::Config {
            environment: vec![
                "PI_CODING_AGENT_DIR=/stores/pi".into(),
                "PI_ACP_PI_COMMAND=pi-wrapper".into(),
            ],
            ..Default::default()
        };
        let list = list_with_spec("pi", spec, &cfg, "", &Owned::default())
            .await
            .unwrap();
        assert_eq!(list.sessions[0].session_id, "pi-wrapper");
        assert_eq!(list.sessions[0].cwd, "/stores/pi");
    }
}
