//! Locate a structured view daemon (`aoe serve`) the client should talk to.
//!
//! `AOE_DAEMON_URL` (+ `AOE_DAEMON_TOKEN`) wins, because env keeps the token
//! out of `ps`. Otherwise `<app_dir>/serve.url` plus a live `serve.pid`,
//! preferring the loopback alternate so a same-box client does not round-trip
//! through a tunnel. [`super::daemon_manager::require_daemon`] wraps this with
//! a health check and a friendlier no-daemon error.

use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use thiserror::Error;

use crate::cli::serve::{daemon_pid, read_serve_urls, ServeUrl};
use crate::daemon::{DaemonClient, DaemonClientError};

/// `base_url` carries no query string so it is safe to log; the token travels
/// separately, as a bearer header in [`super::http`] and a `?token=` query in
/// [`super::ws`].
#[derive(Debug, Clone)]
pub struct DaemonEndpoint {
    /// Bare base URL (`http://127.0.0.1:8080`), no trailing slash or query.
    pub base_url: String,
    /// Discovery-time token, `None` under `--no-auth`. Loopback clients
    /// re-read `serve.token` per request because the daemon rotates it while
    /// a TUI stays open.
    token: Arc<RwLock<Option<String>>>,
    local_token_path: Option<PathBuf>,
    /// `<app_dir>/serve.passphrase`, set only for a loopback local daemon.
    /// Read fresh on every [`resolved_passphrase`](Self::resolved_passphrase)
    /// call rather than cached: unlike the bearer token there is no rotation
    /// path to race, so a plain re-read keeps this simple.
    local_passphrase_path: Option<PathBuf>,
    pub source: Source,
}

impl DaemonEndpoint {
    pub(crate) fn new(base_url: String, token: Option<String>, source: Source) -> Self {
        Self {
            base_url,
            token: Arc::new(RwLock::new(token)),
            local_token_path: None,
            local_passphrase_path: None,
            source,
        }
    }

    pub(crate) fn with_local_token_path(mut self, token_path: PathBuf) -> Self {
        self.local_token_path = Some(token_path);
        self
    }

    pub(crate) fn with_local_passphrase_path(mut self, passphrase_path: PathBuf) -> Self {
        self.local_passphrase_path = Some(passphrase_path);
        self
    }

    /// Same base URL with a `ws://` / `wss://` scheme.
    pub fn ws_base_url(&self) -> String {
        http_to_ws(&self.base_url)
    }

    /// Session-list client carrying the credential as resolved now.
    pub fn daemon_client(&self) -> Result<DaemonClient, DaemonClientError> {
        let token = self.resolved_token();
        DaemonClient::new(&self.base_url, token.as_deref())
    }

    /// The credential to send now, not the discovery-time snapshot. Only a
    /// loopback local-daemon endpoint may re-read the app directory: an env
    /// override or a legacy public endpoint must never be handed some other
    /// local daemon's token.
    pub(crate) fn resolved_token(&self) -> Option<String> {
        match self.local_token_path.as_deref() {
            Some(path) => self.resolved_token_from_path(path),
            None => self.cached_token(),
        }
    }

    fn resolved_token_from_path(&self, token_path: &Path) -> Option<String> {
        let cached = self.cached_token();
        cached.as_ref()?;
        if self.source != Source::LocalDaemon || !is_loopback(&self.base_url) {
            return cached;
        }
        read_valid_token(token_path).map_or(cached, |current| {
            *self.token.write().unwrap_or_else(|e| e.into_inner()) = Some(current.clone());
            Some(current)
        })
    }

    pub(crate) fn has_token(&self) -> bool {
        self.cached_token().is_some()
    }

    pub(crate) fn cached_token(&self) -> Option<String> {
        self.token.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Passphrase to present to `/api/login` when no bearer token resolves
    /// (a `--auth=passphrase` daemon never mints one). `AOE_DAEMON_PASSPHRASE`
    /// always wins when set, so an explicit override works against a remote
    /// `AOE_DAEMON_URL` target too; otherwise only a loopback local daemon
    /// consults `serve.passphrase` (the file the daemon itself writes for its
    /// own `--restart` recall, see `cli::serve::recall_serve_passphrase`).
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
    #[error(
        "no local structured view daemon is running; start one with `aoe serve` or set AOE_DAEMON_URL"
    )]
    NoLocalDaemon,
    #[error("serve.url is empty or malformed; restart `aoe serve` to refresh it")]
    Malformed,
}

/// Locate a daemon endpoint via env override or local serve files.
pub fn discover() -> Result<DaemonEndpoint, DiscoveryError> {
    if let Some(endpoint) = discover_env() {
        return Ok(endpoint);
    }
    discover_local()
}

/// `None` when `AOE_DAEMON_URL` is unset or empty.
pub fn discover_env() -> Option<DaemonEndpoint> {
    let url = env::var("AOE_DAEMON_URL").ok()?;
    let url = url.trim();
    if url.is_empty() {
        return None;
    }
    let token = env::var("AOE_DAEMON_TOKEN")
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty());
    Some(DaemonEndpoint::new(
        trim_query(url).trim_end_matches('/').to_string(),
        token,
        Source::Env,
    ))
}

/// `Err(NoLocalDaemon)` when no live local daemon is found.
pub fn discover_local() -> Result<DaemonEndpoint, DiscoveryError> {
    if daemon_pid().is_none() {
        return Err(DiscoveryError::NoLocalDaemon);
    }
    let urls = read_serve_urls();
    if urls.is_empty() {
        return Err(DiscoveryError::NoLocalDaemon);
    }
    let pick = preferred_daemon_url(&urls).ok_or(DiscoveryError::Malformed)?;
    let token = extract_token(&pick.url).map(str::to_string);
    let base_url = trim_query(&pick.url).trim_end_matches('/').to_string();
    if base_url.is_empty() {
        return Err(DiscoveryError::Malformed);
    }
    let endpoint = DaemonEndpoint::new(base_url, token, Source::LocalDaemon);
    let app_dir = is_loopback(&endpoint.base_url)
        .then(crate::session::get_app_dir)
        .and_then(Result::ok);
    Ok(match app_dir {
        Some(dir) => endpoint
            .with_local_token_path(dir.join("serve.token"))
            .with_local_passphrase_path(dir.join("serve.passphrase")),
        None => endpoint,
    })
}

fn preferred_daemon_url(urls: &[ServeUrl]) -> Option<&ServeUrl> {
    urls.iter()
        .find(|u| is_loopback(&u.url))
        .or_else(|| urls.first())
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

fn extract_token(url: &str) -> Option<&str> {
    let query = url.split_once('?').map(|(_, q)| q)?;
    for pair in query.split('&') {
        if let Some(rest) = pair.strip_prefix("token=") {
            if rest.is_empty() {
                return None;
            }
            return Some(rest);
        }
    }
    None
}

fn read_valid_token(path: &Path) -> Option<String> {
    let token = std::fs::read_to_string(path).ok()?;
    let token = token.trim();
    let valid_len = token.len() == 64 || token.len() == 32;
    let valid_chars = token
        .chars()
        .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
    (valid_len && valid_chars).then(|| token.to_string())
}

fn http_to_ws(http_url: &str) -> String {
    if let Some(rest) = http_url.strip_prefix("https://") {
        return format!("wss://{rest}");
    }
    if let Some(rest) = http_url.strip_prefix("http://") {
        return format!("ws://{rest}");
    }
    http_url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(token: Option<&str>, source: Source) -> DaemonEndpoint {
        DaemonEndpoint::new(
            "http://127.0.0.1:8080".into(),
            token.map(str::to_string),
            source,
        )
    }

    #[test]
    fn url_helpers_and_loopback_detection() {
        for (url, token) in [
            ("http://localhost:8080/?token=abc123", Some("abc123")),
            ("http://localhost:8080/?foo=bar&token=zzz", Some("zzz")),
            ("http://localhost:8080/", None),
            ("http://localhost:8080/?foo=bar", None),
            ("http://localhost:8080/?token=", None),
        ] {
            assert_eq!(extract_token(url), token, "{url}");
        }
        assert_eq!(
            trim_query("http://localhost:8080/?token=abc"),
            "http://localhost:8080/"
        );
        assert_eq!(trim_query("http://host/"), "http://host/");
        assert_eq!(http_to_ws("http://127.0.0.1:8080"), "ws://127.0.0.1:8080");
        assert_eq!(http_to_ws("https://remote.test"), "wss://remote.test");
        assert_eq!(http_to_ws("ws://already"), "ws://already");

        {
            assert!(is_loopback("http://127.0.0.1:8080"));
            assert!(is_loopback("http://localhost:8081/"));
            assert!(is_loopback("http://[::1]:8080"));
            assert!(is_loopback("http://127.2.3.4:8080"));
            assert!(!is_loopback("https://example.com"));
            assert!(!is_loopback("http://192.168.1.50:8080"));
            assert!(!is_loopback("https://localhost.attacker.example"));
            assert!(!is_loopback("http://127.0.0.1.evil.example"));

            // The loopback alternate wins, but a lone public URL is still selected.
            let public = ServeUrl {
                label: None,
                url: "https://aoe.example.test/?token=secret".into(),
            };
            let loopback = ServeUrl {
                label: Some("localhost".into()),
                url: "http://127.0.0.1:8080/?token=secret".into(),
            };
            for (urls, want) in [
                (vec![public.clone(), loopback.clone()], &loopback),
                (vec![public.clone()], &public),
            ] {
                let selected = preferred_daemon_url(&urls).expect("a daemon URL is selected");
                assert_eq!(selected.url, want.url);
            }
        }
    }

    /// A loopback local daemon adopts a rotated token, keeps the last valid one
    /// across a torn or non-hex write, and caches it for the next read.
    #[test]
    fn only_a_loopback_local_daemon_reads_the_rotated_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serve.token");
        let (old, new) = ("a".repeat(64), "b".repeat(64));
        let local = endpoint(Some(&old), Source::LocalDaemon);

        std::fs::write(&path, &new).unwrap();
        assert_eq!(local.resolved_token_from_path(&path), Some(new.clone()));
        for invalid in ["partial".to_string(), "A".repeat(64), "g".repeat(64)] {
            std::fs::write(&path, invalid).unwrap();
            assert_eq!(local.resolved_token_from_path(&path), Some(new.clone()));
        }

        // Only a loopback local daemon may be handed the app directory's token: a
        // legacy public endpoint, an env override, and a `--no-auth` endpoint all
        // keep what discovery gave them.
        {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("serve.token");
            std::fs::write(&path, "b".repeat(64)).unwrap();
            let captured = "a".repeat(64);

            let public = DaemonEndpoint::new(
                "https://old-tunnel.example.com".into(),
                Some(captured.clone()),
                Source::LocalDaemon,
            );
            assert_eq!(
                public.resolved_token_from_path(&path),
                Some(captured.clone())
            );
            assert_eq!(
                endpoint(Some(&captured), Source::Env).resolved_token_from_path(&path),
                Some(captured)
            );
            assert_eq!(
                endpoint(None, Source::LocalDaemon).resolved_token_from_path(&path),
                None
            );
        }
    }

    #[test]
    #[serial_test::serial]
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
        assert_eq!(endpoint.cached_token().as_deref(), Some("real-token"));
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
