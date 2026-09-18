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
    /// Passphrase-login session for a daemon behind a login wall. Travels
    /// alongside the bearer token: `--remote` daemons require both.
    login: Option<crate::daemon::SessionCredential>,
    /// Credentials may travel over non-loopback plain HTTP. Only a registry
    /// entry added with `--insecure` sets it.
    allow_plaintext: bool,
}

impl std::fmt::Debug for DaemonEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonEndpoint")
            .field("source", &self.source)
            .field("authenticated", &(self.has_token() || self.login.is_some()))
            .field("allow_plaintext", &self.allow_plaintext)
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
            login: None,
            allow_plaintext: false,
        }
    }

    /// Changes whenever the address or any credential does, so a caller can
    /// tell a re-paired entry from the one a daemon refused.
    pub(crate) fn credential_fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.base_url.hash(&mut hasher);
        self.token.hash(&mut hasher);
        self.login
            .as_ref()
            .map(|login| (&login.session, &login.binding))
            .hash(&mut hasher);
        self.allow_plaintext.hash(&mut hasher);
        hasher.finish()
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
            login: None,
            allow_plaintext: false,
        }
    }

    /// Attach a passphrase-login credential read from the remote registry.
    pub(crate) fn with_login(mut self, login: Option<crate::daemon::SessionCredential>) -> Self {
        self.login = login;
        self
    }

    pub(crate) fn login(&self) -> Option<&crate::daemon::SessionCredential> {
        self.login.as_ref()
    }

    /// Permit credentials over non-loopback plain HTTP, per the registry entry.
    pub(crate) fn with_plaintext_allowed(mut self, allow: bool) -> Self {
        self.allow_plaintext = allow;
        self
    }

    pub(crate) fn allows_plaintext(&self) -> bool {
        self.allow_plaintext
    }

    pub fn daemon_client(&self) -> Result<DaemonClient, DaemonClientError> {
        if let Some(path) = &self.unix_path {
            DaemonClient::new_unix(path)
        } else {
            DaemonClient::with_login(
                &self.base_url,
                self.bearer_token(),
                self.login.as_ref(),
                self.allow_plaintext,
            )
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
    /// An entry in the `aoe remote` registry.
    Remote,
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
