//! Native endpoint discovery: explicit remote URL, otherwise the local Unix API.

use std::env;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::cli::serve::daemon_pid;
use crate::daemon::{DaemonClient, DaemonClientError};

#[derive(Clone)]
pub struct DaemonEndpoint {
    pub base_url: String,
    token: Option<String>,
    pub source: Source,
    unix_path: Option<PathBuf>,
}

impl std::fmt::Debug for DaemonEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonEndpoint")
            .field("source", &self.source)
            .field("authenticated", &self.has_token())
            .finish_non_exhaustive()
    }
}

impl DaemonEndpoint {
    /// Endpoint for a daemon reachable over its local unix socket. The peer
    /// owner authorizes it, so it carries no bearer token.
    pub fn local_unix(path: PathBuf) -> Self {
        Self {
            base_url: "http://localhost".to_owned(),
            token: None,
            source: Source::LocalDaemon,
            unix_path: Some(path),
        }
    }

    pub(crate) fn unix_path(&self) -> Option<&Path> {
        self.unix_path.as_deref()
    }

    pub(crate) fn new(base_url: String, token: Option<String>, source: Source) -> Self {
        Self {
            base_url,
            token,
            source,
            unix_path: None,
        }
    }

    pub fn daemon_client(&self) -> Result<DaemonClient, DaemonClientError> {
        if let Some(path) = &self.unix_path {
            DaemonClient::new_unix(path)
        } else {
            DaemonClient::new(&self.base_url, self.bearer_token())
        }
    }

    pub(crate) fn bearer_token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    pub(crate) fn has_token(&self) -> bool {
        self.token.is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Env,
    LocalDaemon,
}

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("no local daemon is running; start one with `aoe serve --core-only --daemon`")]
    NoLocalDaemon,
    #[error("the local daemon socket path could not be resolved")]
    LocalPath,
    #[error("the local daemon is not ready; reconnect explicitly after resolving its failure")]
    Unreachable,
}

pub fn discover() -> Result<DaemonEndpoint, DiscoveryError> {
    discover_env().map_or_else(discover_local, Ok)
}

pub fn discover_env() -> Option<DaemonEndpoint> {
    let url = env::var("AOE_DAEMON_URL").ok()?;
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    let token = env::var("AOE_DAEMON_TOKEN")
        .ok()
        .map(|token| token.trim().to_owned())
        .filter(|token| !token.is_empty());
    Some(DaemonEndpoint::new(
        url.trim_end_matches('/').to_owned(),
        token,
        Source::Env,
    ))
}

pub fn discover_local() -> Result<DaemonEndpoint, DiscoveryError> {
    if daemon_pid().is_none() {
        return Err(DiscoveryError::NoLocalDaemon);
    }
    let path =
        crate::daemon::transport::local_socket_path().map_err(|_| DiscoveryError::LocalPath)?;
    Ok(DaemonEndpoint::local_unix(path))
}
