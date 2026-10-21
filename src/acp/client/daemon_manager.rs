//! Locate the structured view daemon the caller should talk to.
//!
//! Rules:
//!
//! 1. If `AOE_DAEMON_URL` is set, point at it and health-check the
//!    endpoint. Never silently fall back to a local daemon: the whole
//!    point of the env override is to attach to a *specific* daemon.
//! 2. If a live local daemon exists (`serve.pid` + reachable
//!    `serve.url`), use it.
//! 3. Otherwise start a detached loopback daemon, unless autostart is
//!    disabled (`--no-autostart`, `AOE_NO_AUTOSTART=1`), in which case
//!    return [`ManagerError::NoDaemonRunning`] with an actionable hint.
//!    [`locate_daemon`] never spawns.
//!
//! Build-namespace discipline (debug vs release) is enforced by
//! `crate::session::get_app_dir`: discovery reads `serve.pid` /
//! `serve.url` from the same app dir an `aoe serve` of the same build
//! would have written, so a debug client never picks up a release
//! daemon (or vice versa).

use std::sync::atomic::{AtomicBool, Ordering};

use thiserror::Error;

use super::autostart;
use super::discovery::{discover, discover_env, DaemonEndpoint, DiscoveryError};
use super::http::HttpError;

#[derive(Debug, Error)]
pub enum ManagerError {
    #[error(
        "AOE_DAEMON_URL is set but the daemon at that URL is unreachable; check the address or unset to use a local daemon"
    )]
    EnvOverrideUnreachable,
    #[error(
        "AOE_DAEMON_URL is set but the daemon rejected the bearer token; check AOE_DAEMON_TOKEN"
    )]
    EnvOverrideUnauthorized,
    /// No daemon was reachable and no env override was set. Carries
    /// the underlying discovery error so callers can distinguish "no
    /// `serve.pid` at all" from "stale PID" if they care; most
    /// callers just render the user-facing hint.
    #[error(
        "no structured view daemon is running.\n\nStart one with one of:\n  aoe serve --daemon                 (localhost only, recommended for solo dev)\n  aoe serve --daemon --remote        (Tailscale Funnel or Cloudflare quick tunnel)\n  aoe serve --daemon --tunnel-name … (named Cloudflare Tunnel)\n\nOr attach to an existing remote daemon with:\n  AOE_DAEMON_URL=<url> AOE_DAEMON_TOKEN=<token> aoe …"
    )]
    NoDaemonRunning(#[from] DiscoveryError),
    #[error(
        "could not start a local aoe daemon: {0}\n\nStart one by hand with `aoe serve --daemon`, or attach to an existing remote daemon with:\n  AOE_DAEMON_URL=<url> AOE_DAEMON_TOKEN=<token> aoe …"
    )]
    AutostartFailed(String),
}

static AUTOSTART_DISABLED: AtomicBool = AtomicBool::new(false);

/// Set once at startup from `--no-autostart` / `AOE_NO_AUTOSTART`.
pub fn set_autostart_disabled(disabled: bool) {
    AUTOSTART_DISABLED.store(disabled, Ordering::Relaxed);
}

/// Locate the daemon the caller should talk to. Never spawns one.
pub async fn locate_daemon() -> Result<DaemonEndpoint, ManagerError> {
    if discover_env().is_some() {
        // Resolve through `discover()` so the parsing/redaction
        // applied to local endpoints is also applied here, then
        // health-check before returning so callers don't bubble up
        // raw reqwest transport errors on every subsequent API call.
        let endpoint = discover().map_err(|_| ManagerError::EnvOverrideUnreachable)?;
        let client = super::HttpClient::new(endpoint.clone())
            .map_err(|_| ManagerError::EnvOverrideUnreachable)?;
        return match client.health_check().await {
            Ok(()) => Ok(endpoint),
            Err(HttpError::Unauthorized) => Err(ManagerError::EnvOverrideUnauthorized),
            Err(_) => Err(ManagerError::EnvOverrideUnreachable),
        };
    }
    discover().map_err(ManagerError::NoDaemonRunning)
}

/// Like [`locate_daemon`], but starts a local daemon when none answers. The pid
/// is `Some` only for the caller that spawned it, so each start is announced
/// once. A TUI caller shows it in the status line; stderr would corrupt the screen.
pub async fn ensure_daemon() -> Result<(DaemonEndpoint, Option<u32>), ManagerError> {
    if discover_env().is_some() || AUTOSTART_DISABLED.load(Ordering::Relaxed) {
        return locate_daemon().await.map(|endpoint| (endpoint, None));
    }
    if let Some(endpoint) = autostart::healthy_local().await {
        return Ok((endpoint, None));
    }
    autostart::start_local()
        .await
        .map_err(ManagerError::AutostartFailed)
}

/// [`ensure_daemon`] for the CLI: announces a start on stderr, also under `--json`.
pub async fn require_daemon() -> Result<DaemonEndpoint, ManagerError> {
    let (endpoint, started) = ensure_daemon().await?;
    if let Some(pid) = started {
        eprintln!("Started aoe daemon (pid {pid})");
    }
    Ok(endpoint)
}
