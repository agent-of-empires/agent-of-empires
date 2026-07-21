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
    /// Bearer token. `None` when the daemon was started with
    /// `--no-auth`, or when `AOE_DAEMON_URL` is set without
    /// `AOE_DAEMON_TOKEN`.
    pub token: Option<String>,
    pub source: Source,
}

impl DaemonEndpoint {
    /// Same base URL, scheme rewritten to `ws://` / `wss://` so a
    /// caller can hand it to `tokio_tungstenite::connect_async`.
    pub fn ws_base_url(&self) -> String {
        http_to_ws(&self.base_url)
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
    Some(DaemonEndpoint {
        base_url: trim_query(url).trim_end_matches('/').to_string(),
        token,
        source: Source::Env,
    })
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
    Ok(DaemonEndpoint {
        base_url,
        token,
        source: Source::LocalDaemon,
    })
}

fn preferred_daemon_url(urls: &[ServeUrl]) -> Option<&ServeUrl> {
    urls.iter()
        .find(|u| is_loopback(&u.url))
        .or_else(|| urls.first())
}

fn is_loopback(url: &str) -> bool {
    let host = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split(['/', '?'])
        .next()
        .unwrap_or("");
    host.starts_with("127.0.0.1") || host.starts_with("localhost") || host.starts_with("[::1]")
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
    fn is_loopback_matches_localhost_variants() {
        assert!(is_loopback("http://127.0.0.1:8080"));
        assert!(is_loopback("http://localhost:8081/"));
        assert!(is_loopback("http://[::1]:8080"));
        assert!(!is_loopback("https://example.com"));
        assert!(!is_loopback("http://192.168.1.50:8080"));
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
        assert_eq!(endpoint.token.as_deref(), Some("real-token"));
        assert_eq!(endpoint.source, Source::Env);
        unsafe {
            std::env::remove_var("AOE_DAEMON_URL");
            std::env::remove_var("AOE_DAEMON_TOKEN");
        }
    }
}
