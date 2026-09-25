//! Native endpoint discovery: explicit remote URL, otherwise the local Unix API.

use std::env;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::cli::serve::daemon_pid;
use crate::daemon::{DaemonClient, DaemonClientError};

#[derive(Clone)]
pub struct DaemonEndpoint {
    /// Browser-reachable dashboard origin used for plugin-relative links.
    ///
    /// This is deliberately independent from the API transport below: a
    /// core-only daemon has no TCP listener, and a remote daemon's API may be
    /// reached through a different origin than its dashboard.
    dashboard_url: Option<String>,
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
            // Legacy ACP HTTP helpers still use this field. Daemon RPCs use
            // `unix_path`; browser links use `dashboard_url` instead.
            base_url: "http://localhost".to_owned(),
            dashboard_url: crate::cli::serve::read_serve_urls()
                .into_iter()
                .next()
                .map(|entry| entry.url),
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
            dashboard_url: None,
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

    /// The browser origin for dashboard navigation, when this daemon exposes
    /// one. API transport and dashboard reachability are separate contracts.
    pub fn dashboard_url(&self) -> Option<&str> {
        self.dashboard_url.as_deref()
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
    Some(
        DaemonEndpoint::new(url.trim_end_matches('/').to_owned(), token, Source::Env)
            .with_dashboard_url(env::var("AOE_DASHBOARD_URL").ok()),
    )
}

impl DaemonEndpoint {
    fn with_dashboard_url(mut self, url: Option<String>) -> Self {
        self.dashboard_url = url
            .map(|url| url.trim().trim_end_matches('/').to_owned())
            .filter(|url| !url.is_empty());
        self
    }
}

pub fn discover_local() -> Result<DaemonEndpoint, DiscoveryError> {
    if daemon_pid().is_none() {
        return Err(DiscoveryError::NoLocalDaemon);
    }
    let path =
        crate::daemon::transport::local_socket_path().map_err(|_| DiscoveryError::LocalPath)?;
    Ok(DaemonEndpoint::local_unix(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    /// The dashboard origin is resolved from the running web daemon's own
    /// published URL, not from the Unix-socket transport placeholder, so a
    /// relative plugin link lands on the port the dashboard actually serves.
    #[test]
    #[serial]
    fn local_daemon_separates_transport_from_the_dashboard_origin() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _app_dir = crate::session::test_support::isolate_app_dir_at(temp.path());

        let core_only = DaemonEndpoint::local_unix(PathBuf::from("/tmp/aoe.sock"));
        assert!(
            core_only.dashboard_url().is_none(),
            "a core-only daemon serves no dashboard to navigate to"
        );

        let app_dir = crate::session::get_app_dir().expect("app dir");
        std::fs::write(
            app_dir.join("serve.url"),
            "http://127.0.0.1:7777/?token=secret\nlocalhost\thttp://127.0.0.1:7777/\n",
        )
        .expect("publish the web daemon URL");
        let web = DaemonEndpoint::local_unix(PathBuf::from("/tmp/aoe.sock"));
        assert_eq!(
            web.dashboard_url(),
            Some("http://127.0.0.1:7777/?token=secret")
        );
    }
}
