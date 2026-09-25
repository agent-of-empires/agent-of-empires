//! Discovery never spawns; only initial TUI bootstrap may ensure the local daemon.

use thiserror::Error;

use super::discovery::{discover, discover_env, DaemonEndpoint, DiscoveryError};
use super::http::HttpError;

#[derive(Debug, Error)]
pub enum ManagerError {
    #[error(
        "AOE_DAEMON_URL is set but the daemon at that URL is unreachable; check the address or unset to use a local daemon"
    )]
    EnvOverrideUnreachable,
    #[error(
        "AOE_DAEMON_URL is set but authentication was rejected; check AOE_DAEMON_TOKEN and daemon policy"
    )]
    EnvOverrideUnauthorized,
    #[error("local daemon unavailable: {0}")]
    NoDaemonRunning(#[from] DiscoveryError),
}

/// This machine's own daemon, ignoring `AOE_DAEMON_URL`. The TUI home view
/// acts on local sessions and lists the env daemon as a remote instead.
pub fn require_local_daemon() -> Result<DaemonEndpoint, ManagerError> {
    super::discovery::discover_local().map_err(ManagerError::NoDaemonRunning)
}

/// Check the selected endpoint without spawning or changing exposure.
pub async fn require_daemon() -> Result<DaemonEndpoint, ManagerError> {
    if discover_env().is_some() {
        let endpoint = discover().map_err(|_| ManagerError::EnvOverrideUnreachable)?;
        let client = super::HttpClient::new(endpoint.clone())
            .map_err(|_| ManagerError::EnvOverrideUnreachable)?;
        return match client.health_check().await {
            Ok(()) => Ok(endpoint),
            Err(HttpError::Unauthorized) => Err(ManagerError::EnvOverrideUnauthorized),
            Err(_) => Err(ManagerError::EnvOverrideUnreachable),
        };
    }
    let endpoint = discover().map_err(ManagerError::NoDaemonRunning)?;
    let client = super::HttpClient::new(endpoint.clone())
        .map_err(|_| ManagerError::NoDaemonRunning(DiscoveryError::Unreachable))?;
    client
        .health_check()
        .await
        .map_err(|_| ManagerError::NoDaemonRunning(DiscoveryError::Unreachable))?;
    Ok(endpoint)
}

/// Called once at local TUI bootstrap, never by a reconnect. Always this
/// machine's localhost daemon: the TUI lists `AOE_DAEMON_URL` as a remote instead.
pub async fn ensure_local_daemon(profile: &str) -> anyhow::Result<DaemonEndpoint> {
    crate::cli::serve::ensure_local_daemon(profile).await
}
