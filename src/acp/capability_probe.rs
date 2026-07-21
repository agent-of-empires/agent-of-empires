//! Handshake-only ACP catalog probe (plugin-picker model-discovery fix).
//!
//! The structured-session model / mode / thought-level pickers are fed by the
//! `config_options` an agent advertises. Those are cached per-agent in
//! [`crate::acp::option_catalog`], but only ever written as a side effect of a
//! *live* session (`src/server/mod.rs` on `ConfigOptionsUpdated`). So an agent
//! that has never run shows an empty picker, which reads as a bug.
//!
//! ACP puts the option set in the `session/new` response itself
//! (`NewSessionResponse.config_options`; claude-agent-acp emits models + modes +
//! thought-levels there, see #1403), so we can populate the catalog without a
//! conversation: spawn the adapter, run initialize + `session/new` against a
//! throwaway cwd, record the first advertised snapshot, and tear the process
//! down. No prompt turn is sent, so no tokens are spent. The probe reuses
//! [`AcpClient::spawn`] on the in-process stdio transport (`socket_path: None`),
//! so it leaves no detached runner or worker-registry entry behind.
//!
//! Everything here degrades to "undiscovered" rather than failing: an unknown
//! agent, an absent adapter, a handshake that needs credentials the daemon does
//! not have, or an adapter that never advertises options all return
//! `Ok(false)`.

use std::time::Duration;

use crate::acp::acp_client::{AcpClient, SpawnConfig};
use crate::acp::state::{AcpSessionId, Event};
use crate::acp::AgentRegistry;

/// Whole-spawn bound: initialize + `session/new` must complete within this or
/// the adapter is treated as undiscovered.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait for the first `ConfigOptionsUpdated` after `session/new`.
/// claude queues it in the `session/new` response so it is usually immediate;
/// this only bounds adapters that push it as a follow-up notification.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// Probe `agent`'s advertised option catalog via a handshake-only ACP session
/// and record it into [`crate::acp::option_catalog`]. Returns `Ok(true)` when a
/// non-empty snapshot was recorded, `Ok(false)` when the agent is unknown, its
/// adapter is not installed, the handshake failed, or no options arrived in
/// time. Never sends a prompt turn.
pub async fn probe_agent(agent: &str) -> anyhow::Result<bool> {
    // Registry agents only. The picker is populated from the static registry,
    // and a custom agent's `agent_acp_cmd` can carry hostnames or secrets we
    // should not blind-spawn from a settings GET.
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

    // Throwaway absolute cwd: `session/new` requires an existing absolute
    // directory, but a handshake writes nothing to it. Dropped on return.
    let tmp = tempfile::tempdir()?;

    let config = SpawnConfig {
        agent_key: agent.to_string(),
        spec,
        cwd: tmp.path().to_path_buf(),
        additional_dirs: Vec::new(),
        // Same as the reconciler/session-spawn paths: auth comes from the
        // daemon's inherited environment, not this field.
        provider_env: Vec::new(),
        default_effort: None,
        default_mode: None,
        // In-process stdio: no detached runner, no persistent worker entry.
        socket_path: None,
        stored_acp_session_id: None,
        fork_from: None,
        sandbox_info: None,
        source_profile: None,
        mcp_servers: Vec::new(),
        seed_history_replay: false,
        artifact_dir: None,
    };

    // Probe-scoped id so it never collides with a real structured-view worker.
    let session_id = AcpSessionId(format!("__probe__{agent}"));

    let mut client =
        match tokio::time::timeout(SPAWN_TIMEOUT, AcpClient::spawn(config, session_id.clone()))
            .await
        {
            Ok(Ok(client)) => client,
            Ok(Err(e)) => {
                tracing::debug!(target: "acp.probe", agent, error = %e, "probe handshake failed");
                return Ok(false);
            }
            // ponytail: a wedged handshake leaks the child (no kill_on_drop on the
            // spawn), reaped at daemon exit. Acceptable on the settings probe path;
            // add kill_on_drop to spawn_subprocess if this ever bites.
            Err(_) => {
                tracing::debug!(target: "acp.probe", agent, "probe handshake timed out");
                return Ok(false);
            }
        };

    let recorded = tokio::time::timeout(DRAIN_TIMEOUT, drain_first_snapshot(&mut client, agent))
        .await
        .unwrap_or(false);

    // Best-effort teardown; both calls are internally bounded.
    let _ = client.delete_session(session_id.0.clone()).await;
    let _ = client.shutdown().await;
    Ok(recorded)
}

/// Drain events until the first non-empty `ConfigOptionsUpdated`, record it, and
/// return `true`. Returns `false` if the stream ends first.
async fn drain_first_snapshot(client: &mut AcpClient, agent: &str) -> bool {
    while let Some(event) = client.next_event().await {
        if let Event::ConfigOptionsUpdated { options } = event {
            if options.is_empty() {
                continue;
            }
            let agent = agent.to_string();
            let now = chrono::Utc::now().to_rfc3339();
            let _ = tokio::task::spawn_blocking(move || {
                crate::acp::option_catalog::record(&agent, &options, now)
            })
            .await;
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An agent that is not in the registry never spawns a process; the probe
    /// degrades to `Ok(false)` immediately. Keeps the happy path (which spawns a
    /// real adapter) out of the hermetic unit suite.
    #[tokio::test]
    async fn unknown_agent_never_spawns() {
        assert!(!probe_agent("definitely-not-an-agent-xyz")
            .await
            .expect("unknown agent is a clean no-op"));
    }
}
