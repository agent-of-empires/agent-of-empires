//! One-time pairing codes that enroll a remote aoe client.
//!
//! The local owner mints a short code over the Unix socket; a client on
//! another machine redeems it at `POST /api/pair` for a device-bound login
//! session. Codes live in memory as SHA-256 hashes only, so a daemon restart
//! invalidates every pending code.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{extract::State, Json};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use super::auth::{resolve_client_ip, LocalAuthorization};
use super::peer::ConnectionPeer;
use super::AppState;

pub(crate) const CODE_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_LIVE_CODES: usize = 5;
/// Wrong guesses tolerated across all live codes before they are all burned.
/// The per-IP limiter coalesces bursts, so this is the bound that holds
/// against parallel or distributed guessing.
const MAX_FAILED_ATTEMPTS: u32 = 20;
/// Six Crockford base32 symbols: 30 bits.
const CODE_LEN: usize = 6;
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
const MAX_DEVICE_NAME_CHARS: usize = 64;

struct PendingCode {
    hash: [u8; 32],
    expires_at: Instant,
}

#[derive(Default)]
struct Pending {
    codes: Vec<PendingCode>,
    failures: u32,
}

#[derive(Default)]
pub struct PairingCodes {
    pending: Mutex<Pending>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Redemption {
    Accepted,
    Rejected,
    /// This failure exhausted the attempt budget and every live code was burned.
    Burned,
    Malformed,
}

impl PairingCodes {
    /// Mint a code, displayed as `XXX-XXX`, valid for [`CODE_TTL`].
    pub(crate) fn mint(&self) -> String {
        self.mint_at(Instant::now())
    }

    fn mint_at(&self, now: Instant) -> String {
        use rand::RngExt;
        let mut bytes = [0u8; CODE_LEN];
        rand::rng().fill(&mut bytes);
        // 32 divides 256, so masking is unbiased.
        let raw: String = bytes
            .iter()
            .map(|b| ALPHABET[(b & 0x1f) as usize] as char)
            .collect();

        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.codes.retain(|c| c.expires_at > now);
        if pending.codes.is_empty() {
            pending.failures = 0;
        }
        if pending.codes.len() >= MAX_LIVE_CODES {
            pending.codes.remove(0);
        }
        pending.codes.push(PendingCode {
            hash: hash_code(&raw),
            expires_at: now + CODE_TTL,
        });
        format!("{}-{}", &raw[..3], &raw[3..])
    }

    /// Consume `input` if it matches a live code. Every live code is compared
    /// in constant time so the match position does not leak.
    pub(crate) fn redeem(&self, input: &str) -> Redemption {
        self.redeem_at(input, Instant::now())
    }

    fn redeem_at(&self, input: &str, now: Instant) -> Redemption {
        let Some(normalized) = normalize_code(input) else {
            return Redemption::Malformed;
        };
        let presented = hash_code(&normalized);
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.codes.retain(|c| c.expires_at > now);
        let mut matched = None;
        for (index, code) in pending.codes.iter().enumerate() {
            if bool::from(code.hash.ct_eq(&presented)) {
                matched = Some(index);
            }
        }
        if let Some(index) = matched {
            pending.codes.remove(index);
            return Redemption::Accepted;
        }
        pending.failures = pending.failures.saturating_add(1);
        if pending.failures >= MAX_FAILED_ATTEMPTS && !pending.codes.is_empty() {
            pending.codes.clear();
            pending.failures = 0;
            return Redemption::Burned;
        }
        Redemption::Rejected
    }
}

fn hash_code(normalized: &str) -> [u8; 32] {
    Sha256::digest(normalized.as_bytes()).into()
}

/// Uppercase, drop separators, and apply Crockford's confusable mapping
/// (`O` to `0`, `I`/`L` to `1`). `None` unless exactly [`CODE_LEN`] symbols.
fn normalize_code(input: &str) -> Option<String> {
    let mut out = String::with_capacity(CODE_LEN);
    for c in input.chars() {
        let c = match c.to_ascii_uppercase() {
            '-' | ' ' => continue,
            'O' => '0',
            'I' | 'L' => '1',
            c if c.is_ascii() && ALPHABET.contains(&(c as u8)) => c,
            _ => return None,
        };
        out.push(c);
    }
    (out.len() == CODE_LEN).then_some(out)
}

fn clean_device_name(raw: &str) -> Option<String> {
    let name: String = raw
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_DEVICE_NAME_CHARS)
        .collect();
    (!name.is_empty()).then_some(name)
}

fn error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(serde_json::json!({ "error": code, "message": message })),
    )
        .into_response()
}

fn owner_only(local: Option<&LocalAuthorization>) -> Option<Response> {
    (!matches!(local, Some(LocalAuthorization::UnixOwner(_)))).then(|| {
        error(
            StatusCode::FORBIDDEN,
            "forbidden",
            "Only the daemon's own machine can do this",
        )
    })
}

/// POST /api/pair/codes
///
/// Local owner only: a LAN, tunnel or loopback TCP caller never mints, even
/// with a valid token or session.
pub async fn mint_handler(
    State(state): State<Arc<AppState>>,
    local: Option<axum::Extension<LocalAuthorization>>,
) -> Response {
    if let Some(refused) = owner_only(local.as_deref()) {
        return refused;
    }
    let code = state.pairing.mint();
    tracing::info!(target: "auth.pairing", "pairing code minted");
    Json(serde_json::json!({
        "code": code,
        "expires_in_secs": CODE_TTL.as_secs(),
    }))
    .into_response()
}

/// GET /api/pair/lockouts
///
/// Local owner only: IPs locked out by failed authentication or pairing,
/// longest remaining first, as `[{"ip", "remaining_secs"}]`.
pub async fn lockouts_handler(
    State(state): State<Arc<AppState>>,
    local: Option<axum::Extension<LocalAuthorization>>,
) -> Response {
    if let Some(refused) = owner_only(local.as_deref()) {
        return refused;
    }
    let mut merged = std::collections::HashMap::<IpAddr, u64>::new();
    for (ip, secs) in state
        .rate_limiter
        .lockouts()
        .await
        .into_iter()
        .chain(state.pairing_limiter.lockouts().await)
    {
        let entry = merged.entry(ip).or_default();
        *entry = (*entry).max(secs);
    }
    let mut lockouts: Vec<_> = merged.into_iter().collect();
    lockouts.sort_by_key(|(ip, secs)| (std::cmp::Reverse(*secs), *ip));
    Json(
        lockouts
            .into_iter()
            .map(|(ip, secs)| serde_json::json!({"ip": ip.to_string(), "remaining_secs": secs}))
            .collect::<Vec<_>>(),
    )
    .into_response()
}

/// DELETE /api/pair/lockouts
///
/// Local owner only: lift every lockout, so the operator can let a device
/// back in without restarting the daemon.
pub async fn clear_lockouts_handler(
    State(state): State<Arc<AppState>>,
    local: Option<axum::Extension<LocalAuthorization>>,
) -> Response {
    if let Some(refused) = owner_only(local.as_deref()) {
        return refused;
    }
    state.rate_limiter.clear().await;
    state.pairing_limiter.clear().await;
    tracing::info!(target: "auth.rate_limit", "lockouts cleared by the local owner");
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
pub struct PairRequest {
    code: String,
    device_name: String,
    /// Base64url of 32 random bytes, presented on every later request exactly
    /// as a passphrase login's binding is.
    device_binding_secret: String,
}

/// POST /api/pair
///
/// Auth-exempt. Redeems a pairing code for a device-bound session that
/// authenticates without the token and satisfies the passphrase wall.
pub async fn pair_handler(
    State(state): State<Arc<AppState>>,
    peer: ConnectionPeer,
    headers: axum::http::HeaderMap,
    body: Result<Json<PairRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let ConnectionPeer::Tcp(addr) = peer else {
        return error(
            StatusCode::CONFLICT,
            "conflict",
            "The local owner does not need to pair",
        );
    };
    let client_ip: IpAddr = resolve_client_ip(addr, &headers, state.behind_tunnel);

    if let Some(remaining) = state.pairing_limiter.check_locked(client_ip).await {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("Retry-After", remaining.to_string())],
            Json(serde_json::json!({
                "error": "rate_limited",
                "message": format!("Too many failed attempts. Try again in {remaining} seconds.")
            })),
        )
            .into_response();
    }

    let Ok(Json(request)) = body else {
        return error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "Expected code, device_name and device_binding_secret",
        );
    };
    let Some(binding) = super::login::decode_binding_secret(&request.device_binding_secret) else {
        return error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "device_binding_secret must be base64url of 32 random bytes",
        );
    };
    let Some(device_name) = clean_device_name(&request.device_name) else {
        return error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "device_name must not be empty",
        );
    };

    match state.pairing.redeem(&request.code) {
        Redemption::Accepted => {}
        Redemption::Malformed => {
            return error(
                StatusCode::BAD_REQUEST,
                "bad_request",
                "A pairing code is six letters and digits, like K7F-3QX",
            );
        }
        outcome @ (Redemption::Rejected | Redemption::Burned) => {
            let locked = state.pairing_limiter.record_failure(client_ip).await;
            tracing::warn!(
                target: "auth.pairing",
                ip = %client_ip,
                locked,
                burned = outcome == Redemption::Burned,
                "pairing code rejected"
            );
            return error(
                StatusCode::UNAUTHORIZED,
                "invalid_code",
                "Invalid or expired pairing code",
            );
        }
    }

    // A fresh code from the owner outranks this IP's earlier failures.
    state.pairing_limiter.record_success(client_ip).await;
    state.rate_limiter.record_success(client_ip).await;
    let user_agent = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");
    let session_id = state
        .login_manager
        .create_paired_session(&binding, &client_ip.to_string(), user_agent, &device_name)
        .await;
    tracing::info!(target: "auth.pairing", ip = %client_ip, device = %device_name, "device paired");

    Json(serde_json::json!({
        "session_id": session_id,
        "device_name": device_name,
        "server_name": crate::util::hostname(),
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minted_codes_are_formatted_crockford() {
        let codes = PairingCodes::default();
        let code = codes.mint();
        assert_eq!(code.len(), 7);
        assert_eq!(&code[3..4], "-");
        let raw = code.replace('-', "");
        assert!(raw.bytes().all(|b| ALPHABET.contains(&b)), "{code}");
        assert_eq!(normalize_code(&code).as_deref(), Some(raw.as_str()));
    }

    #[test]
    fn normalization_accepts_human_variants_and_rejects_garbage() {
        for (input, expected) in [
            ("k7f-3qx", Some("K7F3QX")),
            (" K7F 3QX ", Some("K7F3QX")),
            ("o1l-i00", Some("011100")),
            ("K7F3Q", None),
            ("K7F-3QXX", None),
            ("K7F-3QU", None),
            ("K7F-3Q!", None),
        ] {
            assert_eq!(normalize_code(input).as_deref(), expected, "{input}");
        }
    }

    #[test]
    fn a_code_redeems_once_and_expires() {
        let codes = PairingCodes::default();
        let now = Instant::now();
        let code = codes.mint_at(now);
        assert_eq!(
            codes.redeem_at(&code.to_lowercase(), now),
            Redemption::Accepted
        );
        assert_eq!(codes.redeem_at(&code, now), Redemption::Rejected);

        let code = codes.mint_at(now);
        assert_eq!(
            codes.redeem_at(&code, now + CODE_TTL + Duration::from_secs(1)),
            Redemption::Rejected
        );
    }

    #[test]
    fn live_codes_are_capped_oldest_first() {
        let codes = PairingCodes::default();
        let now = Instant::now();
        let first = codes.mint_at(now);
        let rest: Vec<_> = (0..MAX_LIVE_CODES).map(|_| codes.mint_at(now)).collect();
        assert_eq!(codes.redeem_at(&first, now), Redemption::Rejected);
        for code in rest {
            assert_eq!(codes.redeem_at(&code, now), Redemption::Accepted);
        }
    }

    #[test]
    fn too_many_wrong_guesses_burn_every_live_code() {
        let codes = PairingCodes::default();
        let now = Instant::now();
        let code = codes.mint_at(now);
        let wrong = if code == "000-000" {
            "000-001"
        } else {
            "000-000"
        };
        for _ in 1..MAX_FAILED_ATTEMPTS {
            assert_eq!(codes.redeem_at(wrong, now), Redemption::Rejected);
        }
        assert_eq!(codes.redeem_at("K7F-3Q!", now), Redemption::Malformed);
        assert_eq!(codes.redeem_at(wrong, now), Redemption::Burned);
        assert_eq!(codes.redeem_at(&code, now), Redemption::Rejected);
    }

    mod router {
        use axum::body::Body;
        use axum::extract::ConnectInfo;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        use crate::server::peer::ConnectionPeer;
        use crate::server::test_support;

        const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

        struct Call {
            method: &'static str,
            path: String,
            peer: ConnectionPeer,
            headers: Vec<(&'static str, String)>,
            body: Option<serde_json::Value>,
        }

        fn call(method: &'static str, path: &str) -> Call {
            Call {
                method,
                path: path.to_string(),
                peer: ConnectionPeer::Tcp("203.0.113.7:5555".parse().unwrap()),
                headers: Vec::new(),
                body: None,
            }
        }

        impl Call {
            fn from(mut self, peer: ConnectionPeer) -> Self {
                self.peer = peer;
                self
            }
            fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
                self.headers.push((name, value.into()));
                self
            }
            fn bearer(self) -> Self {
                self.header("authorization", format!("Bearer {TOKEN}"))
            }
            fn session(self, session: &str, binding: &str) -> Self {
                self.header("cookie", format!("aoe_session={session}"))
                    .header("x-aoe-device-binding", binding)
            }
            fn json(mut self, body: serde_json::Value) -> Self {
                self.body = Some(body);
                self
            }
            async fn send(self, app: &axum::Router) -> (StatusCode, serde_json::Value) {
                let mut builder = Request::builder()
                    .method(self.method)
                    .uri(&self.path)
                    .header("host", "localhost");
                for (name, value) in self.headers {
                    builder = builder.header(name, value);
                }
                let body = match self.body {
                    Some(json) => {
                        builder = builder.header("content-type", "application/json");
                        Body::from(json.to_string())
                    }
                    None => Body::empty(),
                };
                let mut request = builder.body(body).unwrap();
                request.extensions_mut().insert(ConnectInfo(self.peer));
                let response = app.clone().oneshot(request).await.unwrap();
                let status = response.status();
                let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                (status, serde_json::from_slice(&bytes).unwrap_or_default())
            }
        }

        fn binding(byte: u8) -> String {
            use base64::Engine;
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([byte; 32])
        }

        fn app(passphrase: Option<&'static str>) -> axum::Router {
            test_support::build_router_for_test(state(passphrase))
        }

        fn state(passphrase: Option<&'static str>) -> std::sync::Arc<crate::server::AppState> {
            test_support::build_test_app_state_with_policy_configured(
                Vec::new(),
                vec!["localhost".into()],
                Vec::new(),
                Some(TOKEN.into()),
                |state| {
                    state.login_manager =
                        std::sync::Arc::new(crate::server::login::LoginManager::new(passphrase));
                },
            )
        }

        async fn mint(app: &axum::Router) -> String {
            let (status, body) = call("POST", "/api/pair/codes")
                .from(ConnectionPeer::UnixOwner { uid: 0 })
                .send(app)
                .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            body["code"].as_str().unwrap().to_string()
        }

        async fn pair(app: &axum::Router, code: &str, secret: &str) -> (StatusCode, String) {
            let (status, body) = call("POST", "/api/pair")
                .json(serde_json::json!({
                    "code": code,
                    "device_name": "laptop",
                    "device_binding_secret": secret,
                }))
                .send(app)
                .await;
            (
                status,
                body["session_id"].as_str().unwrap_or_default().to_string(),
            )
        }

        #[tokio::test]
        async fn only_the_unix_owner_mints_codes() {
            let app = app(None);
            for peer in [
                ConnectionPeer::Tcp("203.0.113.7:5555".parse().unwrap()),
                ConnectionPeer::Tcp("127.0.0.1:5555".parse().unwrap()),
            ] {
                let (status, _) = call("POST", "/api/pair/codes")
                    .from(peer)
                    .bearer()
                    .send(&app)
                    .await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{peer:?}");
            }
            let code = mint(&app).await;
            let secret = binding(1);
            let (_, session) = pair(&app, &code, &secret).await;
            let (status, _) = call("POST", "/api/pair/codes")
                .session(&session, &secret)
                .send(&app)
                .await;
            assert_eq!(status, StatusCode::FORBIDDEN, "a paired device cannot mint");
        }

        #[tokio::test]
        async fn a_paired_session_authenticates_without_the_token_until_revoked() {
            let state = state(None);
            let app = test_support::build_router_for_test(state.clone());
            let (status, _) = call("GET", "/api/sessions").send(&app).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);

            let code = mint(&app).await;
            let secret = binding(1);
            assert_eq!(
                pair(&app, "ZZZ-ZZZ", &secret).await.0,
                StatusCode::UNAUTHORIZED
            );
            let (status, session) = pair(&app, &code, &secret).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(
                pair(&app, &code, &binding(2)).await.0,
                StatusCode::UNAUTHORIZED
            );

            let (status, _) = call("GET", "/api/sessions")
                .session(&session, &secret)
                .send(&app)
                .await;
            assert_eq!(status, StatusCode::OK);
            let (status, _) = call("GET", "/api/sessions")
                .session(&session, &binding(9))
                .send(&app)
                .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "wrong binding");

            state.token_manager.rotate().await;
            state.token_manager.clear_previous().await;
            let (status, _) = call("GET", "/api/sessions")
                .session(&session, &secret)
                .send(&app)
                .await;
            assert_eq!(status, StatusCode::OK, "token rotation leaves the session");

            let (_, devices) = call("GET", "/api/devices")
                .session(&session, &secret)
                .send(&app)
                .await;
            assert_eq!(devices[0]["device_name"], "laptop");
            assert_eq!(devices[0]["session_id"], session.as_str());

            let (status, _) = call("DELETE", &format!("/api/login/sessions/{session}"))
                .from(ConnectionPeer::UnixOwner { uid: 0 })
                .send(&app)
                .await;
            assert_eq!(status, StatusCode::OK);
            let (status, _) = call("GET", "/api/sessions")
                .session(&session, &secret)
                .send(&app)
                .await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "revoked");
        }

        #[tokio::test]
        async fn a_paired_session_passes_the_passphrase_wall() {
            let app = app(Some("correct horse battery"));
            let code = mint(&app).await;
            let secret = binding(3);
            let (status, session) = pair(&app, &code, &secret).await;
            assert_eq!(status, StatusCode::OK);

            let (status, body) = call("GET", "/api/sessions").bearer().send(&app).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
            assert_eq!(
                body["error"], "login_required",
                "the token alone still needs the passphrase"
            );
            let (status, _) = call("GET", "/api/sessions")
                .session(&session, &secret)
                .send(&app)
                .await;
            assert_eq!(status, StatusCode::OK);
        }

        #[tokio::test]
        async fn a_stale_client_on_the_same_ip_cannot_block_pairing() {
            let state = state(None);
            let app = test_support::build_router_for_test(state.clone());
            let ip = "203.0.113.7".parse().unwrap();
            for _ in 0..8 {
                let (status, _) = call("GET", "/api/sessions")
                    .session("revoked", &binding(4))
                    .send(&app)
                    .await;
                assert_eq!(status, StatusCode::UNAUTHORIZED);
            }
            assert_eq!(
                state.rate_limiter.check_locked(ip).await,
                None,
                "a request without a token is not a guess"
            );

            state.rate_limiter.lock_out(ip).await;
            let (status, _) = call("GET", "/api/sessions").bearer().send(&app).await;
            assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
            let code = mint(&app).await;
            let secret = binding(5);
            let (status, session) = pair(&app, &code, &secret).await;
            assert_eq!(
                status,
                StatusCode::OK,
                "generic lockouts do not gate pairing"
            );
            let (status, _) = call("GET", "/api/sessions")
                .session(&session, &secret)
                .send(&app)
                .await;
            assert_eq!(status, StatusCode::OK, "pairing lifts the IP's lockout");
        }

        #[tokio::test]
        async fn only_the_unix_owner_lists_and_clears_lockouts() {
            let state = state(None);
            let app = test_support::build_router_for_test(state.clone());
            let ip = "198.51.100.2".parse().unwrap();
            state.pairing_limiter.lock_out(ip).await;
            for method in ["GET", "DELETE"] {
                let (status, _) = call(method, "/api/pair/lockouts").bearer().send(&app).await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{method} over TCP");
            }
            let owner = || ConnectionPeer::UnixOwner { uid: 0 };
            let (status, listed) = call("GET", "/api/pair/lockouts")
                .from(owner())
                .send(&app)
                .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(listed[0]["ip"], "198.51.100.2");
            assert!(listed[0]["remaining_secs"].as_u64().unwrap() > 0);

            let (status, _) = call("DELETE", "/api/pair/lockouts")
                .from(owner())
                .send(&app)
                .await;
            assert_eq!(status, StatusCode::NO_CONTENT);
            assert_eq!(state.pairing_limiter.check_locked(ip).await, None);
        }

        #[tokio::test]
        async fn repeated_wrong_codes_lock_the_caller_out() {
            let app = app(None);
            let code = mint(&app).await;
            let mut last = StatusCode::OK;
            for _ in 0..8 {
                last = pair(&app, "ZZZ-ZZZ", &binding(1)).await.0;
                // Failures inside the limiter's coalesce window count once.
                tokio::time::sleep(std::time::Duration::from_millis(510)).await;
            }
            assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
            assert_eq!(
                pair(&app, &code, &binding(1)).await.0,
                StatusCode::TOO_MANY_REQUESTS,
                "a locked-out caller cannot redeem even a valid code"
            );
        }
    }

    #[test]
    fn device_names_are_trimmed_bounded_and_required() {
        assert_eq!(clean_device_name("  laptop\n").as_deref(), Some("laptop"));
        assert_eq!(clean_device_name(" \t "), None);
        assert_eq!(
            clean_device_name(&"x".repeat(200)).map(|n| n.chars().count()),
            Some(MAX_DEVICE_NAME_CHARS)
        );
    }
}
