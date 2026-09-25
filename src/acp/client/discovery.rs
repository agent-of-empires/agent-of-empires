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

/// How long an unused client is kept. Long enough to span the sidebar's poll
/// and a user pausing on a session, short enough that disabling a remote
/// eventually releases its connection.
const POOL_IDLE: std::time::Duration = std::time::Duration::from_secs(600);

struct PooledClient {
    /// Rebuild rather than reuse once the endpoint's credentials change.
    fingerprint: u64,
    client: DaemonClient,
    used: std::time::Instant,
}

/// Clients by base URL, or by socket path for a unix endpoint. Keyed by
/// address rather than by remote name so the same daemon reached under two
/// names shares one pool.
fn client_pool() -> &'static std::sync::Mutex<std::collections::HashMap<String, PooledClient>> {
    static POOL: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, PooledClient>>,
    > = std::sync::OnceLock::new();
    POOL.get_or_init(Default::default)
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

    /// The base a browser can reach, for a plugin's relative href or an
    /// "open in browser" action. A unix endpoint's `base_url` is only a host
    /// for the request line, so the daemon's published address is used
    /// instead; without one there is nothing better than the placeholder.
    pub(crate) fn browser_base_url(&self) -> String {
        if self.unix_path.is_none() {
            return self.base_url.clone();
        }
        crate::cli::serve::read_serve_urls()
            .first()
            .map(|entry| {
                entry
                    .url
                    .split('?')
                    .next()
                    .unwrap_or(&entry.url)
                    .trim_end_matches('/')
                    .to_string()
            })
            .filter(|base| !base.is_empty())
            .unwrap_or_else(|| self.base_url.clone())
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

    /// A client for this endpoint, reused across calls.
    ///
    /// Building one is not cheap: `reqwest` loads and parses the system CA
    /// store on every `build()` (~15ms here), and the fresh client also
    /// arrives with an empty connection pool, so a caller that builds one per
    /// request pays a full TLS handshake every time. Clones share the pool, so
    /// a cached client keeps the connection to a remote daemon warm between
    /// polls. The entry is rebuilt when the endpoint's credentials change, so
    /// a re-paired remote never keeps talking with the old ones.
    pub fn daemon_client(&self) -> Result<DaemonClient, DaemonClientError> {
        // A unix endpoint has no credentials to rotate, so its entry is keyed
        // by the socket path and never invalidated. Its requests dial the
        // socket fresh each time, so a cached client holds nothing stale.
        let (key, fingerprint) = match &self.unix_path {
            Some(path) => (path.display().to_string(), 0),
            None => (self.base_url.clone(), self.credential_fingerprint()),
        };
        let mut pool = client_pool().lock().unwrap_or_else(|e| e.into_inner());
        pool.retain(|_, entry| entry.used.elapsed() < POOL_IDLE);
        if let Some(entry) = pool.get_mut(&key) {
            if entry.fingerprint == fingerprint {
                entry.used = std::time::Instant::now();
                return Ok(entry.client.clone());
            }
        }
        let client = match &self.unix_path {
            Some(path) => DaemonClient::new_unix(path)?,
            None => DaemonClient::with_login(
                &self.base_url,
                self.bearer_token(),
                self.login.as_ref(),
                self.allow_plaintext,
            )?,
        };
        pool.insert(
            key,
            PooledClient {
                fingerprint,
                client: client.clone(),
                used: std::time::Instant::now(),
            },
        );
        Ok(client)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A unix endpoint's `base_url` is a request-line placeholder, so a URL
    /// meant for a browser comes from the daemon's published address. Without
    /// this, a plugin's relative href resolved to `http://localhost/...`.
    #[test]
    #[serial_test::serial]
    fn a_unix_endpoint_browses_the_daemons_published_address() {
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let dir = crate::session::get_app_dir().expect("isolated app dir");
        std::fs::write(dir.join("serve.url"), "http://127.0.0.1:8123/?token=abc\n").unwrap();
        let unix = DaemonEndpoint::local_unix(PathBuf::from("/tmp/aoe.sock"));
        assert_eq!(unix.browser_base_url(), "http://127.0.0.1:8123");

        // An http endpoint already knows where it is.
        let http = DaemonEndpoint::new("http://10.0.0.2:8080".into(), None, Source::LocalDaemon);
        assert_eq!(http.browser_base_url(), "http://10.0.0.2:8080");
    }

    #[test]
    #[serial_test::serial]
    fn a_unix_endpoint_keeps_its_placeholder_when_no_address_is_published() {
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let unix = DaemonEndpoint::local_unix(PathBuf::from("/tmp/aoe.sock"));
        assert_eq!(unix.browser_base_url(), "http://localhost");
    }
}
