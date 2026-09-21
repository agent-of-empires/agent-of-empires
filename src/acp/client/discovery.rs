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
        Some(dir) => endpoint.with_local_token_path(dir.join("serve.token")),
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
    fn url_parsing_helpers() {
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
    }

    /// A loopback local daemon adopts a rotated token, keeps the last valid one
    /// across a torn or non-hex write, and caches it for the next read.
    #[test]
    fn loopback_endpoint_tracks_rotated_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serve.token");
        let (old, new) = ("a".repeat(64), "b".repeat(64));
        let endpoint = endpoint(Some(&old), Source::LocalDaemon);

        std::fs::write(&path, &new).unwrap();
        assert_eq!(endpoint.resolved_token_from_path(&path), Some(new.clone()));
        for invalid in ["partial".to_string(), "A".repeat(64), "g".repeat(64)] {
            std::fs::write(&path, invalid).unwrap();
            assert_eq!(endpoint.resolved_token_from_path(&path), Some(new.clone()));
        }
    }

    /// Only a loopback local daemon may be handed the app directory's token: a
    /// legacy public endpoint, an env override, and a `--no-auth` endpoint all
    /// keep what discovery gave them.
    #[test]
    fn non_loopback_endpoints_never_read_the_local_token_file() {
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

    /// The loopback alternate wins, but a lone public URL is still selected.
    #[test]
    fn preferred_daemon_url_prefers_loopback() {
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

    #[test]
    #[serial_test::serial]
    fn discover_env_returns_none_when_unset() {
        let _env =
            crate::session::test_support::EnvGuard::unset(&["AOE_DAEMON_URL", "AOE_DAEMON_TOKEN"]);
        assert!(discover_env().is_none());
    }

    #[test]
    #[serial_test::serial]
    fn discover_env_parses_url_and_token() {
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
}
