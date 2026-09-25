//! Cookie-based fallback auth for `--auth=passphrase` daemons.
//!
//! `HttpClient` sends `Authorization: Bearer <token>` when a token resolves.
//! A passphrase daemon never mints one, so when only a passphrase resolves
//! (`DaemonEndpoint::resolved_passphrase`), the client performs the same
//! `POST /api/login` handshake the web UI does (see `server::login`) and
//! sends the resulting `aoe_session` cookie instead. The device-binding
//! secret and the session cookie are cached under an owner-only directory
//! (see `DaemonEndpoint::session_cache_dir`) so a repeated CLI invocation
//! reuses one long-lived login rather than minting a fresh device session
//! every process.

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use reqwest::header::{self, HeaderMap};
use reqwest::StatusCode;

use super::discovery::DaemonEndpoint;
use super::http::HttpError;

const DEVICE_BINDING_FILENAME: &str = "cli_device_binding";
const SESSION_COOKIE_FILENAME: &str = "cli_login_session";
/// Matches `HttpClient`'s own `DEFAULT_TIMEOUT`; a fresh client is built for
/// the login POST (see `login_client`), so the value isn't shared directly.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, Clone)]
pub(super) struct PassphraseSession {
    /// Full `aoe_session=<id>` pair, ready to send as the `Cookie` header.
    pub(super) cookie: String,
    /// Base64url (no padding) encoded 32-byte secret, sent as
    /// `X-Aoe-Device-Binding` on every request that uses `cookie`.
    pub(super) binding_secret: String,
}

/// In-memory cache for one `HttpClient`, backed by an on-disk cache (see
/// `DaemonEndpoint::session_cache_dir`).
#[derive(Debug, Clone, Default)]
pub(super) struct PassphraseSessionCache(Arc<RwLock<Option<PassphraseSession>>>);

impl PassphraseSessionCache {
    /// Cached session, loading it from disk on first use this process.
    pub(super) fn get(&self, endpoint: &DaemonEndpoint) -> Option<PassphraseSession> {
        if let Some(session) = self.0.read().unwrap_or_else(|e| e.into_inner()).clone() {
            return Some(session);
        }
        let loaded = load_persisted(endpoint)?;
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = Some(loaded.clone());
        Some(loaded)
    }

    /// Invalidate a rejected session so the next [`login`] call performs a
    /// fresh handshake instead of reusing it: drops the in-memory copy *and*
    /// deletes the persisted cookie file, so a subsequent [`get`](Self::get)
    /// can't reload the same rejected cookie straight back off disk. The
    /// device-binding secret is left in place; the new login reuses it.
    pub(super) fn invalidate(&self, endpoint: &DaemonEndpoint) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = None;
        if let Some(dir) = endpoint.session_cache_dir() {
            let _ = std::fs::remove_file(dir.join(SESSION_COOKIE_FILENAME));
        }
    }

    fn set(&self, session: PassphraseSession) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = Some(session);
    }

    #[cfg(test)]
    pub(super) fn set_for_test(&self, session: PassphraseSession) {
        self.set(session);
    }
}

/// Perform the passphrase login handshake and cache the resulting session.
pub(super) async fn login(
    endpoint: &DaemonEndpoint,
    cache: &PassphraseSessionCache,
) -> Result<PassphraseSession, HttpError> {
    let passphrase = endpoint
        .resolved_passphrase()
        .ok_or(HttpError::Unauthorized)?;
    let binding_secret = device_binding_secret(endpoint);
    let url = format!("{}/api/login", endpoint.base_url);
    let body = serde_json::json!({
        "passphrase": passphrase,
        "device_binding_secret": binding_secret,
    });
    let res = login_client()?.post(&url).json(&body).send().await?;
    let status = res.status();
    if !status.is_success() {
        let body = res.text().await.unwrap_or_default();
        return Err(match status {
            StatusCode::UNAUTHORIZED => HttpError::Unauthorized,
            _ => HttpError::Server { status, body },
        });
    }
    let cookie = extract_session_cookie(res.headers()).ok_or(HttpError::Unauthorized)?;
    let session = PassphraseSession {
        cookie,
        binding_secret,
    };
    persist_session(endpoint, &session);
    cache.set(session.clone());
    Ok(session)
}

/// A dedicated client for the login POST with redirects disabled: a
/// misconfigured or hostile daemon at the trusted URL could otherwise
/// 307/308 the request (which reqwest re-POSTs, body included) to a
/// different host, handing it the passphrase and device-binding secret.
fn login_client() -> Result<reqwest::Client, HttpError> {
    reqwest::Client::builder()
        .timeout(LOGIN_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(HttpError::Transport)
}

fn extract_session_cookie(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::SET_COOKIE)
        .iter()
        .find_map(|value| value.to_str().ok())
        .and_then(|raw| raw.split(';').next())
        .map(str::trim)
        .filter(|segment| {
            segment.len() > "aoe_session=".len() && segment.starts_with("aoe_session=")
        })
        .map(str::to_string)
}

fn device_binding_secret(endpoint: &DaemonEndpoint) -> String {
    let Some(dir) = endpoint.session_cache_dir() else {
        return generate_binding_secret();
    };
    let path = dir.join(DEVICE_BINDING_FILENAME);
    if let Ok(raw) = std::fs::read_to_string(&path) {
        let trimmed = raw.trim();
        if crate::server::login::decode_binding_secret(trimmed).is_some() {
            return trimmed.to_string();
        }
    }
    let secret = generate_binding_secret();
    write_owner_only(&path, &secret);
    secret
}

fn generate_binding_secret() -> String {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;
    use rand::RngExt;
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn persist_session(endpoint: &DaemonEndpoint, session: &PassphraseSession) {
    let Some(dir) = endpoint.session_cache_dir() else {
        return;
    };
    write_owner_only(&dir.join(SESSION_COOKIE_FILENAME), &session.cookie);
}

fn load_persisted(endpoint: &DaemonEndpoint) -> Option<PassphraseSession> {
    let dir = endpoint.session_cache_dir()?;
    let binding_secret = std::fs::read_to_string(dir.join(DEVICE_BINDING_FILENAME)).ok()?;
    let binding_secret = binding_secret.trim().to_string();
    crate::server::login::decode_binding_secret(&binding_secret)?;
    let cookie = std::fs::read_to_string(dir.join(SESSION_COOKIE_FILENAME)).ok()?;
    let cookie = cookie.trim().to_string();
    if cookie.len() <= "aoe_session=".len() || !cookie.starts_with("aoe_session=") {
        return None;
    }
    Some(PassphraseSession {
        cookie,
        binding_secret,
    })
}

/// Best-effort owner-only write: this is a cache, not a correctness
/// requirement, so a failure here just means the next call re-logs in.
fn write_owner_only(path: &Path, contents: &str) {
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let tmp = path.with_extension("tmp");
    // Owner-only permissions apply at creation, not after the fact: a
    // separate chmod would leave the secret briefly world-readable under
    // the process umask between the write and the permission change.
    let file = open_owner_only(&tmp);
    if file
        .and_then(|mut f| f.write_all(contents.as_bytes()))
        .is_err()
    {
        return;
    }
    let _ = std::fs::rename(&tmp, path);
}

#[cfg(unix)]
fn open_owner_only(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_owner_only(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::client::discovery::Source;

    fn local_endpoint(dir: &std::path::Path) -> DaemonEndpoint {
        DaemonEndpoint::new("http://127.0.0.1:8080".into(), None, Source::LocalDaemon)
            .with_local_passphrase_path(dir.join("serve.passphrase"))
    }

    #[test]
    fn extract_session_cookie_parses_first_attribute_only() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::SET_COOKIE,
            "aoe_session=abc123; HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000"
                .parse()
                .unwrap(),
        );
        assert_eq!(
            extract_session_cookie(&headers).as_deref(),
            Some("aoe_session=abc123")
        );
    }

    #[test]
    fn extract_session_cookie_none_when_missing_or_empty() {
        assert_eq!(extract_session_cookie(&HeaderMap::new()), None);

        let mut headers = HeaderMap::new();
        headers.insert(header::SET_COOKIE, "aoe_session=; Path=/".parse().unwrap());
        assert_eq!(extract_session_cookie(&headers), None);

        let mut headers = HeaderMap::new();
        headers.insert(header::SET_COOKIE, "other=value; Path=/".parse().unwrap());
        assert_eq!(extract_session_cookie(&headers), None);
    }

    #[test]
    fn generated_binding_secret_round_trips_through_server_decoder() {
        let secret = generate_binding_secret();
        assert!(crate::server::login::decode_binding_secret(&secret).is_some());
    }

    #[test]
    fn device_binding_secret_persists_and_reuses_across_calls() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = local_endpoint(dir.path());

        let first = device_binding_secret(&endpoint);
        let second = device_binding_secret(&endpoint);
        assert_eq!(first, second);
        assert!(crate::server::login::decode_binding_secret(&first).is_some());
    }

    #[test]
    fn device_binding_secret_regenerates_when_file_is_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = local_endpoint(dir.path());
        std::fs::write(
            dir.path().join(DEVICE_BINDING_FILENAME),
            "not-valid-base64!!",
        )
        .unwrap();

        let secret = device_binding_secret(&endpoint);
        assert!(crate::server::login::decode_binding_secret(&secret).is_some());
    }

    #[test]
    #[serial_test::serial]
    fn device_binding_secret_ephemeral_without_a_session_cache_dir() {
        let _env = crate::session::test_support::EnvGuard::unset(&["AOE_DAEMON_PASSPHRASE"]);
        let endpoint = DaemonEndpoint::new("https://remote.example.com".into(), None, Source::Env);
        let first = device_binding_secret(&endpoint);
        let second = device_binding_secret(&endpoint);
        assert_ne!(first, second, "no session dir means nothing to persist");
    }

    #[test]
    fn persist_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = local_endpoint(dir.path());
        let session = PassphraseSession {
            cookie: "aoe_session=xyz".to_string(),
            binding_secret: generate_binding_secret(),
        };

        assert!(load_persisted(&endpoint).is_none());
        // device_binding_secret must be persisted too, or the loaded cookie
        // is orphaned without its matching secret.
        write_owner_only(
            &dir.path().join(DEVICE_BINDING_FILENAME),
            &session.binding_secret,
        );
        persist_session(&endpoint, &session);

        let loaded = load_persisted(&endpoint).expect("session should round-trip");
        assert_eq!(loaded.cookie, session.cookie);
        assert_eq!(loaded.binding_secret, session.binding_secret);
    }

    #[test]
    fn load_persisted_rejects_malformed_cookie_file() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = local_endpoint(dir.path());
        write_owner_only(
            &dir.path().join(DEVICE_BINDING_FILENAME),
            &generate_binding_secret(),
        );
        write_owner_only(&dir.path().join(SESSION_COOKIE_FILENAME), "garbage");

        assert!(load_persisted(&endpoint).is_none());
    }

    #[test]
    fn cache_loads_persisted_session_once_and_reuses_in_memory() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = local_endpoint(dir.path());
        let secret = generate_binding_secret();
        write_owner_only(&dir.path().join(DEVICE_BINDING_FILENAME), &secret);
        write_owner_only(
            &dir.path().join(SESSION_COOKIE_FILENAME),
            "aoe_session=cached",
        );

        let cache = PassphraseSessionCache::default();
        let session = cache.get(&endpoint).expect("should load from disk");
        assert_eq!(session.cookie, "aoe_session=cached");

        // Deleting the on-disk file must not affect the in-memory cache.
        std::fs::remove_file(dir.path().join(SESSION_COOKIE_FILENAME)).unwrap();
        assert!(cache.get(&endpoint).is_some());
    }

    #[test]
    fn invalidate_clears_memory_and_deletes_persisted_cookie() {
        // A persisted cookie that survives invalidation would let the very
        // next `get` reload the same rejected session straight off disk,
        // so the retry-after-401 flow would resend it unchanged.
        let dir = tempfile::tempdir().unwrap();
        let endpoint = local_endpoint(dir.path());
        let secret = generate_binding_secret();
        write_owner_only(&dir.path().join(DEVICE_BINDING_FILENAME), &secret);
        write_owner_only(
            &dir.path().join(SESSION_COOKIE_FILENAME),
            "aoe_session=cached",
        );

        let cache = PassphraseSessionCache::default();
        cache.get(&endpoint).expect("should load from disk");

        cache.invalidate(&endpoint);

        assert!(
            !dir.path().join(SESSION_COOKIE_FILENAME).exists(),
            "invalidate must delete the persisted cookie file"
        );
        assert!(cache.get(&endpoint).is_none());

        // The device-binding secret survives: a fresh login reuses it
        // rather than binding a new "device" every time a session expires.
        assert!(dir.path().join(DEVICE_BINDING_FILENAME).exists());
    }
}
