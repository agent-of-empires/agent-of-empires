//! Passphrase-based login as a second authentication factor.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::{FromRequest, State};
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use super::auth::resolve_client_ip;
use super::AppState;
use crate::util::{now_ms, system_time_to_ms};

/// Session lifetime (sliding window).
pub(crate) const SESSION_LIFETIME: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Step-up elevation window.
const ELEVATION_LIFETIME: Duration = Duration::from_secs(15 * 60);

/// Maximum concurrent login sessions before evicting the oldest.
const MAX_SESSIONS: usize = 50;

/// Minimum recommended passphrase length.
const MIN_PASSPHRASE_LENGTH: usize = 8;

/// Length in raw bytes of the client-generated device binding secret.
const BINDING_SECRET_BYTES: usize = 32;

/// Filename for the persisted login-session store under the app dir.
const SESSIONS_FILE: &str = "login_sessions.toml";

/// Schema version stamped into the persisted store.
const SESSIONS_SCHEMA_VERSION: u32 = 1;

/// The sliding window refreshes `expires_at` on every authenticated request, but rewriting
/// the store on every request is unacceptable write amplification.
const REFRESH_PERSIST_THRESHOLD: Duration = Duration::from_secs(24 * 60 * 60);

struct LoginSession {
    expires_at: Instant,
    /// SHA-256 hash of the client-presented device binding secret.
    binding_hash: [u8; 32],
    /// Step-up elevation deadline.
    elevated_until: Option<Instant>,
    /// Per-session failed elevation attempts since the last reset.
    elevation_failures: u32,
    /// Lockout deadline that gates further `/api/login/elevate` attempts for this session.
    elevation_locked_until: Option<Instant>,
    /// Wall-clock creation time, for the connected-devices view.
    created_at: SystemTime,
    /// Client IP at creation, display-only telemetry for the devices view.
    created_ip: String,
    /// User-agent string captured at login, for a friendly device
    /// label in the devices view. Display-only.
    user_agent: String,
    /// Deadline value at the last time this session was persisted to disk.
    last_persisted_expires_at: Instant,
}

/// Threshold for the per-session elevation rate limiter.
const MAX_ELEVATION_FAILURES: u32 = 3;
const ELEVATION_LOCKOUT: Duration = Duration::from_secs(15 * 60);

/// Manages passphrase verification and login session lifecycle.
pub struct LoginManager {
    passphrase_hash: Option<String>,
    sessions: RwLock<HashMap<String, LoginSession>>,
    /// Path to the on-disk session store, when persistence is enabled and an app dir is
    /// available.
    sessions_path: Option<PathBuf>,
}

/// Argon2 hash of a passphrase with a fresh random salt.
fn hash_passphrase(passphrase: &str) -> String {
    use argon2::{Argon2, PasswordHasher};

    Argon2::default()
        .hash_password(passphrase.as_bytes())
        .expect("argon2 hashing must not fail")
        .to_string()
}

/// Verify a passphrase against a stored argon2 PHC hash string.
fn argon2_verify(passphrase: &str, hash: &str) -> bool {
    use argon2::{Argon2, PasswordVerifier};

    Argon2::default()
        .verify_password(passphrase.as_bytes(), hash)
        .is_ok()
}

impl LoginManager {
    /// Create a new login manager without persistence.
    pub fn new(passphrase: Option<&str>) -> Self {
        Self {
            passphrase_hash: passphrase.map(hash_passphrase),
            sessions: RwLock::new(HashMap::new()),
            sessions_path: None,
        }
    }

    /// Create a login manager that persists sessions under `app_dir`, rehydrating any
    /// previously stored sessions whose passphrase still matches and whose sliding window
    /// has not lapsed.
    pub fn with_persistence(passphrase: Option<&str>, app_dir: &Path) -> Self {
        let passphrase_hash = passphrase.map(hash_passphrase);
        let sessions_path = app_dir.join(SESSIONS_FILE);

        let sessions = match load_sessions(&sessions_path, passphrase) {
            Ok(map) => map,
            Err(e) => {
                tracing::warn!(
                    target: "auth.passphrase",
                    error = %e,
                    "could not load persisted login sessions; starting empty"
                );
                HashMap::new()
            }
        };

        // Rewrite the store once at startup so the passphrase hash is refreshed (rotates
        // the salt) and any dropped/expired entries are pruned on disk.
        let snapshot = build_persisted(&passphrase_hash, &sessions);
        write_sessions(&sessions_path, &snapshot);

        Self {
            passphrase_hash,
            sessions: RwLock::new(sessions),
            sessions_path: Some(sessions_path),
        }
    }

    /// Whether passphrase login is enabled.
    pub fn is_enabled(&self) -> bool {
        self.passphrase_hash.is_some()
    }

    /// Verify a passphrase against the stored hash.
    pub fn verify_passphrase(&self, input: &str) -> bool {
        match self.passphrase_hash {
            Some(ref hash) => argon2_verify(input, hash),
            None => false,
        }
    }

    /// Create a new login session bound to a device.
    pub async fn create_session(
        &self,
        binding_secret_bytes: &[u8],
        created_ip: &str,
        user_agent: &str,
    ) -> String {
        let session_id = super::generate_token();
        let now = Instant::now();
        let session = LoginSession {
            expires_at: now + SESSION_LIFETIME,
            binding_hash: hash_binding_secret(binding_secret_bytes),
            elevated_until: None,
            elevation_failures: 0,
            elevation_locked_until: None,
            created_at: SystemTime::now(),
            created_ip: created_ip.to_string(),
            user_agent: user_agent.to_string(),
            last_persisted_expires_at: now + SESSION_LIFETIME,
        };

        {
            let mut sessions = self.sessions.write().await;

            // Evict oldest if at capacity
            if sessions.len() >= MAX_SESSIONS {
                if let Some(oldest_id) = sessions
                    .iter()
                    .min_by_key(|(_, s)| s.expires_at)
                    .map(|(id, _)| id.clone())
                {
                    sessions.remove(&oldest_id);
                }
            }

            sessions.insert(session_id.clone(), session);
        }

        self.persist().await;
        session_id
    }

    /// Validate a session.
    pub async fn validate_session(&self, session_id: &str, presented_binding: &[u8]) -> bool {
        if session_id.is_empty() || presented_binding.len() != BINDING_SECRET_BYTES {
            return false;
        }

        let presented_hash = hash_binding_secret(presented_binding);

        // `needs_persist` is set under the lock, acted on after release.
        let mut needs_persist = false;
        // `Some(deadline)` when this validation refreshed the sliding window far enough to
        // re-persist.
        let mut refreshed_deadline: Option<Instant> = None;
        let valid = {
            let mut sessions = self.sessions.write().await;
            let Some(session) = sessions.get_mut(session_id) else {
                return false;
            };

            if Instant::now() > session.expires_at {
                sessions.remove(session_id);
                needs_persist = true;
                false
            } else if session.binding_hash.ct_eq(&presented_hash).unwrap_u8() == 0 {
                // Constant-time compare.
                false
            } else {
                // Sliding window: extend expiry on each valid access.
                let new_expiry = Instant::now() + SESSION_LIFETIME;
                session.expires_at = new_expiry;
                if new_expiry.saturating_duration_since(session.last_persisted_expires_at)
                    > REFRESH_PERSIST_THRESHOLD
                {
                    needs_persist = true;
                    refreshed_deadline = Some(new_expiry);
                }
                true
            }
        };

        if needs_persist && self.persist().await {
            if let Some(deadline) = refreshed_deadline {
                if let Some(session) = self.sessions.write().await.get_mut(session_id) {
                    session.last_persisted_expires_at = deadline;
                }
            }
        }
        valid
    }

    /// Mark a session as elevated (passphrase confirmed) for `ELEVATION_LIFETIME`.
    pub async fn elevate_session(&self, session_id: &str) -> bool {
        if session_id.is_empty() {
            return false;
        }
        {
            let mut sessions = self.sessions.write().await;
            let Some(session) = sessions.get_mut(session_id) else {
                return false;
            };
            if Instant::now() > session.expires_at {
                return false;
            }
            session.elevated_until = Some(Instant::now() + ELEVATION_LIFETIME);
            session.elevation_failures = 0;
            session.elevation_locked_until = None;
        }
        // Persist the cleared lockout counters; `elevated_until` itself
        // is intentionally never written. See #1235.
        self.persist().await;
        true
    }

    /// Whether the session's elevation endpoint is locked out.
    pub async fn elevation_lockout_remaining(&self, session_id: &str) -> Option<u64> {
        if session_id.is_empty() {
            return None;
        }
        let sessions = self.sessions.read().await;
        let session = sessions.get(session_id)?;
        let locked_until = session.elevation_locked_until?;
        let now = Instant::now();
        if now >= locked_until {
            return None;
        }
        Some(locked_until.saturating_duration_since(now).as_secs().max(1))
    }

    /// Record a failed passphrase entry on `/api/login/elevate` for this session.
    pub async fn record_elevation_failure(&self, session_id: &str) -> bool {
        if session_id.is_empty() {
            return false;
        }
        let mut armed = false;
        let changed = {
            let mut sessions = self.sessions.write().await;
            let Some(session) = sessions.get_mut(session_id) else {
                return false;
            };
            let now = Instant::now();
            // Existing lockout still in force: ignore the attempt.
            if let Some(deadline) = session.elevation_locked_until {
                if now < deadline {
                    return false;
                }
                session.elevation_failures = 0;
                session.elevation_locked_until = None;
            }
            session.elevation_failures = session.elevation_failures.saturating_add(1);
            if session.elevation_failures >= MAX_ELEVATION_FAILURES {
                session.elevation_locked_until = Some(now + ELEVATION_LOCKOUT);
                tracing::warn!(
                    target: "auth.passphrase",
                    failures = session.elevation_failures,
                    lockout_secs = ELEVATION_LOCKOUT.as_secs(),
                    "session elevation lockout armed after threshold"
                );
                armed = true;
            }
            true
        };
        // Persist the updated lockout counters so a restart cannot reset
        // an attacker's failure budget. See #1235.
        if changed {
            self.persist().await;
        }
        armed
    }

    /// Read elevation state.
    pub async fn elevation_state(&self, session_id: &str) -> (bool, Option<u64>) {
        if session_id.is_empty() {
            return (false, None);
        }
        let sessions = self.sessions.read().await;
        let Some(session) = sessions.get(session_id) else {
            return (false, None);
        };
        let now = Instant::now();
        if now > session.expires_at {
            return (false, None);
        }
        let Some(deadline) = session.elevated_until else {
            return (false, None);
        };
        if now > deadline {
            return (false, None);
        }
        let remaining = deadline.saturating_duration_since(now).as_secs();
        (true, Some(remaining))
    }

    /// Whether the session is currently elevated.
    pub async fn is_elevated(&self, session_id: &str) -> bool {
        self.elevation_state(session_id).await.0
    }

    /// Invalidate a session (logout).
    pub async fn invalidate_session(&self, session_id: &str) {
        let removed = self.sessions.write().await.remove(session_id).is_some();
        if removed {
            self.persist().await;
        }
    }

    /// Revoke a single session by id from the connected-devices view.
    pub async fn revoke_session(&self, session_id: &str) -> bool {
        let removed = self.sessions.write().await.remove(session_id).is_some();
        if removed {
            self.persist().await;
        }
        removed
    }

    /// Sign out every device.
    pub async fn logout_all(&self) -> usize {
        let count = {
            let mut sessions = self.sessions.write().await;
            let n = sessions.len();
            sessions.clear();
            n
        };
        self.persist().await;
        count
    }

    /// Snapshot of the current sessions for the connected-devices view.
    pub async fn device_snapshot(&self, current_session_id: Option<&str>) -> Vec<DeviceSession> {
        let sessions = self.sessions.read().await;
        let now_inst = Instant::now();
        let now_sys = SystemTime::now();
        let mut out: Vec<DeviceSession> = sessions
            .iter()
            .map(|(id, s)| {
                // last_seen = now - (time remaining until the deadline minus the full
                // lifetime).
                let remaining = s.expires_at.saturating_duration_since(now_inst);
                let since_last_seen = SESSION_LIFETIME.saturating_sub(remaining);
                let last_seen = now_sys.checked_sub(since_last_seen).unwrap_or(now_sys);
                DeviceSession {
                    session_id: id.clone(),
                    user_agent: s.user_agent.clone(),
                    created_ip: s.created_ip.clone(),
                    created_at: chrono::DateTime::<chrono::Utc>::from(s.created_at),
                    last_seen: chrono::DateTime::<chrono::Utc>::from(last_seen),
                    current: current_session_id == Some(id.as_str()),
                }
            })
            .collect();
        // Newest sign-in first for a stable, predictable ordering.
        out.sort_by_key(|d| std::cmp::Reverse(d.created_at));
        out
    }

    /// Remove expired sessions. Called periodically.
    pub async fn cleanup_expired(&self) {
        let before;
        let after;
        {
            let mut sessions = self.sessions.write().await;
            before = sessions.len();
            let now = Instant::now();
            sessions.retain(|_, s| now < s.expires_at);
            after = sessions.len();
        }
        if after != before {
            self.persist().await;
        }
    }

    /// Persist the current sessions to disk when persistence is enabled.
    async fn persist(&self) -> bool {
        let Some(path) = self.sessions_path.clone() else {
            return true;
        };
        let snapshot = {
            let sessions = self.sessions.read().await;
            build_persisted(&self.passphrase_hash, &sessions)
        };
        tokio::task::spawn_blocking(move || write_sessions(&path, &snapshot))
            .await
            .unwrap_or(false)
    }

    /// Spawn periodic cleanup (piggybacks on the rate limiter's interval).
    /// Exits cleanly on shutdown so `aoe serve --stop` drains within one
    /// tick instead of waiting for the 5 s force exit safety net.
    pub fn spawn_cleanup_task(self: &Arc<Self>, shutdown: CancellationToken) {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = interval.tick() => manager.cleanup_expired().await,
                    _ = shutdown.cancelled() => break,
                }
            }
        });
    }
}

/// Hash a device binding secret with SHA-256.
fn hash_binding_secret(secret: &[u8]) -> [u8; 32] {
    Sha256::digest(secret).into()
}

// ── Persistence ──────────────────────────────────────────────────────────────

/// A login session as surfaced to the connected-devices view.
#[derive(Clone, Serialize)]
pub struct DeviceSession {
    pub session_id: String,
    pub user_agent: String,
    pub created_ip: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_seen: chrono::DateTime<chrono::Utc>,
    /// True for the session making the request, so the UI can label
    /// "this device" and guard self-revocation.
    pub current: bool,
}

/// On-disk shape of `login_sessions.toml`. `passphrase_hash` is the argon2 PHC string of
/// the passphrase in force when the file was last written; on load it gates whether the
/// sessions are rehydrated (same passphrase) or dropped (changed).
#[derive(Serialize, Deserialize)]
struct PersistedFile {
    schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    passphrase_hash: Option<String>,
    #[serde(default)]
    sessions: Vec<PersistedSession>,
}

#[derive(Serialize, Deserialize)]
struct PersistedSession {
    id: String,
    /// base64url (no pad) of the 32-byte SHA-256 binding hash.
    binding_hash: String,
    /// Sliding-window deadline as Unix epoch milliseconds (wall clock,
    /// since `Instant` is process-local and cannot be persisted).
    expires_at_ms: u64,
    created_at_ms: u64,
    #[serde(default)]
    created_ip: String,
    #[serde(default)]
    user_agent: String,
    #[serde(default)]
    elevation_failures: u32,
    /// Lockout deadline as Unix epoch milliseconds; 0 means no lockout.
    #[serde(default)]
    elevation_locked_until_ms: u64,
}

/// Convert an in-memory `Instant` deadline to a wall-clock epoch-ms value for persistence.
fn instant_deadline_to_ms(deadline: Instant, now_inst: Instant, now_ms_val: u64) -> u64 {
    let remaining = deadline.saturating_duration_since(now_inst);
    if remaining.is_zero() {
        0
    } else {
        now_ms_val.saturating_add(remaining.as_millis() as u64)
    }
}

/// Build the on-disk representation from the live session map.
fn build_persisted(
    passphrase_hash: &Option<String>,
    sessions: &HashMap<String, LoginSession>,
) -> PersistedFile {
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;

    let now_inst = Instant::now();
    let now = now_ms();
    let persisted = sessions
        .iter()
        .map(|(id, s)| PersistedSession {
            id: id.clone(),
            binding_hash: URL_SAFE_NO_PAD.encode(s.binding_hash),
            expires_at_ms: instant_deadline_to_ms(s.expires_at, now_inst, now),
            created_at_ms: system_time_to_ms(s.created_at),
            created_ip: s.created_ip.clone(),
            user_agent: s.user_agent.clone(),
            elevation_failures: s.elevation_failures,
            elevation_locked_until_ms: s
                .elevation_locked_until
                .map(|d| instant_deadline_to_ms(d, now_inst, now))
                .unwrap_or(0),
        })
        .collect();

    PersistedFile {
        schema_version: SESSIONS_SCHEMA_VERSION,
        passphrase_hash: passphrase_hash.clone(),
        sessions: persisted,
    }
}

/// Atomically write the session store, owner-only (0600).
fn write_sessions(path: &Path, file: &PersistedFile) -> bool {
    // The no-symlink, owner-only invariant belongs on the write path too, not just at load.
    if let Err(e) = check_path_security(path) {
        tracing::warn!(
            target: "auth.passphrase",
            error = %e,
            "refusing to write persisted login sessions to an insecure path"
        );
        return false;
    }
    let toml = match toml::to_string(file) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(target: "auth.passphrase", error = %e, "serialize login sessions");
            return false;
        }
    };
    if let Err(e) = crate::session::atomic_write(path, toml.as_bytes()) {
        tracing::warn!(target: "auth.passphrase", error = %e, "write login sessions");
        return false;
    }
    // `atomic_write` lands the file via a `NamedTempFile`, which is 0600
    // on first create; re-assert it defensively so the secret can never
    // widen even if an earlier file had looser perms.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    true
}

/// Fail-closed check that the sessions store is safe to read from or write to.
fn check_path_security(path: &Path) -> anyhow::Result<()> {
    use anyhow::{bail, Context};

    if let Some(parent) = path.parent() {
        let pmeta = std::fs::symlink_metadata(parent).context("stat login sessions parent dir")?;
        if pmeta.file_type().is_symlink() {
            bail!("login sessions parent dir is a symlink; refusing");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if pmeta.permissions().mode() & 0o022 != 0 {
                bail!("login sessions parent dir is group/world writable; refusing");
            }
        }
    }

    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                bail!("login sessions path is a symlink; refusing");
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if meta.permissions().mode() & 0o077 != 0 {
                    bail!("login sessions file is group/world accessible; refusing");
                }
            }
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).context("stat login sessions file"),
    }
}

/// Load and rehydrate persisted sessions.
fn load_sessions(
    path: &Path,
    passphrase: Option<&str>,
) -> anyhow::Result<HashMap<String, LoginSession>> {
    use anyhow::Context;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use base64::Engine;

    check_path_security(path)?;

    let raw = match std::fs::read_to_string(path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => return Err(e).context("read login sessions file"),
    };
    let file: PersistedFile = toml::from_str(&raw).context("parse login sessions file")?;

    if file.schema_version != SESSIONS_SCHEMA_VERSION {
        tracing::info!(
            target: "auth.passphrase",
            found = file.schema_version,
            expected = SESSIONS_SCHEMA_VERSION,
            "login sessions schema mismatch; dropping persisted sessions"
        );
        return Ok(HashMap::new());
    }

    // Without a configured passphrase there is no login gate, so any
    // persisted sessions are meaningless; start clean.
    let Some(passphrase) = passphrase else {
        return Ok(HashMap::new());
    };
    // Drop everything if the passphrase changed since the file was
    // written (or the file carries no hash to verify against).
    match file.passphrase_hash.as_deref() {
        Some(hash) if argon2_verify(passphrase, hash) => {}
        _ => {
            let n = file.sessions.len();
            if n > 0 {
                tracing::info!(
                    target: "auth.passphrase",
                    dropped = n,
                    "passphrase changed since last run; persisted sessions invalidated"
                );
            }
            return Ok(HashMap::new());
        }
    }

    let now_inst = Instant::now();
    let now = now_ms();
    let lifetime_ms = SESSION_LIFETIME.as_millis() as u64;

    let mut out = HashMap::new();
    for ps in file.sessions {
        // Drop already-expired entries; clamp future deadlines back to
        // the lifetime ceiling (clock skew or tampering).
        if ps.expires_at_ms <= now {
            continue;
        }
        let remaining_ms = (ps.expires_at_ms - now).min(lifetime_ms);
        let expires_at = now_inst + Duration::from_millis(remaining_ms);

        let Ok(binding_vec) = URL_SAFE_NO_PAD.decode(ps.binding_hash.as_bytes()) else {
            continue;
        };
        let Ok(binding_hash) = <[u8; 32]>::try_from(binding_vec.as_slice()) else {
            continue;
        };

        let created_at = UNIX_EPOCH
            .checked_add(Duration::from_millis(ps.created_at_ms))
            .unwrap_or_else(SystemTime::now);

        let elevation_locked_until = if ps.elevation_locked_until_ms > now {
            let rem =
                (ps.elevation_locked_until_ms - now).min(ELEVATION_LOCKOUT.as_millis() as u64);
            Some(now_inst + Duration::from_millis(rem))
        } else {
            None
        };

        out.insert(
            ps.id,
            LoginSession {
                expires_at,
                binding_hash,
                // Never persisted: a restart drops elevation.
                elevated_until: None,
                elevation_failures: ps.elevation_failures,
                elevation_locked_until,
                created_at,
                created_ip: ps.created_ip,
                user_agent: ps.user_agent,
                last_persisted_expires_at: expires_at,
            },
        );
    }
    Ok(out)
}

/// Decode a base64url-encoded device binding secret from the wire.
pub fn decode_binding_secret(s: &str) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
    use base64::Engine;
    let trimmed = s.trim();
    if trimmed.is_empty() {
        return None;
    }
    let decoded = URL_SAFE_NO_PAD
        .decode(trimmed)
        .or_else(|_| URL_SAFE.decode(trimmed))
        .ok()?;
    if decoded.len() == BINDING_SECRET_BYTES {
        Some(decoded)
    } else {
        None
    }
}

/// Check if passphrase meets minimum length. Returns a warning message if not.
pub fn check_passphrase_strength(passphrase: &str) -> Option<String> {
    if passphrase.len() < MIN_PASSPHRASE_LENGTH {
        Some(format!(
            "WARNING: Passphrase is only {} characters. \
             Consider using at least {} characters for better security.",
            passphrase.len(),
            MIN_PASSPHRASE_LENGTH
        ))
    } else {
        None
    }
}

// ── Handlers ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct LoginRequest {
    passphrase: String,
    /// Base64url encoding of 32 random bytes the client persists in `localStorage`.
    device_binding_secret: String,
}

/// POST /api/login
pub async fn login_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
    login_body: Result<Json<LoginRequest>, axum::extract::rejection::JsonRejection>,
) -> axum::response::Response {
    let client_ip = resolve_client_ip(addr, &headers);

    if !state.login_manager.is_enabled() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "not_found",
                "message": "Login is not enabled"
            })),
        )
            .into_response();
    }

    // Rate limit check
    if let Some(remaining) = state.rate_limiter.check_locked(client_ip).await {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("Retry-After", remaining.to_string())],
            Json(serde_json::json!({
                "error": "rate_limited",
                "message": format!("Too many failed attempts. Try again in {} seconds.", remaining)
            })),
        )
            .into_response();
    }

    let login_req = match login_body {
        Ok(Json(req)) => req,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "bad_request",
                    "message": "Missing or invalid passphrase / device_binding_secret"
                })),
            )
                .into_response();
        }
    };

    let Some(binding_bytes) = decode_binding_secret(&login_req.device_binding_secret) else {
        // Treat malformed bindings as a usage error (the client sent garbage), not a failed
        // login attempt.
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "bad_request",
                "message": format!(
                    "device_binding_secret must be base64url of {} random bytes",
                    BINDING_SECRET_BYTES
                )
            })),
        )
            .into_response();
    };

    tracing::debug!(target: "auth.passphrase",
        ip = %client_ip,
        passphrase_len = login_req.passphrase.len(),
        "Login attempt"
    );

    if state.login_manager.verify_passphrase(&login_req.passphrase) {
        state.rate_limiter.record_success(client_ip).await;

        // Captured for the persisted session's connected-devices label
        // and reused for the new-login push below. Display-only.
        let user_agent = headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown")
            .to_string();

        let session_id = state
            .login_manager
            .create_session(&binding_bytes, &client_ip.to_string(), &user_agent)
            .await;

        tracing::info!(target: "auth.passphrase", ip = %client_ip, "passphrase login successful");

        // Fire-and-forget push to every existing subscriber.
        let state_for_push = state.clone();
        tokio::spawn(async move {
            trigger_new_login_push(&state_for_push, &user_agent).await;
        });

        let cookie = build_login_cookie(&session_id, state.behind_tunnel);
        let mut response = Json(serde_json::json!({
            "ok": true
        }))
        .into_response();

        response.headers_mut().insert(
            header::SET_COOKIE,
            cookie.parse().expect("cookie format must be valid"),
        );

        response
    } else {
        let locked = state.rate_limiter.record_failure(client_ip).await;
        tracing::warn!(
            target: "auth.passphrase",
            ip = %client_ip,
            locked = locked,
            reason = "incorrect_passphrase",
            "passphrase login failed"
        );

        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "unauthorized",
                "message": "Incorrect passphrase"
            })),
        )
            .into_response()
    }
}

#[derive(Deserialize)]
pub struct ElevateRequest {
    passphrase: String,
}

/// POST /api/login/elevate
pub async fn elevate_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::ConnectInfo(addr): axum::extract::ConnectInfo<std::net::SocketAddr>,
    request: axum::extract::Request,
) -> axum::response::Response {
    let client_ip = resolve_client_ip(addr, request.headers());

    if !state.login_manager.is_enabled() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "not_found",
                "message": "Login is not enabled"
            })),
        )
            .into_response();
    }

    let Some(session_id) = extract_login_session(&request) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "login_required",
                "message": "No active login session"
            })),
        )
            .into_response();
    };

    // Two rate limiters guard this endpoint.
    if let Some(remaining) = state
        .login_manager
        .elevation_lockout_remaining(&session_id)
        .await
    {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("Retry-After", remaining.to_string())],
            Json(serde_json::json!({
                "error": "rate_limited",
                "message": format!("Too many failed attempts. Try again in {} seconds.", remaining)
            })),
        )
            .into_response();
    }

    if let Some(remaining) = state.rate_limiter.check_locked(client_ip).await {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("Retry-After", remaining.to_string())],
            Json(serde_json::json!({
                "error": "rate_limited",
                "message": format!("Too many failed attempts. Try again in {} seconds.", remaining)
            })),
        )
            .into_response();
    }

    let elevate_req: ElevateRequest =
        match axum::Json::<ElevateRequest>::from_request(request, &()).await {
            Ok(axum::Json(req)) => req,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "bad_request",
                        "message": "Missing or invalid passphrase field"
                    })),
                )
                    .into_response();
            }
        };

    if !state
        .login_manager
        .verify_passphrase(&elevate_req.passphrase)
    {
        let ip_locked = state.rate_limiter.record_failure(client_ip).await;
        let session_locked = state
            .login_manager
            .record_elevation_failure(&session_id)
            .await;
        tracing::warn!(
            target: "auth.passphrase",
            ip = %client_ip,
            ip_locked = ip_locked,
            session_locked = session_locked,
            reason = "incorrect_passphrase_on_elevate",
            "elevation failed"
        );
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "unauthorized",
                "message": "Incorrect passphrase"
            })),
        )
            .into_response();
    }

    state.rate_limiter.record_success(client_ip).await;
    let elevated = state.login_manager.elevate_session(&session_id).await;
    if !elevated {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({
                "error": "login_required",
                "message": "Login session expired"
            })),
        )
            .into_response();
    }

    let (_, remaining_secs) = state.login_manager.elevation_state(&session_id).await;
    tracing::info!(
        target: "auth.passphrase",
        ip = %client_ip,
        "session elevated"
    );

    Json(serde_json::json!({
        "ok": true,
        "elevated_until_secs": remaining_secs,
    }))
    .into_response()
}

/// POST /api/logout
pub async fn logout_handler(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> axum::response::Response {
    // Extract session cookie
    if let Some(session_id) = extract_login_session(&request) {
        state.login_manager.invalidate_session(&session_id).await;
    }

    let clear_cookie = clear_login_cookie(state.behind_tunnel);

    let mut response = Json(serde_json::json!({ "ok": true })).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        clear_cookie.parse().expect("cookie format must be valid"),
    );

    response
}

/// Build a `Set-Cookie` header that clears the login session cookie.
fn clear_login_cookie(secure: bool) -> String {
    format!(
        "aoe_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0{}",
        if secure { "; Secure" } else { "" }
    )
}

/// GET /api/devices
pub async fn devices_handler(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> Json<Vec<DeviceSession>> {
    let current = extract_login_session(&request);
    Json(
        state
            .login_manager
            .device_snapshot(current.as_deref())
            .await,
    )
}

/// POST /api/login/logout-all
pub async fn logout_all_handler(State(state): State<Arc<AppState>>) -> axum::response::Response {
    let count = state.login_manager.logout_all().await;
    tracing::info!(target: "auth.passphrase", count, "signed out all devices");

    let mut response = Json(serde_json::json!({ "ok": true, "count": count })).into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        clear_login_cookie(state.behind_tunnel)
            .parse()
            .expect("cookie format must be valid"),
    );
    response
}

/// DELETE /api/login/sessions/{id}
pub async fn revoke_session_handler(
    State(state): State<Arc<AppState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    request: axum::extract::Request,
) -> axum::response::Response {
    let is_self = extract_login_session(&request).as_deref() == Some(id.as_str());
    let revoked = state.login_manager.revoke_session(&id).await;
    tracing::info!(target: "auth.passphrase", revoked, is_self, "revoked device session");

    let mut response = Json(serde_json::json!({ "ok": true, "revoked": revoked })).into_response();
    if is_self && revoked {
        response.headers_mut().insert(
            header::SET_COOKIE,
            clear_login_cookie(state.behind_tunnel)
                .parse()
                .expect("cookie format must be valid"),
        );
    }
    response
}

/// GET /api/login/status
pub async fn login_status_handler(
    State(state): State<Arc<AppState>>,
    request: axum::extract::Request,
) -> Json<serde_json::Value> {
    let required = state.login_manager.is_enabled();

    if !required {
        return Json(serde_json::json!({
            "required": false,
            "authenticated": true,
            "elevated": true,
            "elevated_until_secs": null,
        }));
    }

    let session_id = extract_login_session(&request);
    let presented_binding = super::auth::extract_device_binding(&request);

    let (authenticated, session_id_for_elevation) = match (session_id, presented_binding) {
        (Some(sid), Some(secret)) => {
            let ok = state.login_manager.validate_session(&sid, &secret).await;
            (ok, if ok { Some(sid) } else { None })
        }
        _ => (false, None),
    };

    let (elevated, elevated_secs) = match session_id_for_elevation {
        Some(sid) => state.login_manager.elevation_state(&sid).await,
        None => (false, None),
    };

    Json(serde_json::json!({
        "required": required,
        "authenticated": authenticated,
        "elevated": elevated,
        "elevated_until_secs": elevated_secs,
    }))
}

/// Extract the `aoe_session` cookie value from a request.
pub fn extract_login_session(request: &axum::extract::Request) -> Option<String> {
    let cookie_header = request.headers().get(header::COOKIE)?;
    let cookie_str = cookie_header.to_str().ok()?;
    for cookie in cookie_str.split(';') {
        let cookie = cookie.trim();
        if let Some(value) = cookie.strip_prefix("aoe_session=") {
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Fire a fire-and-forget web push to every existing subscriber that a new dashboard login
/// just succeeded.
async fn trigger_new_login_push(state: &AppState, user_agent: &str) {
    let Some(push) = state.push.as_ref() else {
        return;
    };
    if !state.push_enabled {
        return;
    }
    let subs = push.store.snapshot().await;
    if subs.is_empty() {
        return;
    }
    let truncated_ua = user_agent.chars().take(80).collect::<String>();
    let title = "New aoe dashboard login".to_string();
    let body = format!("New device signed in. UA: {truncated_ua}");
    let client = match super::push_send::build_client() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(target: "auth.passphrase", "build_client: {e}");
            return;
        }
    };
    for sub in subs {
        let Some(url) = super::push::build_push_url(&sub, "/") else {
            continue;
        };
        let payload = super::push_send::PushPayload {
            title: title.clone(),
            body: body.clone(),
            url,
            tag: "aoe-new-login".to_string(),
            session_id: String::new(),
        };
        let body_bytes = match serde_json::to_vec(&payload) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(target: "auth.passphrase", "serialise payload: {e}");
                continue;
            }
        };
        let auth_header = match super::push_send::vapid_auth_header(push, &sub.endpoint) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(target: "auth.passphrase", "vapid header: {e}");
                continue;
            }
        };
        let cipher = match super::push_send::encrypt_aes128gcm(&sub, &body_bytes) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(target: "auth.passphrase", "encrypt: {e}");
                continue;
            }
        };
        let _ = client
            .post(&sub.endpoint)
            .header("Authorization", &auth_header)
            .header("Content-Encoding", "aes128gcm")
            .header("Content-Type", "application/octet-stream")
            .header("TTL", "60")
            .body(cipher)
            .send()
            .await;
    }
}

/// Build a Set-Cookie header for the login session.
pub fn build_login_cookie(session_id: &str, secure: bool) -> String {
    let mut cookie = format!(
        "aoe_session={}; HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000",
        session_id
    );
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(byte: u8) -> Vec<u8> {
        vec![byte; BINDING_SECRET_BYTES]
    }

    async fn session(mgr: &LoginManager, secret: &[u8]) -> String {
        mgr.create_session(secret, "127.0.0.1", "test-agent").await
    }

    #[test]
    fn passphrase_verification_is_all_or_nothing() {
        let mgr = LoginManager::new(Some("my_secret"));
        assert!(mgr.is_enabled());
        assert!(mgr.verify_passphrase("my_secret"));
        for wrong in ["wrong", "", "my_secre"] {
            assert!(!mgr.verify_passphrase(wrong), "{wrong:?}");
        }

        let disabled = LoginManager::new(None);
        assert!(!disabled.is_enabled());
        assert!(!disabled.verify_passphrase("anything"));

        // Persisted stores hold hashes produced by argon2 0.5.3.
        let hash = "$argon2id$v=19$m=19456,t=2,p=1$YW9lLWZpeGVkLXNhbHQxNg$DsLn90oHo6VdenuubImBcuPgEWcMMEPqYxc8jPxJZcY";
        assert!(argon2_verify("hunter2", hash));
        assert!(!argon2_verify("hunter3", hash));
    }

    /// A session is keyed by its binding secret, not its IP (#1131), and the secret must
    /// match in full: a prefix of the right bytes is still a different device.
    #[tokio::test]
    async fn validate_session_matches_on_the_whole_binding_secret() {
        let mgr = LoginManager::new(Some("test"));
        let secret = binding(0xAA);
        let id = session(&mgr, &secret).await;

        assert!(mgr.validate_session(&id, &secret).await);
        assert!(mgr.validate_session(&id, &secret).await, "IP is not bound");
        assert!(!mgr.validate_session(&id, &binding(0xBB)).await);
        assert!(
            !mgr.validate_session(&id, &[0xAA; BINDING_SECRET_BYTES - 1])
                .await,
            "a truncated secret must not match on its prefix"
        );
        for unknown in ["nonexistent", ""] {
            assert!(!mgr.validate_session(unknown, &secret).await, "{unknown:?}");
        }

        mgr.invalidate_session(&id).await;
        assert!(!mgr.validate_session(&id, &secret).await);
        mgr.invalidate_session("nonexistent").await;
    }

    #[tokio::test]
    async fn elevation_starts_false_and_expires() {
        let mgr = LoginManager::new(Some("test"));
        let secret = binding(0x22);
        let id = session(&mgr, &secret).await;

        assert!(!mgr.is_elevated(&id).await);
        assert!(mgr.elevate_session(&id).await);
        let (elevated, remaining) = mgr.elevation_state(&id).await;
        assert!(elevated && remaining.is_some());

        {
            let mut sessions = mgr.sessions.write().await;
            let entry = sessions.get_mut(&id).expect("session");
            entry.elevated_until = Some(Instant::now() - Duration::from_secs(1));
        }
        assert!(!mgr.is_elevated(&id).await);

        assert!(!mgr.elevate_session("nope").await);
        assert!(!mgr.is_elevated("nope").await);
    }

    /// #1131 follow-up: the failure budget arms a lockout once, a success resets it,
    /// and an unknown session never arms anything.
    #[tokio::test]
    async fn elevation_lockout_arms_once_per_failure_budget() {
        let mgr = LoginManager::new(Some("test"));
        let secret = binding(0x77);
        let id = session(&mgr, &secret).await;

        assert!(mgr.elevation_lockout_remaining(&id).await.is_none());
        for _ in 0..(MAX_ELEVATION_FAILURES - 1) {
            assert!(!mgr.record_elevation_failure(&id).await);
        }
        assert!(mgr.record_elevation_failure(&id).await, "threshold crossed");
        assert!(mgr.elevation_lockout_remaining(&id).await.is_some());
        assert!(
            !mgr.record_elevation_failure(&id).await,
            "failures while locked do not extend the window"
        );

        assert!(mgr.elevate_session(&id).await);
        for _ in 0..(MAX_ELEVATION_FAILURES - 1) {
            assert!(!mgr.record_elevation_failure(&id).await);
        }
        assert!(
            mgr.elevation_lockout_remaining(&id).await.is_none(),
            "a success starts a fresh budget"
        );

        assert!(!mgr.record_elevation_failure("nope").await);
        assert!(mgr.elevation_lockout_remaining("nope").await.is_none());
    }

    #[tokio::test]
    async fn max_sessions_evicts_oldest() {
        let mgr = LoginManager::new(Some("test"));
        let secret = binding(0x44);
        let mut first_id = String::new();
        for i in 0..MAX_SESSIONS {
            let id = session(&mgr, &secret).await;
            if i == 0 {
                first_id = id;
            }
        }
        assert!(mgr.validate_session(&first_id, &secret).await);
        session(&mgr, &secret).await;
        assert_eq!(mgr.sessions.read().await.len(), MAX_SESSIONS);
    }

    #[tokio::test]
    async fn cleanup_expired_removes_stale() {
        let mgr = LoginManager::new(Some("test"));
        let secret = binding(0x55);
        let id = session(&mgr, &secret).await;
        {
            let mut sessions = mgr.sessions.write().await;
            sessions.get_mut(&id).expect("session").expires_at =
                Instant::now() - Duration::from_secs(1);
        }
        mgr.cleanup_expired().await;
        assert!(!mgr.validate_session(&id, &secret).await);
    }

    #[tokio::test]
    async fn revoke_session_removes_one() {
        let mgr = LoginManager::new(Some("pass"));
        let secret = binding(0xF6);
        let id = session(&mgr, &secret).await;
        assert!(mgr.revoke_session(&id).await);
        assert!(!mgr.validate_session(&id, &secret).await);
        assert!(
            !mgr.revoke_session(&id).await,
            "a gone session reports false"
        );
    }

    #[tokio::test]
    async fn device_snapshot_reports_metadata_and_current() {
        let mgr = LoginManager::new(Some("pass"));
        let secret = binding(0x17);
        let id = mgr
            .create_session(&secret, "10.0.0.9", "Mozilla/5.0 Firefox/123")
            .await;

        let devices = mgr.device_snapshot(Some(&id)).await;
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].session_id, id);
        assert_eq!(devices[0].created_ip, "10.0.0.9");
        assert_eq!(devices[0].user_agent, "Mozilla/5.0 Firefox/123");
        assert!(devices[0].current, "the requesting session is flagged");
        assert!(!mgr.device_snapshot(Some("someone-else")).await[0].current);
    }

    #[test]
    fn decode_binding_secret_accepts_only_a_full_url_safe_secret() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;

        let raw = [0xAB; BINDING_SECRET_BYTES];
        assert_eq!(
            decode_binding_secret(&URL_SAFE_NO_PAD.encode(raw)).as_deref(),
            Some(&raw[..])
        );
        for wrong_length in [
            URL_SAFE_NO_PAD.encode([0xAB; 16]),
            URL_SAFE_NO_PAD.encode([0xAB; 64]),
        ] {
            assert!(decode_binding_secret(&wrong_length).is_none());
        }
        for garbage in ["", "!@#$%^&*()"] {
            assert!(decode_binding_secret(garbage).is_none());
        }
    }

    #[test]
    fn passphrase_strength_rejects_short_secrets() {
        assert!(check_passphrase_strength("short").is_some());
        assert!(check_passphrase_strength("longenough").is_none());
    }

    #[test]
    fn build_login_cookie_carries_secure_only_over_tls() {
        let insecure = build_login_cookie("abc123", false);
        for needle in [
            "aoe_session=abc123",
            "HttpOnly",
            "SameSite=Strict",
            "Max-Age=2592000",
        ] {
            assert!(insecure.contains(needle), "{insecure:?} lacks {needle}");
        }
        assert!(!insecure.contains("Secure"));
        assert!(build_login_cookie("abc123", true).contains("Secure"));
    }

    #[test]
    fn extract_login_session_reads_only_the_session_cookie() {
        let request = |cookie: Option<&str>| {
            let mut builder = axum::http::Request::builder();
            if let Some(cookie) = cookie {
                builder = builder.header(header::COOKIE, cookie);
            }
            builder.body(axum::body::Body::empty()).unwrap()
        };
        assert_eq!(
            extract_login_session(&request(Some("aoe_token=foo; aoe_session=bar123"))),
            Some("bar123".to_string())
        );
        assert_eq!(extract_login_session(&request(Some("aoe_token=foo"))), None);
        assert_eq!(extract_login_session(&request(None)), None);
    }

    /// #1235: a session survives a restart, but only while the passphrase that minted
    /// it does. Elevation is a recency claim, so a restart breaks it; an armed lockout
    /// is an attacker's budget, so a restart must not reset it.
    #[tokio::test]
    async fn persisted_sessions_rehydrate_but_elevation_does_not() {
        let dir = tempfile::tempdir().unwrap();
        let secret = binding(0xA1);

        let id = {
            let mgr = LoginManager::with_persistence(Some("hunter2"), dir.path());
            let id = session(&mgr, &secret).await;
            assert!(mgr.elevate_session(&id).await);
            for _ in 0..MAX_ELEVATION_FAILURES {
                mgr.record_elevation_failure(&id).await;
            }
            id
        };

        let restarted = LoginManager::with_persistence(Some("hunter2"), dir.path());
        assert!(restarted.validate_session(&id, &secret).await);
        assert!(
            !restarted.is_elevated(&id).await,
            "elevation is not persisted"
        );
        assert!(
            restarted.elevation_lockout_remaining(&id).await.is_some(),
            "an armed lockout must survive a restart"
        );

        let rekeyed = LoginManager::with_persistence(Some("second-pass"), dir.path());
        assert!(
            !rekeyed.validate_session(&id, &secret).await,
            "changing the passphrase must drop persisted sessions"
        );
    }

    #[tokio::test]
    async fn logout_all_clears_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let secret = binding(0xE5);

        let id = {
            let mgr = LoginManager::with_persistence(Some("pass"), dir.path());
            let id = session(&mgr, &secret).await;
            assert_eq!(mgr.logout_all().await, 1);
            assert!(!mgr.validate_session(&id, &secret).await);
            id
        };

        let restarted = LoginManager::with_persistence(Some("pass"), dir.path());
        assert!(!restarted.validate_session(&id, &secret).await);
    }

    #[tokio::test]
    async fn load_drops_expired_entries() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SESSIONS_FILE);
        let file = PersistedFile {
            schema_version: SESSIONS_SCHEMA_VERSION,
            passphrase_hash: Some(hash_passphrase("pass")),
            sessions: vec![PersistedSession {
                id: "stale".to_string(),
                binding_hash: URL_SAFE_NO_PAD.encode([0x18u8; 32]),
                expires_at_ms: now_ms().saturating_sub(60_000),
                created_at_ms: now_ms().saturating_sub(120_000),
                created_ip: "127.0.0.1".to_string(),
                user_agent: "old".to_string(),
                elevation_failures: 0,
                elevation_locked_until_ms: 0,
            }],
        };
        write_sessions(&path, &file);

        let loaded = load_sessions(&path, Some("pass")).unwrap();
        assert!(loaded.is_empty(), "expired entry must be dropped on load");
    }

    #[tokio::test]
    async fn persistence_disabled_writes_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let mgr = LoginManager::new(Some("pass"));
        mgr.create_session(&binding(0x19), "127.0.0.1", "ua").await;
        assert!(!dir.path().join(SESSIONS_FILE).exists());
    }

    /// The store is fail-closed on a planted symlink: neither the write path nor the
    /// startup rewrite may follow one, and a loose-permission file is refused too.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlinked_session_store_is_refused_not_followed() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("outside.toml");
        std::fs::write(&target, "original").unwrap();
        let store = dir.path().join(SESSIONS_FILE);
        std::os::unix::fs::symlink(&target, &store).unwrap();

        let file = PersistedFile {
            schema_version: SESSIONS_SCHEMA_VERSION,
            passphrase_hash: Some(hash_passphrase("pass")),
            sessions: vec![],
        };
        assert!(!write_sessions(&store, &file));
        assert!(check_path_security(&store).is_err());

        let _mgr = LoginManager::with_persistence(Some("pass"), dir.path());
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "original",
            "the symlink target must never receive the session store"
        );

        // Permissions: only a 0600 file (under the 0700 tempdir) is accepted, and a
        // path that does not exist yet is fine.
        for (mode, ok) in [(0o644, false), (0o600, true)] {
            let path = dir.path().join(format!("perm{mode:o}.toml"));
            std::fs::write(&path, "x").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(check_path_security(&path).is_ok(), ok, "mode {mode:o}");
        }
        assert!(check_path_security(&dir.path().join("missing.toml")).is_ok());
    }
}
