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
    /// `<app_dir>/serve.passphrase`. Only the tests build this: the local
    /// daemon is reached over its owner-verified Unix socket, so no
    /// production endpoint resolves a passphrase from a local file (see
    /// [`resolved_passphrase`](Self::resolved_passphrase)).
    local_passphrase_path: Option<PathBuf>,
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
            local_passphrase_path: None,
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
            local_passphrase_path: None,
            source,
            unix_path: None,
        }
    }

    /// Test seam: no production endpoint carries a local passphrase file (see
    /// the field), so this builder is only reachable from tests.
    #[cfg(test)]
    pub(crate) fn with_local_passphrase_path(mut self, passphrase_path: PathBuf) -> Self {
        self.local_passphrase_path = Some(passphrase_path);
        self
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

    /// Passphrase to present to `/api/login` when no bearer token resolves
    /// (a `--auth=passphrase` daemon never mints one). `AOE_DAEMON_PASSPHRASE`
    /// always wins when set, so an explicit override works against a remote
    /// `AOE_DAEMON_URL` target too; otherwise only a loopback local daemon
    /// consults `serve.passphrase` (the file the daemon itself writes for its
    /// own `--restart` recall, see `cli::serve::recall_serve_passphrase`). No
    /// production endpoint takes that branch: the local daemon is reached
    /// over its owner-verified Unix socket, so only the env override
    /// applies.
    pub(crate) fn resolved_passphrase(&self) -> Option<String> {
        if let Some(env_value) = env_passphrase_override() {
            return transport_is_safe_for_passphrase(&self.base_url).then_some(env_value);
        }
        if self.source != Source::LocalDaemon || !is_loopback(&self.base_url) {
            return None;
        }
        let path = self.local_passphrase_path.as_deref()?;
        let raw = std::fs::read_to_string(path).ok()?;
        let trimmed = raw.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    }

    /// Directory used to cache the CLI's own passphrase-login session
    /// (device-binding secret + `aoe_session` cookie), so a repeated CLI
    /// invocation reuses one long-lived login instead of minting a fresh
    /// device session every process. A loopback local daemon caches
    /// directly under `<app_dir>` (the file `resolved_passphrase` already
    /// trusts). A remote endpoint with a usable passphrase caches under a
    /// per-URL subdirectory instead: without this, every `aoe acp <verb>`
    /// call against the same remote daemon logged in again, and enough of
    /// them evict real browser sessions under the daemon's session cap.
    /// `None` when no passphrase is resolvable at all — nothing to cache.
    pub(crate) fn session_cache_dir(&self) -> Option<PathBuf> {
        if self.source == Source::LocalDaemon && is_loopback(&self.base_url) {
            return self
                .local_passphrase_path
                .as_deref()
                .and_then(Path::parent)
                .map(PathBuf::from);
        }
        self.resolved_passphrase()?;
        let app_dir = crate::session::get_app_dir().ok()?;
        Some(
            app_dir
                .join("remote-passphrase-sessions")
                .join(remote_cache_key(&self.base_url)),
        )
    }
}

/// Filesystem-safe cache key for a remote endpoint's base URL. A character
/// substitution would let two distinct hostnames collide (`foo-bar.com` and
/// `foo_bar.com` both sanitize to `foo_bar_com`), sharing one endpoint's
/// cached session with another's, so this hashes the whole URL instead.
fn remote_cache_key(base_url: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(base_url.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn env_passphrase_override() -> Option<String> {
    let value = env::var("AOE_DAEMON_PASSPHRASE").ok()?;
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// A passphrase may only travel to an endpoint that can't be read in
/// transit: `https://`, or loopback (never leaves the host). Anything
/// else — a plaintext `http://` URL to a non-loopback host — would hand
/// the shared secret to an on-path attacker.
fn transport_is_safe_for_passphrase(base_url: &str) -> bool {
    base_url.starts_with("https://") || is_loopback(base_url)
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
        // A `?token=` in the URL is stripped: the credential belongs in
        // AOE_DAEMON_TOKEN, and a query would make the base URL unusable as
        // a request target.
        DaemonEndpoint::new(
            trim_query(url).trim_end_matches('/').to_string(),
            token,
            Source::Env,
        )
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

fn is_loopback(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    crate::daemon::is_loopback_url(&parsed)
}

fn trim_query(url: &str) -> &str {
    url.split_once('?').map(|(u, _)| u).unwrap_or(url)
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

    #[test]
    #[serial]
    fn discover_env_parses_url_and_token() {
        {
            let _env = crate::session::test_support::EnvGuard::unset(&[
                "AOE_DAEMON_URL",
                "AOE_DAEMON_TOKEN",
            ]);
            assert!(discover_env().is_none());
        }
        let _env = crate::session::test_support::EnvGuard::set(&[
            (
                "AOE_DAEMON_URL",
                "https://remote.example.com:9000/?token=zzz",
            ),
            ("AOE_DAEMON_TOKEN", "real-token"),
        ]);
        let endpoint = discover_env().expect("env override should resolve");
        // Stripped defensively: the token belongs in AOE_DAEMON_TOKEN.
        assert_eq!(endpoint.base_url, "https://remote.example.com:9000");
        assert_eq!(endpoint.bearer_token(), Some("real-token"));
        assert_eq!(endpoint.source, Source::Env);
    }

    fn passphrase_endpoint(source: Source, passphrase_path: Option<PathBuf>) -> DaemonEndpoint {
        let mut endpoint = DaemonEndpoint::new("http://127.0.0.1:8080".into(), None, source);
        if let Some(path) = passphrase_path {
            endpoint = endpoint.with_local_passphrase_path(path);
        }
        endpoint
    }

    #[test]
    #[serial_test::serial]
    fn resolved_passphrase_reads_local_file_for_loopback_daemon() {
        let _env = crate::session::test_support::EnvGuard::unset(&["AOE_DAEMON_PASSPHRASE"]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serve.passphrase");
        std::fs::write(&path, "correct horse battery staple\n").unwrap();

        let endpoint = passphrase_endpoint(Source::LocalDaemon, Some(path));
        assert_eq!(
            endpoint.resolved_passphrase().as_deref(),
            Some("correct horse battery staple")
        );
    }

    #[test]
    #[serial_test::serial]
    fn resolved_passphrase_env_override_wins_over_local_file() {
        let _env =
            crate::session::test_support::EnvGuard::set(&[("AOE_DAEMON_PASSPHRASE", "from-env")]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serve.passphrase");
        std::fs::write(&path, "from-file").unwrap();

        let endpoint = passphrase_endpoint(Source::LocalDaemon, Some(path));
        assert_eq!(endpoint.resolved_passphrase().as_deref(), Some("from-env"));
    }

    #[test]
    #[serial_test::serial]
    fn resolved_passphrase_env_override_works_for_remote_endpoint() {
        let _env =
            crate::session::test_support::EnvGuard::set(&[("AOE_DAEMON_PASSPHRASE", "from-env")]);
        let endpoint = passphrase_endpoint(Source::Env, None);
        assert_eq!(endpoint.resolved_passphrase().as_deref(), Some("from-env"));
    }

    #[test]
    #[serial_test::serial]
    fn resolved_passphrase_env_override_rejects_plaintext_remote_endpoint() {
        // A plaintext http:// URL to a non-loopback host would send the
        // shared passphrase to /api/login in the clear; an on-path
        // attacker could read it, so the override must not apply.
        let _env =
            crate::session::test_support::EnvGuard::set(&[("AOE_DAEMON_PASSPHRASE", "from-env")]);
        let endpoint =
            DaemonEndpoint::new("http://remote.example.com:8080".into(), None, Source::Env);
        assert_eq!(endpoint.resolved_passphrase(), None);
    }

    #[test]
    #[serial_test::serial]
    fn resolved_passphrase_env_override_allows_https_remote_endpoint() {
        let _env =
            crate::session::test_support::EnvGuard::set(&[("AOE_DAEMON_PASSPHRASE", "from-env")]);
        let endpoint = DaemonEndpoint::new("https://remote.example.com".into(), None, Source::Env);
        assert_eq!(endpoint.resolved_passphrase().as_deref(), Some("from-env"));
    }

    #[test]
    #[serial_test::serial]
    fn resolved_passphrase_none_without_env_or_local_file() {
        let _env = crate::session::test_support::EnvGuard::unset(&["AOE_DAEMON_PASSPHRASE"]);
        let endpoint = passphrase_endpoint(Source::LocalDaemon, None);
        assert_eq!(endpoint.resolved_passphrase(), None);

        let remote = passphrase_endpoint(Source::Env, None);
        assert_eq!(remote.resolved_passphrase(), None);
    }

    #[test]
    #[serial_test::serial]
    fn resolved_passphrase_ignores_empty_local_file() {
        let _env = crate::session::test_support::EnvGuard::unset(&["AOE_DAEMON_PASSPHRASE"]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serve.passphrase");
        std::fs::write(&path, "  \n").unwrap();

        let endpoint = passphrase_endpoint(Source::LocalDaemon, Some(path));
        assert_eq!(endpoint.resolved_passphrase(), None);
    }

    #[test]
    fn session_cache_dir_is_the_passphrase_files_parent_for_loopback_local_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serve.passphrase");
        let endpoint = passphrase_endpoint(Source::LocalDaemon, Some(path));
        assert_eq!(endpoint.session_cache_dir(), Some(dir.path().to_path_buf()));
    }

    #[test]
    #[serial_test::serial]
    fn session_cache_dir_none_for_remote_endpoint_without_a_passphrase() {
        let _env = crate::session::test_support::EnvGuard::unset(&["AOE_DAEMON_PASSPHRASE"]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serve.passphrase");
        let endpoint = passphrase_endpoint(Source::Env, Some(path));
        assert_eq!(endpoint.session_cache_dir(), None);
    }

    #[test]
    #[serial_test::serial]
    fn session_cache_dir_is_keyed_by_url_for_remote_endpoint_with_a_passphrase() {
        // Without a cache here, every `aoe acp <verb>` call against the same
        // remote daemon logs in again, and enough of them evict real
        // browser sessions under the daemon's session cap.
        let home = tempfile::tempdir().unwrap();
        let _env = crate::session::test_support::EnvGuard::set(&[
            ("AOE_DAEMON_PASSPHRASE", "hunter2"),
            ("HOME", home.path().to_str().unwrap()),
            (
                "XDG_CONFIG_HOME",
                home.path().join(".config").to_str().unwrap(),
            ),
        ]);
        let endpoint = DaemonEndpoint::new("https://remote.example.com".into(), None, Source::Env);
        let dir = endpoint
            .session_cache_dir()
            .expect("a remote endpoint with a usable passphrase should cache");
        assert!(dir.starts_with(crate::session::get_app_dir().unwrap()));
        assert_eq!(
            dir.file_name().unwrap().to_str().unwrap(),
            remote_cache_key("https://remote.example.com")
        );

        // A different URL must not collide with the first one's cache.
        let other = DaemonEndpoint::new("https://other.example.com".into(), None, Source::Env);
        assert_ne!(other.session_cache_dir(), endpoint.session_cache_dir());
    }

    #[test]
    #[serial_test::serial]
    fn session_cache_dir_none_for_non_loopback_local_daemon() {
        let _env = crate::session::test_support::EnvGuard::unset(&["AOE_DAEMON_PASSPHRASE"]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serve.passphrase");
        let endpoint = DaemonEndpoint::new(
            "https://old-tunnel.example.com".into(),
            None,
            Source::LocalDaemon,
        )
        .with_local_passphrase_path(path);
        // Not loopback, so it takes the remote-cache branch; no env
        // override and the local file lookup requires loopback, so no
        // passphrase resolves and there is nothing to cache.
        assert_eq!(endpoint.session_cache_dir(), None);
    }
}
