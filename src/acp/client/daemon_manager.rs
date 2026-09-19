//! Discovery never spawns; only initial TUI bootstrap and explicit reconnect may ensure a local core.

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

/// Called once at local TUI bootstrap, never by a reconnect timer.
pub async fn ensure_daemon(profile: &str) -> anyhow::Result<DaemonEndpoint> {
    if discover_env().is_some() {
        return require_daemon().await.map_err(Into::into);
    }
    crate::cli::serve::ensure_core_daemon(profile).await
}
