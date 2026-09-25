//! Passphrase login for a native client.
//!
//! A daemon started with `--remote` mandates both a bearer token and a
//! passphrase, so reaching one from the TUI needs the same exchange the
//! dashboard performs: `POST /api/login` with the passphrase and a
//! client-generated device-binding secret, then present the returned session
//! plus that secret on every later request (`X-Aoe-Device-Binding` for REST,
//! an `aoe-device.<secret>` subprotocol for WebSockets).
//!
//! The passphrase is used once and never persisted; only the session and the
//! binding are stored, so a stolen registry cannot mint fresh sessions.
//!
//! [`pair`] mints the same kind of session from a one-time pairing code
//! instead, and needs no token.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use reqwest::header::{AUTHORIZATION, SET_COOKIE};
use thiserror::Error;

use crate::daemon::SessionCredential;

/// Raw length of the device-binding secret, matching the server's
/// `BINDING_SECRET_BYTES`.
const BINDING_SECRET_BYTES: usize = 32;

const LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Debug, Error)]
pub enum LoginError {
    #[error("invalid daemon base URL: {reason}")]
    InvalidBaseUrl { reason: &'static str },
    #[error("a passphrase requires HTTPS or a loopback HTTP URL")]
    InsecureTransport,
    #[error("could not generate a device binding secret")]
    Entropy,
    #[error("daemon transport error: {0}")]
    Transport(#[source] reqwest::Error),
    #[error("no passphrase login at this URL (HTTP 404)")]
    NotEnabled,
    #[error("incorrect passphrase")]
    Unauthorized,
    #[error("{}", crate::daemon::lockout_message(*.0))]
    RateLimited(Option<u64>),
    #[error("daemon returned HTTP {0}")]
    Status(reqwest::StatusCode),
    #[error("login succeeded but the daemon set no session cookie")]
    MissingSession,
    #[error("this daemon does not support pairing codes (HTTP 404); upgrade it or use --token")]
    PairingUnsupported,
    #[error("invalid or expired pairing code; create a new one on the remote")]
    InvalidCode,
    #[error("the daemon rejected the pairing request: {0}")]
    BadRequest(String),
}

/// Mint a fresh device-binding secret, base64url-encoded the way the server
/// decodes it.
pub fn new_binding_secret() -> Result<String, LoginError> {
    let mut bytes = [0u8; BINDING_SECRET_BYTES];
    getrandom::fill(&mut bytes).map_err(|_| LoginError::Entropy)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// Exchange a passphrase for a device-bound session.
///
/// `token` is still required: `--remote` keeps the token gate in front of the
/// passphrase wall, and `/api/login` is login-exempt but not token-exempt.
pub async fn login(
    base_url: &str,
    token: Option<&str>,
    passphrase: &str,
    binding: &str,
    allow_plaintext: bool,
) -> Result<SessionCredential, LoginError> {
    ensure_secure_transport(base_url, allow_plaintext)?;
    let url = format!("{}/api/login", base_url.trim_end_matches('/'));
    let http = http_client()?;

    let mut request = http.post(&url).json(&serde_json::json!({
        "passphrase": passphrase,
        "device_binding_secret": binding,
    }));
    if let Some(token) = token {
        request = request.header(AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = request.send().await.map_err(LoginError::Transport)?;

    let status = response.status();
    if !status.is_success() {
        return Err(match status {
            reqwest::StatusCode::NOT_FOUND => LoginError::NotEnabled,
            reqwest::StatusCode::UNAUTHORIZED => LoginError::Unauthorized,
            reqwest::StatusCode::TOO_MANY_REQUESTS => {
                LoginError::RateLimited(crate::daemon::retry_after_secs(response.headers()))
            }
            other => LoginError::Status(other),
        });
    }

    let session = response
        .headers()
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(session_from_set_cookie)
        .ok_or(LoginError::MissingSession)?;

    Ok(SessionCredential {
        session,
        binding: binding.to_string(),
    })
}

/// A redeemed pairing code.
pub struct Paired {
    pub credential: SessionCredential,
    /// The daemon machine's hostname, when it reports one.
    pub server_name: Option<String>,
}

/// Redeem a one-time pairing code for a device-bound session.
pub async fn pair(
    base_url: &str,
    code: &str,
    device_name: &str,
    binding: &str,
    allow_plaintext: bool,
) -> Result<Paired, LoginError> {
    ensure_secure_transport(base_url, allow_plaintext)?;
    let url = format!("{}/api/pair", base_url.trim_end_matches('/'));
    let response = http_client()?
        .post(&url)
        .json(&serde_json::json!({
            "code": code,
            "device_name": device_name,
            "device_binding_secret": binding,
        }))
        .send()
        .await
        .map_err(LoginError::Transport)?;

    #[derive(serde::Deserialize)]
    struct Response {
        session_id: String,
        #[serde(default)]
        server_name: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Rejection {
        message: String,
    }
    match response.status() {
        status if status.is_success() => {
            let paired: Response = response.json().await.map_err(LoginError::Transport)?;
            Ok(Paired {
                credential: SessionCredential {
                    session: paired.session_id,
                    binding: binding.to_string(),
                },
                server_name: paired.server_name,
            })
        }
        reqwest::StatusCode::NOT_FOUND => Err(LoginError::PairingUnsupported),
        reqwest::StatusCode::UNAUTHORIZED => Err(LoginError::InvalidCode),
        reqwest::StatusCode::TOO_MANY_REQUESTS => Err(LoginError::RateLimited(
            crate::daemon::retry_after_secs(response.headers()),
        )),
        reqwest::StatusCode::BAD_REQUEST => {
            let message = response
                .json::<Rejection>()
                .await
                .map_or_else(|_| "bad request".to_string(), |r| r.message);
            Err(LoginError::BadRequest(message))
        }
        other => Err(LoginError::Status(other)),
    }
}

fn http_client() -> Result<reqwest::Client, LoginError> {
    reqwest::Client::builder()
        .timeout(LOGIN_TIMEOUT)
        .user_agent(concat!("aoe-remote/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(LoginError::Transport)
}

/// A passphrase is a stronger secret than the token and travels in a body, so
/// it gets the same transport rule the bearer already has.
pub(crate) fn ensure_secure_transport(
    base_url: &str,
    allow_plaintext: bool,
) -> Result<(), LoginError> {
    let url = crate::daemon::native_url(base_url).map_err(|error| match error {
        crate::daemon::DaemonClientError::InvalidBaseUrl { reason } => {
            LoginError::InvalidBaseUrl { reason }
        }
        _ => LoginError::InvalidBaseUrl {
            reason: "could not parse URL",
        },
    })?;
    crate::daemon::ensure_credential_transport(&url, true, allow_plaintext)
        .map_err(|_| LoginError::InsecureTransport)
}

/// Pull `aoe_session` out of one `Set-Cookie` value. The daemon sets
/// `HttpOnly`/`SameSite` attributes a native client ignores.
fn session_from_set_cookie(value: &str) -> Option<String> {
    for part in value.split(';') {
        let part = part.trim();
        if let Some(id) = part.strip_prefix("aoe_session=") {
            if !id.is_empty() {
                return Some(id.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_lockout_names_its_wait_and_where_the_failures_come_from() {
        let app = axum::Router::new().fallback(|| async {
            (
                axum::http::StatusCode::TOO_MANY_REQUESTS,
                [("Retry-After", "725")],
                "{}",
            )
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });

        let error = pair(&url, "K7F-3QX", "laptop", "binding", false)
            .await
            .err()
            .expect("locked out");
        assert!(
            matches!(error, LoginError::RateLimited(Some(725))),
            "{error:?}"
        );
        let client =
            crate::daemon::DaemonClient::with_login(&url, Some("tok"), None, false).unwrap();
        let polled = client.list_sessions(None).await.expect_err("locked out");
        assert!(matches!(
            polled,
            crate::daemon::DaemonClientError::RateLimited {
                retry_after_secs: Some(725)
            }
        ));
        for message in [error.to_string(), polled.summary()] {
            assert!(message.contains("locked out for 13m"), "{message}");
            assert!(message.contains("aoe remote list"), "{message}");
        }
    }

    #[test]
    fn binding_secret_decodes_to_the_expected_length() {
        let encoded = new_binding_secret().unwrap();
        let raw = URL_SAFE_NO_PAD.decode(&encoded).unwrap();
        assert_eq!(raw.len(), BINDING_SECRET_BYTES);
    }

    #[test]
    fn binding_secrets_are_not_repeated() {
        assert_ne!(new_binding_secret().unwrap(), new_binding_secret().unwrap());
    }

    #[test]
    fn reads_the_session_out_of_a_full_cookie() {
        let value = "aoe_session=abc123; HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000";
        assert_eq!(session_from_set_cookie(value).as_deref(), Some("abc123"));
    }

    #[test]
    fn ignores_unrelated_and_empty_cookies() {
        assert_eq!(session_from_set_cookie("aoe_token=xyz; Path=/"), None);
        assert_eq!(session_from_set_cookie("aoe_session=; Path=/"), None);
    }

    #[test]
    fn passphrase_transport_rule() {
        for (url, allow_plaintext, allowed) in [
            ("http://mini.example.com:8080", false, false),
            ("http://192.168.1.20:8081", true, true),
            ("http://127.0.0.1:8080", false, true),
            ("https://mini.example.ts.net", false, true),
        ] {
            let result = ensure_secure_transport(url, allow_plaintext);
            assert_eq!(result.is_ok(), allowed, "{url} {allow_plaintext}");
            if !allowed {
                assert!(matches!(result, Err(LoginError::InsecureTransport)));
            }
        }
    }
}
