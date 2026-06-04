//! Locate the structured view daemon the caller should talk to.
//!
//! Rules:
//!
//! 1. If `AOE_DAEMON_URL` is set, point at it and health-check the
//!    endpoint. Never silently fall back to a local daemon: the whole
//!    point of the env override is to attach to a *specific* daemon.
//! 2. If a live local daemon exists (`serve.pid` + reachable
//!    `serve.url`), use it.
//! 3. Otherwise return [`ManagerError::NoDaemonRunning`] with an
//!    actionable hint. Auto-spawn is intentionally not provided:
//!    starting a loopback daemon by side-effect hides the choice
//!    between localhost, Tailscale, and Cloudflare from the user and
//!    leaves an `aoe serve` process behind that they did not ask for.
//!    The caller is expected to render the hint and bail.
//!
//! Build-namespace discipline (debug vs release) is enforced by
//! `crate::session::get_app_dir`: discovery reads `serve.pid` /
//! `serve.url` from the same app dir an `aoe serve` of the same build
//! would have written, so a debug client never picks up a release
//! daemon (or vice versa).

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
}

/// Locate the daemon the caller should talk to. Does *not* spawn one;
/// returns [`ManagerError::NoDaemonRunning`] if neither the env
/// override nor a live local daemon resolves, so the caller can
/// surface the message and let the user decide how to start the
/// server.
pub async fn require_daemon() -> Result<DaemonEndpoint, ManagerError> {
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
