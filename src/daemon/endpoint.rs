//! Resolved Daemon transport and dashboard coordinates, independent of host discovery.

use std::path::{Path, PathBuf};

use super::{DaemonClient, DaemonClientError};

#[derive(Clone)]
pub struct DaemonEndpoint {
    /// Browser-reachable dashboard origin, independent of the API transport.
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
    /// Local Unix transport with an already-resolved dashboard origin, if any.
    /// Peer ownership authorizes it, so it carries no bearer token.
    pub fn local_unix(path: PathBuf, dashboard_url: Option<String>) -> Self {
        Self {
            // HTTP request targets use this placeholder, not the dashboard origin.
            base_url: "http://localhost".to_owned(),
            dashboard_url,
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

    /// Browser origin for dashboard navigation, separate from API reachability.
    pub fn dashboard_url(&self) -> Option<&str> {
        self.dashboard_url.as_deref()
    }

    pub(crate) fn with_dashboard_url(mut self, url: Option<String>) -> Self {
        self.dashboard_url = url;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Env,
    LocalDaemon,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_omits_credentials_and_transport_coordinates() {
        let endpoints = [
            DaemonEndpoint::new(
                "https://private-api.example".into(),
                Some("private-bearer".into()),
                Source::Env,
            )
            .with_dashboard_url(Some(
                "https://private-dashboard.example/?token=private-link".into(),
            )),
            DaemonEndpoint::local_unix(
                PathBuf::from("/private-socket/api.sock"),
                Some("https://private-dashboard.example".into()),
            ),
        ];
        for endpoint in endpoints {
            let debug = format!("{endpoint:?}");
            for secret in [
                "private-api",
                "private-bearer",
                "private-dashboard",
                "private-link",
                "private-socket",
            ] {
                assert!(!debug.contains(secret));
            }
        }
    }
}
