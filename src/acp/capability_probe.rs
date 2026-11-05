//! Disposable standalone ACP catalog probe (plugin-picker model-discovery fix).

use std::time::Duration;

use crate::acp::acp_client::{probe_option_catalog, SpawnConfig};
use crate::acp::AgentRegistry;

/// Whole-handshake bound: initialize + `session/new` must complete within this or
/// the adapter is treated as undiscovered.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for the first nonempty config snapshot after `session/new`.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Probe `agent`'s advertised option catalog via a disposable standalone ACP session
/// and record it into [`crate::acp::option_catalog`].
pub async fn probe_agent(agent: &str) -> anyhow::Result<bool> {
    // Registry agents only.
    let registry = AgentRegistry::with_defaults();
    let Some(mut spec) = registry.get(agent).cloned() else {
        return Ok(false);
    };
    // Absent adapter would just ENOENT at exec; skip the spawn entirely.
    if !crate::cli::acp::command_present(&spec.command) {
        return Ok(false);
    }
    if spec.command.contains("${aoe_data_dir}") {
        if let Ok(data_dir) = crate::session::get_app_dir() {
            spec.command = spec
                .command
                .replace("${aoe_data_dir}", &data_dir.to_string_lossy());
        }
    }

    // Throwaway absolute cwd: `session/new` requires an existing absolute directory.
    // This session has no managed identity and grants no filesystem or responder authority.
    let tmp = tempfile::tempdir()?;

    let config = SpawnConfig {
        launch_lease: None,
        wrapper_substitution: None,
        agent_key: agent.to_string(),
        tool: agent.to_string(),
        spec,
        cwd: tmp.path().to_path_buf(),
        additional_dirs: Vec::new(),
        // Same as the reconciler/session-spawn paths: auth comes from the
        // daemon's inherited environment, not this field.
        provider_env: Vec::new(),
        host_environment: Vec::new(),
        default_effort: None,
        default_effort_explicit: false,
        default_mode: None,
        default_model: None,
        // In-process stdio: no detached runner, no persistent worker entry.
        socket_path: None,
        stored_acp_session_id: None,
        fork_from: None,
        sandbox_info: None,
        source_profile: None,
        mcp_servers: Vec::new(),
        seed_history_replay: false,
        prior_conversation: false,
        generation: 0,
        artifact_dir: None,
        claude_store_pin: None,
        provider_routing: Vec::new(),
        base_host_environment: Vec::new(),
    };

    let options = match probe_option_catalog(config, SPAWN_TIMEOUT, DRAIN_TIMEOUT).await {
        Ok(Some(options)) => options,
        Ok(None) => return Ok(false),
        Err(error) => {
            tracing::debug!(target: "acp.probe", agent, error = %error, "probe handshake failed");
            return Ok(false);
        }
    };
    let agent = agent.to_string();
    let now = chrono::Utc::now().to_rfc3339();
    Ok(tokio::task::spawn_blocking(move || {
        crate::acp::option_catalog::record(&agent, &options, now).is_ok()
    })
    .await
    .unwrap_or(false))
}
