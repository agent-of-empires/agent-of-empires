//! Locate a structured view daemon (`aoe serve`) the client should talk to.
//!
//! Resolution order:
//!
//! 1. `AOE_DAEMON_URL` env var (paired with `AOE_DAEMON_TOKEN`). Env
//!    is preferred over CLI flags so the token never leaks via `ps`.
//! 2. Local daemon: `<app_dir>/serve.url` + a live `<app_dir>/serve.pid`.
//!    The loopback alternate is preferred over the primary line so
//!    clients on the same box don't round-trip through a tunnel.
//!
//! Returns `Err(NoLocalDaemon)` when neither resolves.
//! [`super::daemon_manager::require_daemon`] wraps this with a
//! health-check on the env override and a friendlier no-daemon error
//! variant whose message tells the user how to start one.

use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use thiserror::Error;

use crate::cli::serve::{daemon_pid, read_serve_urls, ServeUrl};

/// A located daemon endpoint. `base_url` carries no query string so it
/// is safe to log; the auth token (if any) travels separately and is
/// applied as an `Authorization: Bearer` header by [`super::http`] and
/// as a `?token=` query for the WebSocket handshake by [`super::ws`].
#[derive(Debug, Clone)]
pub struct DaemonEndpoint {
    /// Bare base URL (`http://127.0.0.1:8080`). No trailing slash, no
    /// query string.
    pub base_url: String,
    /// Bearer token captured during discovery. Loopback clients re-read the
    /// owner-only `serve.token` before each request because remote daemons
    /// rotate it while a TUI can remain open.
    ///
    /// `None` when the daemon was started with `--no-auth`, or when
    /// `AOE_DAEMON_URL` is set without `AOE_DAEMON_TOKEN`.
    token: Arc<RwLock<Option<String>>>,
    local_token_path: Option<PathBuf>,
    pub source: Source,
}

impl DaemonEndpoint {
    pub(crate) fn new(base_url: String, token: Option<String>, source: Source) -> Self {
        Self {
            base_url,
            token: Arc::new(RwLock::new(token)),
            local_token_path: None,
            source,
        }
    }

    pub(crate) fn with_local_token_path(mut self, token_path: PathBuf) -> Self {
        self.local_token_path = Some(token_path);
        self
    }

    /// Same base URL, scheme rewritten to `ws://` / `wss://` so a
    /// caller can hand it to `tokio_tungstenite::connect_async`.
    pub fn ws_base_url(&self) -> String {
        http_to_ws(&self.base_url)
    }

    /// Resolve the credential to send now rather than relying forever on the
    /// discovery-time snapshot. Only loopback local-daemon discovery may
    /// consult the app directory; explicit env and legacy public endpoints
    /// must never receive a token read for some other local daemon.
    pub(crate) fn resolved_token(&self) -> Option<String> {
        let cached = self.cached_token();
        if self.source != Source::LocalDaemon || !is_loopback(&self.base_url) {
            return cached;
        }
        let Some(token_path) = self.local_token_path.as_deref() else {
            return cached;
        };
        self.resolved_token_from_path(token_path)
    }

    fn resolved_token_from_path(&self, token_path: &Path) -> Option<String> {
        let cached = self.cached_token();
        cached.as_ref()?;
        if self.source != Source::LocalDaemon || !is_loopback(&self.base_url) {
            return cached;
        }
        let Some(current) = read_valid_token(token_path) else {
            return cached;
        };
        *self.token.write().unwrap_or_else(|e| e.into_inner()) = Some(current.clone());
        Some(current)
    }

    pub(crate) fn has_token(&self) -> bool {
        self.cached_token().is_some()
    }

    pub(crate) fn cached_token(&self) -> Option<String> {
        self.token.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `AOE_DAEMON_URL` env var.
    Env,
    /// Read from `<app_dir>/serve.url`.
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

/// `AOE_DAEMON_URL` (+ optional `AOE_DAEMON_TOKEN`). Returns `None`
/// when the env var is unset or empty.
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

/// Local serve daemon discovery. Returns `Err(NoLocalDaemon)` when no
/// live daemon is found.
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
    let endpoint = if is_loopback(&endpoint.base_url) {
        match crate::session::get_app_dir() {
            Ok(app_dir) => endpoint.with_local_token_path(app_dir.join("serve.token")),
            Err(_) => endpoint,
        }
    } else {
        endpoint
    };
    Ok(endpoint)
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
    let Some(host) = parsed.host_str() else {
        return false;
    };
    let ip_host = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    host.eq_ignore_ascii_case("localhost")
        || ip_host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
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
    fn extract_token_simple() {
        assert_eq!(
            extract_token("http://localhost:8080/?token=abc123"),
            Some("abc123")
        );
    }

    #[test]
    fn extract_token_none_when_missing() {
        assert_eq!(extract_token("http://localhost:8080/"), None);
        assert_eq!(extract_token("http://localhost:8080/?foo=bar"), None);
    }

    #[test]
    fn extract_token_none_when_empty() {
        assert_eq!(extract_token("http://localhost:8080/?token="), None);
    }

    #[test]
    fn extract_token_multi_param() {
        assert_eq!(
            extract_token("http://localhost:8080/?foo=bar&token=zzz"),
            Some("zzz")
        );
    }

    #[test]
    fn trim_query_strips_query_string() {
        assert_eq!(
            trim_query("http://localhost:8080/?token=abc"),
            "http://localhost:8080/"
        );
        assert_eq!(
            trim_query("http://localhost:8080/"),
            "http://localhost:8080/"
        );
    }

    #[test]
    fn http_to_ws_handles_both_schemes() {
        assert_eq!(http_to_ws("http://127.0.0.1:8080"), "ws://127.0.0.1:8080");
        assert_eq!(
            http_to_ws("https://remote.example.com"),
            "wss://remote.example.com"
        );
        assert_eq!(http_to_ws("ws://already"), "ws://already");
    }

    #[test]
    fn local_endpoint_resolves_rotated_token_at_use_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serve.token");
        let old = "a".repeat(64);
        let new = "b".repeat(64);
        std::fs::write(&path, &new).unwrap();

        let endpoint = endpoint(Some(&old), Source::LocalDaemon);
        assert_eq!(endpoint.resolved_token_from_path(&path), Some(new));
    }

    #[test]
    fn local_endpoint_falls_back_during_invalid_token_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serve.token");
        let old = "a".repeat(64);
        std::fs::write(&path, "partial").unwrap();

        let endpoint = endpoint(Some(&old), Source::LocalDaemon);
        assert_eq!(endpoint.resolved_token_from_path(&path), Some(old));
    }

    #[test]
    fn local_endpoint_rejects_non_lowercase_hex_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serve.token");
        let old = "a".repeat(64);
        let endpoint = endpoint(Some(&old), Source::LocalDaemon);

        for invalid in ["A".repeat(64), "g".repeat(64)] {
            std::fs::write(&path, invalid).unwrap();
            assert_eq!(endpoint.resolved_token_from_path(&path), Some(old.clone()));
        }
    }

    #[test]
    fn local_endpoint_caches_last_valid_rotated_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serve.token");
        let old = "a".repeat(64);
        let new = "b".repeat(64);
        let endpoint = endpoint(Some(&old), Source::LocalDaemon);

        std::fs::write(&path, &new).unwrap();
        assert_eq!(endpoint.resolved_token_from_path(&path), Some(new.clone()));
        std::fs::write(&path, "partial").unwrap();
        assert_eq!(endpoint.resolved_token_from_path(&path), Some(new));
    }

    #[test]
    fn legacy_public_endpoint_never_receives_local_daemon_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serve.token");
        let captured = "a".repeat(64);
        std::fs::write(&path, "b".repeat(64)).unwrap();
        let endpoint = DaemonEndpoint::new(
            "https://old-tunnel.example.com".into(),
            Some(captured.clone()),
            Source::LocalDaemon,
        );

        assert_eq!(endpoint.resolved_token_from_path(&path), Some(captured));
    }

    #[test]
    fn env_endpoint_never_reads_local_daemon_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serve.token");
        let explicit = "a".repeat(64);
        std::fs::write(&path, "b".repeat(64)).unwrap();

        let endpoint = endpoint(Some(&explicit), Source::Env);
        assert_eq!(endpoint.resolved_token_from_path(&path), Some(explicit));
    }

    #[test]
    fn no_auth_endpoint_ignores_lingering_token_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serve.token");
        std::fs::write(&path, "b".repeat(64)).unwrap();

        let endpoint = endpoint(None, Source::LocalDaemon);
        assert_eq!(endpoint.resolved_token_from_path(&path), None);
    }

    #[test]
    fn is_loopback_matches_localhost_variants() {
        assert!(is_loopback("http://127.0.0.1:8080"));
        assert!(is_loopback("http://localhost:8081/"));
        assert!(is_loopback("http://[::1]:8080"));
        assert!(is_loopback("http://127.2.3.4:8080"));
        assert!(!is_loopback("https://example.com"));
        assert!(!is_loopback("http://192.168.1.50:8080"));
        assert!(!is_loopback("https://localhost.attacker.example"));
        assert!(!is_loopback("http://127.0.0.1.evil.example"));
    }

    #[test]
    fn preferred_daemon_url_uses_loopback_alternate() {
        let urls = vec![
            ServeUrl {
                label: None,
                url: "https://aoe.example.test/?token=secret".into(),
            },
            ServeUrl {
                label: Some("localhost".into()),
                url: "http://127.0.0.1:8080/?token=secret".into(),
            },
        ];

        let selected = preferred_daemon_url(&urls).expect("a daemon URL should be selected");
        assert_eq!(selected.url, urls[1].url);
    }

    #[test]
    fn preferred_daemon_url_keeps_single_url_backward_compatibility() {
        let urls = vec![ServeUrl {
            label: None,
            url: "https://aoe.example.test/?token=secret".into(),
        }];

        let selected = preferred_daemon_url(&urls).expect("a daemon URL should be selected");
        assert_eq!(selected.url, urls[0].url);
    }

    // Env-touching tests must run serially; cargo test runs in
    // parallel by default and set_var races with the unset cases.
    #[test]
    #[serial_test::serial]
    fn discover_env_returns_none_when_unset() {
        unsafe {
            std::env::remove_var("AOE_DAEMON_URL");
            std::env::remove_var("AOE_DAEMON_TOKEN");
        }
        assert!(discover_env().is_none());
    }

    #[test]
    #[serial_test::serial]
    fn discover_env_parses_url_and_token() {
        unsafe {
            std::env::set_var(
                "AOE_DAEMON_URL",
                "https://remote.example.com:9000/?token=zzz",
            );
            std::env::set_var("AOE_DAEMON_TOKEN", "real-token");
        }
        let endpoint = discover_env().expect("env override should resolve");
        // ENV override strips the query string defensively even though
        // tokens should travel via AOE_DAEMON_TOKEN, not the URL.
        assert_eq!(endpoint.base_url, "https://remote.example.com:9000");
        assert_eq!(endpoint.cached_token().as_deref(), Some("real-token"));
        assert_eq!(endpoint.source, Source::Env);
        unsafe {
            std::env::remove_var("AOE_DAEMON_URL");
            std::env::remove_var("AOE_DAEMON_TOKEN");
        }
    }
}
