//! Local clients share one private daemon startup path; discovery never spawns.

use std::sync::atomic::{AtomicBool, Ordering};

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
    #[error(
        "could not start a local aoe daemon: {0}\n\nStart one by hand with `aoe serve --core-only --daemon`, or attach to an existing remote daemon with:\n  AOE_DAEMON_URL=<url> AOE_DAEMON_TOKEN=<token> aoe …"
    )]
    AutostartFailed(String),
}

static AUTOSTART_DISABLED: AtomicBool = AtomicBool::new(false);

/// Set once at startup from `--no-autostart` / `AOE_NO_AUTOSTART`.
pub fn set_autostart_disabled(disabled: bool) {
    AUTOSTART_DISABLED.store(disabled, Ordering::Relaxed);
}

pub(crate) fn autostart_disabled() -> bool {
    AUTOSTART_DISABLED.load(Ordering::Relaxed)
}

/// Check the selected endpoint without spawning or changing exposure.
pub async fn locate_daemon() -> Result<DaemonEndpoint, ManagerError> {
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

/// Start a local daemon if needed; only its spawning caller receives the PID.
pub async fn ensure_daemon() -> Result<(DaemonEndpoint, Option<u32>), ManagerError> {
    if discover_env().is_some() || autostart_disabled() {
        return locate_daemon().await.map(|endpoint| (endpoint, None));
    }
    crate::cli::serve::ensure_local_daemon("", true)
        .await
        .map_err(|error| ManagerError::AutostartFailed(error.to_string()))
}

/// Announce CLI autostart on stderr, also under `--json`.
pub async fn require_daemon() -> Result<DaemonEndpoint, ManagerError> {
    let (endpoint, started) = ensure_daemon().await?;
    announce_start(started);
    Ok(endpoint)
}

/// Read scope is resolved from the snapshot, not the Daemon's startup profile.
pub(crate) async fn require_local_read_daemon() -> Result<DaemonEndpoint, ManagerError> {
    let allow_start = !autostart_disabled();
    let (endpoint, started) = async {
        let profile = if allow_start {
            let default = crate::session::config::resolve_default_profile();
            if crate::session::get_profile_dir_path(&default)?.try_exists()? {
                default
            } else {
                crate::session::list_profiles()?
                    .into_iter()
                    .next()
                    .unwrap_or(default)
            }
        } else {
            String::new()
        };
        crate::cli::serve::ensure_local_daemon(&profile, allow_start).await
    }
    .await
    .map_err(|error| ManagerError::AutostartFailed(error.to_string()))?;
    announce_start(started);
    Ok(endpoint)
}

fn announce_start(started: Option<u32>) {
    if let Some(pid) = started {
        eprintln!("Started aoe daemon (pid {pid})");
    }
}

/// Initial native bootstrap and explicit reconnect may start the local Daemon.
pub async fn ensure_native_daemon(profile: &str) -> anyhow::Result<DaemonEndpoint> {
    if discover_env().is_some() {
        return locate_daemon().await.map_err(Into::into);
    }
    crate::cli::serve::ensure_local_daemon(profile, !autostart_disabled())
        .await
        .map(|(endpoint, _)| endpoint)
}
