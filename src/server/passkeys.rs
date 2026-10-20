//! Passkey (WebAuthn) sign-in for the web dashboard, alongside the passphrase.
//!
//! The relying party is derived per request from a strictly validated `Origin`, so a
//! passkey is enrolled and used per origin. Only public keys and counters are stored.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use webauthn_rs::prelude::{
    Passkey, PasskeyAuthentication, PasskeyRegistration, PublicKeyCredential,
    RegisterPublicKeyCredential, Url, Uuid, Webauthn, WebauthnBuilder,
};

use super::access::norm_host;
use super::auth::{resolve_client_ip, AuthenticatedSession};
use super::login::{decode_binding_secret, issue_session, session_id_from_headers};
use super::secure_store::{check_path_security, write_private};
use super::AppState;
use crate::util::now_ms;

const STORE_FILE: &str = "passkeys.json";
const STORE_SCHEMA_VERSION: u32 = 1;
const MAX_CREDENTIALS: usize = 20;
const MAX_LABEL_CHARS: usize = 64;
const CEREMONY_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_REGISTRATIONS: usize = 32;
const MAX_AUTHENTICATIONS: usize = 100;
const MAX_AUTHENTICATIONS_PER_IP: usize = 5;

/// The WebAuthn relying party an origin maps to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    origin: String,
    rp_id: String,
}

impl Profile {
    /// Accept only a bare https origin (or `http://localhost`) that the DNS-rebinding
    /// allowlist already trusts and that matches the request `Host`.
    pub(super) fn resolve(
        origin: Option<&str>,
        host: Option<&str>,
        allowed_origins: &[String],
    ) -> Option<Self> {
        let url = Url::parse(origin?).ok()?;
        let hostname = url.domain()?.to_ascii_lowercase();
        let secure = match url.scheme() {
            "https" => true,
            "http" => hostname == "localhost",
            _ => false,
        };
        let bare = url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none();
        if !secure || !bare || hostname.ends_with('.') {
            return None;
        }
        let canonical = url.origin().ascii_serialization();
        if !allowed_origins.contains(&canonical) || norm_host(host?) != hostname {
            return None;
        }
        Some(Self {
            origin: canonical,
            rp_id: hostname,
        })
    }

    fn from_headers(headers: &HeaderMap, allowed_origins: &[String]) -> Option<Self> {
        if headers.get_all(axum::http::header::ORIGIN).iter().count() != 1 {
            return None;
        }
        Self::resolve(
            header_str(headers, axum::http::header::ORIGIN),
            header_str(headers, axum::http::header::HOST),
            allowed_origins,
        )
    }

    fn webauthn(&self) -> anyhow::Result<Webauthn> {
        let origin = Url::parse(&self.origin)?;
        Ok(WebauthnBuilder::new(&self.rp_id, &origin)?
            .rp_name("aoe")
            .build()?)
    }
}

fn header_str(headers: &HeaderMap, name: axum::http::header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

#[derive(Clone, Serialize, Deserialize)]
struct StoredPasskey {
    /// base64url (no pad) credential id.
    credential_id: String,
    label: String,
    rp_id: String,
    origin: String,
    created_at_ms: u64,
    last_used_at_ms: Option<u64>,
    passkey: Passkey,
}

impl StoredPasskey {
    fn matches(&self, profile: &Profile) -> bool {
        self.rp_id == profile.rp_id && self.origin == profile.origin
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct PersistedStore {
    schema_version: u32,
    /// Stable id for the single aoe owner, reused for every registration.
    user_handle: String,
    credentials: Vec<StoredPasskey>,
}

impl PersistedStore {
    fn new() -> Self {
        Self {
            schema_version: STORE_SCHEMA_VERSION,
            user_handle: Uuid::new_v4().to_string(),
            credentials: Vec::new(),
        }
    }

    fn user_id(&self) -> Uuid {
        Uuid::parse_str(&self.user_handle).unwrap_or_else(|_| Uuid::nil())
    }

    fn passkeys_for(&self, profile: &Profile) -> Vec<Passkey> {
        self.credentials
            .iter()
            .filter(|c| c.matches(profile))
            .map(|c| c.passkey.clone())
            .collect()
    }
}

/// Metadata the dashboard shows; never includes key material.
#[derive(Serialize)]
pub struct PasskeyInfo {
    credential_id: String,
    label: String,
    origin: String,
    created_at_ms: u64,
    last_used_at_ms: Option<u64>,
}

impl From<&StoredPasskey> for PasskeyInfo {
    fn from(c: &StoredPasskey) -> Self {
        Self {
            credential_id: c.credential_id.clone(),
            label: c.label.clone(),
            origin: c.origin.clone(),
            created_at_ms: c.created_at_ms,
            last_used_at_ms: c.last_used_at_ms,
        }
    }
}

struct Ceremony<S> {
    state: S,
    profile: Profile,
    /// Session id for registrations, client IP for logins.
    owner: String,
    label: String,
    expires_at: Instant,
}

#[derive(Debug)]
pub enum PasskeyError {
    /// No passkey is enrolled for this origin.
    Unavailable,
    TooManyCeremonies,
    LimitReached,
    /// Unknown, expired, consumed or mismatched ceremony.
    InvalidCeremony,
    Verification(String),
    Storage(String),
}

pub struct PasskeyManager {
    path: Option<PathBuf>,
    /// Set when an existing store could not be loaded; the file is then never overwritten.
    load_failed: bool,
    store: Mutex<PersistedStore>,
    registrations: StdMutex<HashMap<String, Ceremony<PasskeyRegistration>>>,
    authentications: StdMutex<HashMap<String, Ceremony<PasskeyAuthentication>>>,
}

fn lock<T>(m: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn purge_expired<S>(map: &mut HashMap<String, Ceremony<S>>) {
    let now = Instant::now();
    map.retain(|_, c| c.expires_at > now);
}

impl PasskeyManager {
    pub fn in_memory() -> Self {
        Self::build(None, false, PersistedStore::new())
    }

    /// Load `<app_dir>/passkeys.json`, creating the store on first use.
    pub fn with_persistence(app_dir: &Path) -> Self {
        let path = app_dir.join(STORE_FILE);
        match load_store(&path) {
            Ok(Some(store)) => Self::build(Some(path), false, store),
            Ok(None) => Self::build(Some(path), false, PersistedStore::new()),
            Err(e) => {
                tracing::warn!(
                    target: "auth.passkey",
                    error = %e,
                    "could not load passkey store; passkeys disabled and the file left untouched"
                );
                Self::build(Some(path), true, PersistedStore::new())
            }
        }
    }

    fn build(path: Option<PathBuf>, load_failed: bool, store: PersistedStore) -> Self {
        Self {
            path,
            load_failed,
            store: Mutex::new(store),
            registrations: StdMutex::new(HashMap::new()),
            authentications: StdMutex::new(HashMap::new()),
        }
    }

    pub async fn available_for(&self, profile: &Profile) -> bool {
        !self.load_failed && !self.store.lock().await.passkeys_for(profile).is_empty()
    }

    pub async fn list(&self) -> Vec<PasskeyInfo> {
        let store = self.store.lock().await;
        store.credentials.iter().map(PasskeyInfo::from).collect()
    }

    async fn persist(&self, next: &PersistedStore) -> Result<(), PasskeyError> {
        if self.load_failed {
            return Err(PasskeyError::Storage("passkey store failed to load".into()));
        }
        let Some(path) = self.path.clone() else {
            return Ok(());
        };
        let bytes = serde_json::to_vec_pretty(next)
            .map_err(|e| PasskeyError::Storage(format!("serialize: {e}")))?;
        tokio::task::spawn_blocking(move || write_private(&path, &bytes))
            .await
            .map_err(|e| PasskeyError::Storage(format!("join: {e}")))?
            .map_err(|e| PasskeyError::Storage(e.to_string()))
    }

    pub async fn remove(&self, credential_id: &str) -> Result<bool, PasskeyError> {
        let mut store = self.store.lock().await;
        let mut next = store.clone();
        next.credentials
            .retain(|c| c.credential_id != credential_id);
        if next.credentials.len() == store.credentials.len() {
            return Ok(false);
        }
        self.persist(&next).await?;
        *store = next;
        Ok(true)
    }

    pub async fn begin_login(
        &self,
        profile: &Profile,
        ip: IpAddr,
    ) -> Result<(String, serde_json::Value), PasskeyError> {
        if self.load_failed {
            return Err(PasskeyError::Unavailable);
        }
        let creds = self.store.lock().await.passkeys_for(profile);
        if creds.is_empty() {
            return Err(PasskeyError::Unavailable);
        }
        let webauthn = profile
            .webauthn()
            .map_err(|e| PasskeyError::Verification(e.to_string()))?;
        let (challenge, state) = webauthn
            .start_passkey_authentication(&creds)
            .map_err(|e| PasskeyError::Verification(e.to_string()))?;
        let owner = ip.to_string();
        let tx = super::generate_token();
        {
            let mut map = lock(&self.authentications);
            purge_expired(&mut map);
            let per_ip = map.values().filter(|c| c.owner == owner).count();
            if map.len() >= MAX_AUTHENTICATIONS || per_ip >= MAX_AUTHENTICATIONS_PER_IP {
                return Err(PasskeyError::TooManyCeremonies);
            }
            map.insert(
                tx.clone(),
                Ceremony {
                    state,
                    profile: profile.clone(),
                    owner,
                    label: String::new(),
                    expires_at: Instant::now() + CEREMONY_TTL,
                },
            );
        }
        let options = serde_json::to_value(challenge)
            .map_err(|e| PasskeyError::Verification(e.to_string()))?;
        Ok((tx, options))
    }

    /// Verify an assertion and persist the new counter before returning. The ceremony is
    /// consumed whether or not verification succeeds.
    pub async fn finish_login(
        &self,
        tx: &str,
        profile: &Profile,
        credential: &PublicKeyCredential,
    ) -> Result<(), PasskeyError> {
        let ceremony = lock(&self.authentications)
            .remove(tx)
            .filter(|c| c.expires_at > Instant::now() && c.profile == *profile)
            .ok_or(PasskeyError::InvalidCeremony)?;
        let webauthn = ceremony
            .profile
            .webauthn()
            .map_err(|e| PasskeyError::Verification(e.to_string()))?;
        let result = webauthn
            .finish_passkey_authentication(credential, &ceremony.state)
            .map_err(|e| PasskeyError::Verification(e.to_string()))?;

        let mut store = self.store.lock().await;
        let mut next = store.clone();
        let entry = next
            .credentials
            .iter_mut()
            .find(|c| c.matches(profile) && c.passkey.cred_id() == result.cred_id())
            .ok_or_else(|| PasskeyError::Verification("unknown credential".into()))?;
        entry.passkey.update_credential(&result);
        entry.last_used_at_ms = Some(now_ms());
        self.persist(&next).await?;
        *store = next;
        // Outstanding ceremonies hold a pre-update counter snapshot; drop them so a stale
        // one cannot accept a counter at or below the one just stored.
        lock(&self.authentications).clear();
        Ok(())
    }

    pub async fn begin_register(
        &self,
        profile: &Profile,
        session_id: &str,
        label: &str,
    ) -> Result<(String, serde_json::Value), PasskeyError> {
        let store = self.store.lock().await;
        if self.load_failed || store.credentials.len() >= MAX_CREDENTIALS {
            return Err(PasskeyError::LimitReached);
        }
        let exclude = store
            .passkeys_for(profile)
            .iter()
            .map(|p| p.cred_id().clone())
            .collect();
        let webauthn = profile
            .webauthn()
            .map_err(|e| PasskeyError::Verification(e.to_string()))?;
        let (challenge, state) = webauthn
            .start_passkey_registration(store.user_id(), "aoe", "aoe dashboard", Some(exclude))
            .map_err(|e| PasskeyError::Verification(e.to_string()))?;
        drop(store);
        let tx = super::generate_token();
        {
            let mut map = lock(&self.registrations);
            purge_expired(&mut map);
            map.retain(|_, c| c.owner != session_id);
            if map.len() >= MAX_REGISTRATIONS {
                return Err(PasskeyError::TooManyCeremonies);
            }
            map.insert(
                tx.clone(),
                Ceremony {
                    state,
                    profile: profile.clone(),
                    owner: session_id.to_string(),
                    label: sanitize_label(label),
                    expires_at: Instant::now() + CEREMONY_TTL,
                },
            );
        }
        let options = serde_json::to_value(challenge)
            .map_err(|e| PasskeyError::Verification(e.to_string()))?;
        Ok((tx, options))
    }

    pub async fn finish_register(
        &self,
        tx: &str,
        profile: &Profile,
        session_id: &str,
        credential: &RegisterPublicKeyCredential,
    ) -> Result<PasskeyInfo, PasskeyError> {
        let ceremony = lock(&self.registrations)
            .remove(tx)
            .filter(|c| {
                c.expires_at > Instant::now() && c.profile == *profile && c.owner == session_id
            })
            .ok_or(PasskeyError::InvalidCeremony)?;
        let webauthn = ceremony
            .profile
            .webauthn()
            .map_err(|e| PasskeyError::Verification(e.to_string()))?;
        let passkey = webauthn
            .finish_passkey_registration(credential, &ceremony.state)
            .map_err(|e| PasskeyError::Verification(e.to_string()))?;

        let mut store = self.store.lock().await;
        if store.credentials.len() >= MAX_CREDENTIALS {
            return Err(PasskeyError::LimitReached);
        }
        let credential_id = URL_SAFE_NO_PAD.encode(passkey.cred_id().as_ref());
        if store
            .credentials
            .iter()
            .any(|c| c.credential_id == credential_id)
        {
            return Err(PasskeyError::Verification("duplicate credential".into()));
        }
        let stored = StoredPasskey {
            credential_id,
            label: ceremony.label,
            rp_id: profile.rp_id.clone(),
            origin: profile.origin.clone(),
            created_at_ms: now_ms(),
            last_used_at_ms: None,
            passkey,
        };
        let info = PasskeyInfo::from(&stored);
        let mut next = store.clone();
        next.credentials.push(stored);
        self.persist(&next).await?;
        *store = next;
        Ok(info)
    }
}

fn sanitize_label(raw: &str) -> String {
    let label: String = raw
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_LABEL_CHARS)
        .collect();
    let label = label.trim();
    if label.is_empty() {
        "Passkey".to_string()
    } else {
        label.to_string()
    }
}

fn load_store(path: &Path) -> anyhow::Result<Option<PersistedStore>> {
    check_path_security(path)?;
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let store: PersistedStore = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        store.schema_version == STORE_SCHEMA_VERSION,
        "unsupported passkey store schema {}",
        store.schema_version
    );
    Ok(Some(store))
}

// ── Handlers ───────────────────────────────────────────────────────────────

fn json_error(status: StatusCode, error: &str, message: &str) -> Response {
    (
        status,
        Json(serde_json::json!({ "error": error, "message": message })),
    )
        .into_response()
}

fn error_response(err: &PasskeyError) -> Response {
    match err {
        PasskeyError::Unavailable => json_error(
            StatusCode::NOT_FOUND,
            "not_found",
            "No passkey for this origin",
        ),
        PasskeyError::TooManyCeremonies => json_error(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "Too many passkey requests in progress",
        ),
        PasskeyError::LimitReached => json_error(
            StatusCode::CONFLICT,
            "limit_reached",
            "Passkey limit reached",
        ),
        PasskeyError::InvalidCeremony | PasskeyError::Verification(_) => json_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Passkey verification failed",
        ),
        PasskeyError::Storage(_) => json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage_error",
            "Could not save passkey",
        ),
    }
}

fn origin_rejected() -> Response {
    json_error(
        StatusCode::FORBIDDEN,
        "origin_not_allowed",
        "Passkeys need an https origin (or localhost) this dashboard is served on",
    )
}

fn login_disabled() -> Response {
    json_error(StatusCode::NOT_FOUND, "not_found", "Login is not enabled")
}

/// POST /api/login/passkey/start
pub async fn login_start_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
) -> Response {
    let ip = resolve_client_ip(addr, &headers);
    if !state.login_manager.is_enabled() {
        return login_disabled();
    }
    if let Some(remaining) = state.rate_limiter.check_locked(ip).await {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("Retry-After", remaining.to_string())],
            Json(serde_json::json!({ "error": "rate_limited" })),
        )
            .into_response();
    }
    let Some(profile) = Profile::from_headers(&headers, &state.allowed_origins) else {
        return origin_rejected();
    };
    match state.passkey_manager.begin_login(&profile, ip).await {
        Ok((tx, options)) => {
            Json(serde_json::json!({ "transaction_id": tx, "options": options })).into_response()
        }
        Err(e) => error_response(&e),
    }
}

#[derive(Deserialize)]
pub struct LoginFinishRequest {
    transaction_id: String,
    credential: PublicKeyCredential,
    device_binding_secret: String,
}

/// POST /api/login/passkey/finish
pub async fn login_finish_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    body: Result<Json<LoginFinishRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let ip = resolve_client_ip(addr, &headers);
    if !state.login_manager.is_enabled() {
        return login_disabled();
    }
    if let Some(remaining) = state.rate_limiter.check_locked(ip).await {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("Retry-After", remaining.to_string())],
            Json(serde_json::json!({ "error": "rate_limited" })),
        )
            .into_response();
    }
    let Ok(Json(req)) = body else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "Invalid passkey login body",
        );
    };
    let Some(binding) = decode_binding_secret(&req.device_binding_secret) else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "device_binding_secret must be base64url of 32 random bytes",
        );
    };
    let Some(profile) = Profile::from_headers(&headers, &state.allowed_origins) else {
        return origin_rejected();
    };
    match state
        .passkey_manager
        .finish_login(&req.transaction_id, &profile, &req.credential)
        .await
    {
        Ok(()) => {
            state.rate_limiter.record_success(ip).await;
            tracing::info!(target: "auth.passkey", ip = %ip, "passkey login successful");
            issue_session(&state, &binding, ip, &headers).await
        }
        Err(e) => {
            let locked = state.rate_limiter.record_failure(ip).await;
            tracing::warn!(
                target: "auth.passkey",
                ip = %ip,
                locked,
                error = ?e,
                "passkey login failed"
            );
            error_response(&e)
        }
    }
}

#[derive(Deserialize, Default)]
pub struct RegisterStartRequest {
    #[serde(default)]
    label: String,
}

/// The session enrolling a passkey. The auth middleware sets it, except on the loopback
/// bypass, where the cookie and device binding are validated here instead.
async fn enrolling_session(
    state: &AppState,
    session: Option<Extension<AuthenticatedSession>>,
    headers: &HeaderMap,
) -> Option<String> {
    if let Some(Extension(AuthenticatedSession(id))) = session {
        return Some(id);
    }
    let id = session_id_from_headers(headers)?;
    let binding = decode_binding_secret(header_str(
        headers,
        axum::http::HeaderName::from_static("x-aoe-device-binding"),
    )?)?;
    state
        .login_manager
        .validate_session(&id, &binding)
        .await
        .then_some(id)
}

/// POST /api/passkeys/register/start (elevation-gated in the auth middleware)
pub async fn register_start_handler(
    State(state): State<Arc<AppState>>,
    session: Option<Extension<AuthenticatedSession>>,
    headers: HeaderMap,
    body: Option<Json<RegisterStartRequest>>,
) -> Response {
    if !state.login_manager.is_enabled() {
        return login_disabled();
    }
    let Some(session_id) = enrolling_session(&state, session, &headers).await else {
        return json_error(StatusCode::UNAUTHORIZED, "login_required", "Sign in first");
    };
    let Some(profile) = Profile::from_headers(&headers, &state.allowed_origins) else {
        return origin_rejected();
    };
    let label = body.map(|Json(b)| b.label).unwrap_or_default();
    match state
        .passkey_manager
        .begin_register(&profile, &session_id, &label)
        .await
    {
        Ok((tx, options)) => {
            Json(serde_json::json!({ "transaction_id": tx, "options": options })).into_response()
        }
        Err(e) => error_response(&e),
    }
}

#[derive(Deserialize)]
pub struct RegisterFinishRequest {
    transaction_id: String,
    credential: RegisterPublicKeyCredential,
}

/// POST /api/passkeys/register/finish (elevation-gated in the auth middleware)
pub async fn register_finish_handler(
    State(state): State<Arc<AppState>>,
    session: Option<Extension<AuthenticatedSession>>,
    headers: HeaderMap,
    body: Result<Json<RegisterFinishRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if !state.login_manager.is_enabled() {
        return login_disabled();
    }
    let Some(session_id) = enrolling_session(&state, session, &headers).await else {
        return json_error(StatusCode::UNAUTHORIZED, "login_required", "Sign in first");
    };
    let Ok(Json(req)) = body else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "Invalid passkey body",
        );
    };
    let Some(profile) = Profile::from_headers(&headers, &state.allowed_origins) else {
        return origin_rejected();
    };
    match state
        .passkey_manager
        .finish_register(&req.transaction_id, &profile, &session_id, &req.credential)
        .await
    {
        Ok(info) => {
            tracing::info!(target: "auth.passkey", "passkey enrolled");
            Json(serde_json::json!({ "ok": true, "passkey": info })).into_response()
        }
        Err(e) => {
            tracing::warn!(target: "auth.passkey", error = ?e, "passkey enrolment failed");
            error_response(&e)
        }
    }
}

/// GET /api/passkeys
pub async fn list_handler(State(state): State<Arc<AppState>>) -> Json<Vec<PasskeyInfo>> {
    Json(state.passkey_manager.list().await)
}

/// DELETE /api/passkeys/{id} (elevation-gated in the auth middleware)
pub async fn delete_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    match state.passkey_manager.remove(&id).await {
        Ok(removed) => {
            tracing::info!(target: "auth.passkey", removed, "passkey removed");
            Json(serde_json::json!({ "ok": true, "removed": removed })).into_response()
        }
        Err(e) => error_response(&e),
    }
}

/// Whether the browser's current origin can sign in with a passkey, for the login page.
pub(super) async fn availability(
    state: &AppState,
    headers: &HeaderMap,
    origin: Option<&str>,
) -> bool {
    let Some(profile) = Profile::resolve(
        origin,
        header_str(headers, axum::http::header::HOST),
        &state.allowed_origins,
    ) else {
        return false;
    };
    state.passkey_manager.available_for(&profile).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use webauthn_authenticator_rs::softpasskey::SoftPasskey;
    use webauthn_authenticator_rs::WebauthnAuthenticator;
    use webauthn_rs::prelude::CreationChallengeResponse;
    use webauthn_rs::prelude::RequestChallengeResponse;

    const ORIGIN: &str = "https://aoe.example.com";
    const OTHER_ORIGIN: &str = "https://other.example.com";
    /// A client accepts this for the RP id `aoe.example.com`; the server must not.
    const SUBDOMAIN_ORIGIN: &str = "https://evil.aoe.example.com";

    fn allowed() -> Vec<String> {
        vec![ORIGIN.to_string(), OTHER_ORIGIN.to_string()]
    }

    fn profile(origin: &str) -> Profile {
        let host = origin.trim_start_matches("https://");
        Profile::resolve(Some(origin), Some(host), &allowed()).expect("valid profile")
    }

    fn authenticator() -> WebauthnAuthenticator<SoftPasskey> {
        WebauthnAuthenticator::new(SoftPasskey::new(true))
    }

    fn ip() -> IpAddr {
        "203.0.113.7".parse().unwrap()
    }

    fn url(origin: &str) -> Url {
        Url::parse(origin).unwrap()
    }

    async fn enrol(
        mgr: &PasskeyManager,
        auth: &mut WebauthnAuthenticator<SoftPasskey>,
        origin: &str,
        session: &str,
    ) -> PasskeyInfo {
        let p = profile(origin);
        let (tx, options) = mgr.begin_register(&p, session, "Phone").await.unwrap();
        let options: CreationChallengeResponse = serde_json::from_value(options).unwrap();
        let cred = auth.do_registration(url(origin), options).unwrap();
        mgr.finish_register(&tx, &p, session, &cred).await.unwrap()
    }

    /// Run one assertion with `signing_origin` baked into the client data.
    async fn assert_at(
        mgr: &PasskeyManager,
        auth: &mut WebauthnAuthenticator<SoftPasskey>,
        origin: &str,
        signing_origin: &str,
    ) -> (String, Result<(), PasskeyError>) {
        let p = profile(origin);
        let (tx, options) = mgr.begin_login(&p, ip()).await.unwrap();
        let options: RequestChallengeResponse = serde_json::from_value(options).unwrap();
        let cred = auth
            .do_authentication(url(signing_origin), options)
            .unwrap();
        let res = mgr.finish_login(&tx, &p, &cred).await;
        (tx, res)
    }

    async fn stored_counter(mgr: &PasskeyManager) -> u64 {
        let store = mgr.store.lock().await;
        serde_json::to_value(&store.credentials[0].passkey)
            .unwrap()
            .pointer("/cred/counter")
            .and_then(|v| v.as_u64())
            .expect("counter in serialized passkey")
    }

    #[test]
    fn profile_resolution_accepts_only_trusted_https_or_localhost_origins() {
        let allowed = vec![
            ORIGIN.to_string(),
            "https://aoe.example.com:8443".to_string(),
            "http://localhost:8080".to_string(),
            "http://aoe.example.com".to_string(),
            "https://10.0.0.5".to_string(),
        ];
        let cases: &[(&str, &str, bool)] = &[
            (ORIGIN, "aoe.example.com", true),
            ("https://aoe.example.com:8443", "aoe.example.com:8443", true),
            ("http://localhost:8080", "localhost:8080", true),
            ("http://aoe.example.com", "aoe.example.com", false),
            ("https://10.0.0.5", "10.0.0.5", false),
            ("https://evil.example.com", "evil.example.com", false),
            (ORIGIN, "evil.example.com", false),
            ("https://aoe.example.com/path", "aoe.example.com", false),
            ("https://aoe.example.com/?q=1", "aoe.example.com", false),
            ("https://user@aoe.example.com", "aoe.example.com", false),
            ("null", "aoe.example.com", false),
        ];
        for (origin, host, ok) in cases {
            assert_eq!(
                Profile::resolve(Some(origin), Some(host), &allowed).is_some(),
                *ok,
                "{origin} via {host}"
            );
        }
        assert!(Profile::resolve(None, Some("aoe.example.com"), &allowed).is_none());
        assert!(Profile::resolve(Some(ORIGIN), None, &allowed).is_none());
    }

    #[tokio::test]
    async fn enrol_and_sign_in_persists_owner_only_and_advances_counter() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = PasskeyManager::with_persistence(dir.path());
        let mut auth = authenticator();
        let info = enrol(&mgr, &mut auth, ORIGIN, "s1").await;
        assert_eq!(info.label, "Phone");

        let (_, res) = assert_at(&mgr, &mut auth, ORIGIN, ORIGIN).await;
        res.expect("assertion accepted");
        assert_eq!(stored_counter(&mgr).await, 1);
        assert!(mgr.list().await[0].last_used_at_ms.is_some());

        let path = dir.path().join(STORE_FILE);
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let reloaded = PasskeyManager::with_persistence(dir.path());
        assert!(reloaded.available_for(&profile(ORIGIN)).await);
        assert_eq!(
            stored_counter(&reloaded).await,
            1,
            "counter survives restart"
        );
    }

    #[tokio::test]
    async fn assertion_signed_for_another_origin_is_rejected_and_consumes_the_ceremony() {
        let mgr = PasskeyManager::in_memory();
        let mut auth = authenticator();
        enrol(&mgr, &mut auth, ORIGIN, "s1").await;

        let (tx, res) = assert_at(&mgr, &mut auth, ORIGIN, SUBDOMAIN_ORIGIN).await;
        assert!(matches!(res, Err(PasskeyError::Verification(_))));
        assert_eq!(
            stored_counter(&mgr).await,
            0,
            "failed assertion leaves the counter"
        );

        // The failed attempt burned the transaction: nothing can finish it afterwards.
        let p = profile(ORIGIN);
        let (tx2, options) = mgr.begin_login(&p, ip()).await.unwrap();
        let options: RequestChallengeResponse = serde_json::from_value(options).unwrap();
        let good = auth.do_authentication(url(ORIGIN), options).unwrap();
        assert!(matches!(
            mgr.finish_login(&tx, &p, &good).await,
            Err(PasskeyError::InvalidCeremony)
        ));
        mgr.finish_login(&tx2, &p, &good)
            .await
            .expect("fresh ceremony still works");
    }

    #[tokio::test]
    async fn credential_is_only_offered_to_the_origin_it_was_enrolled_for() {
        let mgr = PasskeyManager::in_memory();
        let mut auth = authenticator();
        enrol(&mgr, &mut auth, ORIGIN, "s1").await;

        assert!(!mgr.available_for(&profile(OTHER_ORIGIN)).await);
        assert!(matches!(
            mgr.begin_login(&profile(OTHER_ORIGIN), ip()).await,
            Err(PasskeyError::Unavailable)
        ));

        // A ceremony started for one origin cannot be finished from another.
        let (tx, options) = mgr.begin_login(&profile(ORIGIN), ip()).await.unwrap();
        let options: RequestChallengeResponse = serde_json::from_value(options).unwrap();
        let cred = auth.do_authentication(url(ORIGIN), options).unwrap();
        assert!(matches!(
            mgr.finish_login(&tx, &profile(OTHER_ORIGIN), &cred).await,
            Err(PasskeyError::InvalidCeremony)
        ));
    }

    #[tokio::test]
    async fn replayed_assertion_cannot_finish_a_second_time_or_a_new_challenge() {
        let mgr = PasskeyManager::in_memory();
        let mut auth = authenticator();
        enrol(&mgr, &mut auth, ORIGIN, "s1").await;
        let p = profile(ORIGIN);

        let (tx, options) = mgr.begin_login(&p, ip()).await.unwrap();
        let options: RequestChallengeResponse = serde_json::from_value(options).unwrap();
        let cred = auth.do_authentication(url(ORIGIN), options).unwrap();
        mgr.finish_login(&tx, &p, &cred).await.unwrap();

        assert!(matches!(
            mgr.finish_login(&tx, &p, &cred).await,
            Err(PasskeyError::InvalidCeremony)
        ));
        let (tx2, _) = mgr.begin_login(&p, ip()).await.unwrap();
        assert!(matches!(
            mgr.finish_login(&tx2, &p, &cred).await,
            Err(PasskeyError::Verification(_))
        ));
    }

    #[tokio::test]
    async fn counter_regression_is_rejected_as_a_possible_clone() {
        let mgr = PasskeyManager::in_memory();
        let mut auth = authenticator();
        enrol(&mgr, &mut auth, ORIGIN, "s1").await;
        {
            let mut store = mgr.store.lock().await;
            let mut json = serde_json::to_value(&store.credentials[0].passkey).unwrap();
            *json.pointer_mut("/cred/counter").unwrap() = 100.into();
            store.credentials[0].passkey = serde_json::from_value(json).unwrap();
        }
        let (_, res) = assert_at(&mgr, &mut auth, ORIGIN, ORIGIN).await;
        assert!(matches!(res, Err(PasskeyError::Verification(_))));
        assert_eq!(
            stored_counter(&mgr).await,
            100,
            "stored counter is not lowered"
        );
    }

    #[tokio::test]
    async fn successful_login_drops_stale_ceremonies() {
        let mgr = PasskeyManager::in_memory();
        let mut auth = authenticator();
        enrol(&mgr, &mut auth, ORIGIN, "s1").await;
        let p = profile(ORIGIN);
        let (stale_tx, _) = mgr.begin_login(&p, ip()).await.unwrap();
        assert_at(&mgr, &mut auth, ORIGIN, ORIGIN).await.1.unwrap();
        assert!(!lock(&mgr.authentications).contains_key(&stale_tx));
    }

    #[tokio::test]
    async fn expired_ceremony_and_foreign_session_cannot_finish_registration() {
        let mgr = PasskeyManager::in_memory();
        let mut auth = authenticator();
        let p = profile(ORIGIN);

        let (tx, options) = mgr.begin_register(&p, "s1", "").await.unwrap();
        let options: CreationChallengeResponse = serde_json::from_value(options).unwrap();
        let cred = auth.do_registration(url(ORIGIN), options).unwrap();
        assert!(matches!(
            mgr.finish_register(&tx, &p, "other-session", &cred).await,
            Err(PasskeyError::InvalidCeremony)
        ));

        let (tx, options) = mgr.begin_register(&p, "s1", "").await.unwrap();
        let options: CreationChallengeResponse = serde_json::from_value(options).unwrap();
        let cred = auth.do_registration(url(ORIGIN), options).unwrap();
        lock(&mgr.registrations).get_mut(&tx).unwrap().expires_at = Instant::now();
        assert!(matches!(
            mgr.finish_register(&tx, &p, "s1", &cred).await,
            Err(PasskeyError::InvalidCeremony)
        ));
        assert!(mgr.list().await.is_empty());
    }

    #[tokio::test]
    async fn login_ceremonies_are_capped_per_ip_without_evicting_registrations() {
        let mgr = PasskeyManager::in_memory();
        let mut auth = authenticator();
        enrol(&mgr, &mut auth, ORIGIN, "s1").await;
        let p = profile(ORIGIN);
        mgr.begin_register(&p, "s2", "").await.unwrap();

        for _ in 0..MAX_AUTHENTICATIONS_PER_IP {
            mgr.begin_login(&p, ip()).await.unwrap();
        }
        assert!(matches!(
            mgr.begin_login(&p, ip()).await,
            Err(PasskeyError::TooManyCeremonies)
        ));
        let other: IpAddr = "203.0.113.8".parse().unwrap();
        mgr.begin_login(&p, other)
            .await
            .expect("other IP unaffected");
        assert_eq!(lock(&mgr.registrations).len(), 1);
    }

    #[tokio::test]
    async fn remove_revokes_the_credential() {
        let mgr = PasskeyManager::in_memory();
        let mut auth = authenticator();
        let info = enrol(&mgr, &mut auth, ORIGIN, "s1").await;
        assert!(mgr.remove(&info.credential_id).await.unwrap());
        assert!(!mgr.remove(&info.credential_id).await.unwrap());
        assert!(!mgr.available_for(&profile(ORIGIN)).await);
    }

    #[tokio::test]
    async fn unreadable_or_insecure_store_disables_passkeys_and_is_never_overwritten() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STORE_FILE);

        for (content, mode) in [("{ not json", 0o600), ("{}", 0o644)] {
            std::fs::write(&path, content).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            let mgr = PasskeyManager::with_persistence(dir.path());
            let p = profile(ORIGIN);
            assert!(!mgr.available_for(&p).await);
            assert!(matches!(
                mgr.begin_register(&p, "s1", "").await,
                Err(PasskeyError::LimitReached)
            ));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
        }

        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(dir.path().join("target"), &path).unwrap();
        let mgr = PasskeyManager::with_persistence(dir.path());
        assert!(mgr.load_failed, "a symlinked store is refused");
    }

    fn state_with_login(passphrase: Option<&str>) -> Arc<AppState> {
        let mut state = crate::server::test_support::build_test_app_state(Vec::new());
        Arc::get_mut(&mut state).unwrap().login_manager =
            Arc::new(crate::server::login::LoginManager::new(passphrase));
        state
    }

    #[tokio::test]
    async fn handlers_require_login_to_be_enabled_and_a_session_to_enrol() {
        let addr = ConnectInfo("203.0.113.7:4000".parse::<std::net::SocketAddr>().unwrap());
        let mut headers = HeaderMap::new();
        headers.insert("origin", ORIGIN.parse().unwrap());
        headers.insert("host", "aoe.example.com".parse().unwrap());

        let off = state_with_login(None);
        let res = login_start_handler(State(off.clone()), addr, headers.clone()).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        let on = state_with_login(Some("pw"));
        let res = register_start_handler(State(on.clone()), None, headers.clone(), None).await;
        assert_eq!(
            res.status(),
            StatusCode::UNAUTHORIZED,
            "no session, no enrolment"
        );

        // Login is on but no origin is allowlisted, so even a session cannot enrol.
        let session = Some(Extension(AuthenticatedSession("s1".into())));
        let res = register_start_handler(State(on.clone()), session, headers.clone(), None).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // Allowlisted origin, but nothing enrolled: login start has nothing to offer.
        let mut with_origin = state_with_login(Some("pw"));
        Arc::get_mut(&mut with_origin).unwrap().allowed_origins = allowed();
        let res = login_start_handler(State(with_origin.clone()), addr, headers.clone()).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND);

        // Two Origin headers are ambiguous and refused.
        headers.append("origin", ORIGIN.parse().unwrap());
        let res = login_start_handler(State(with_origin), addr, headers).await;
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }

    /// On a direct loopback bind the auth wall is bypassed and sets no session extension,
    /// so enrolment must validate the cookie and binding itself.
    #[tokio::test]
    async fn loopback_enrolment_validates_the_bound_session_through_the_router() {
        use tower::ServiceExt;

        let mut state = crate::server::test_support::build_test_app_state_with_policy(
            Vec::new(),
            vec!["localhost".to_string()],
            vec!["http://localhost".to_string()],
            None,
        );
        Arc::get_mut(&mut state).unwrap().login_manager =
            Arc::new(crate::server::login::LoginManager::new(Some("pw")));
        let binding = [9u8; 32];
        let session = state
            .login_manager
            .create_session(&binding, "127.0.0.1", "t")
            .await;
        let binding = URL_SAFE_NO_PAD.encode(binding);
        let app = crate::server::test_support::build_router_for_test(state);

        let send = |cookie: Option<&str>, bound: bool| {
            let mut req = axum::http::Request::builder()
                .method("POST")
                .uri("/api/passkeys/register/start")
                .header("host", "localhost")
                .header("origin", "http://localhost")
                .header("content-type", "application/json");
            if let Some(cookie) = cookie {
                req = req.header("cookie", cookie);
            }
            if bound {
                req = req.header("x-aoe-device-binding", &binding);
            }
            let mut req = req
                .body(axum::body::Body::from(r#"{"label":"Laptop"}"#))
                .unwrap();
            req.extensions_mut()
                .insert(ConnectInfo(std::net::SocketAddr::from((
                    [127, 0, 0, 1],
                    5555,
                ))));
            app.clone().oneshot(req)
        };
        let cookie = format!("aoe_session={session}");

        let ok = send(Some(&cookie), true).await.unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        for (cookie, bound) in [
            (Some(cookie.as_str()), false),
            (None, true),
            (Some("aoe_session=forged"), true),
        ] {
            let res = send(cookie, bound).await.unwrap();
            assert_eq!(
                res.status(),
                StatusCode::UNAUTHORIZED,
                "{cookie:?} bound={bound}"
            );
        }
    }

    #[test]
    fn labels_are_trimmed_bounded_and_stripped_of_control_characters() {
        assert_eq!(sanitize_label("  "), "Passkey");
        assert_eq!(sanitize_label("My\nPhone\t"), "MyPhone");
        assert_eq!(
            sanitize_label(&"x".repeat(200)).chars().count(),
            MAX_LABEL_CHARS
        );
    }
}
