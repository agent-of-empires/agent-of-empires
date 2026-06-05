//! Passphrase-based login as a second authentication factor.
//!
//! When a passphrase is configured, users must enter it after token auth
//! to access the dashboard. Login sessions are tracked server-side with
//! a device-binding secret (replaces the prior strict IP binding, see
//! #1131) and a 30-day sliding expiry window. Active use refreshes
//! the deadline; 30 days of inactivity logs the device out and
//! requires re-entering the passphrase. See #1137 for the rationale,
//! and #1163 / #1167 for the lifetime extension (matches the
//! "rarely-ever-log-out" experience users expect from a single-owner
//! dev tool).
//!
//! The device-binding model: the client generates 32 random bytes via
//! `crypto.getRandomValues`, stores them in `localStorage`, and presents
//! them on every authenticated request. The server stores only the
//! SHA-256 hash and uses a constant-time compare. A leaked session
//! cookie alone is therefore insufficient, the attacker also needs the
//! binding secret. Mobile IP rotation no longer logs anyone out because
//! IP is now telemetry only.

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

/// Session lifetime (sliding window). Refreshes on every
/// authenticated request, so an active user never sees it; the
/// effective behavior is "log out after 30 days of inactivity",
/// matching the rarely-prompt experience users expect from a tool
/// they live in (GitHub-style, not banking-style). The cookie's
/// `Max-Age=2592000` already advertised 30 days; this aligns the
/// server-side TTL with the client-side hint. See #1137 (initial
/// 24h window) and #1167 (extension rationale: bound devices stay
/// signed in independent of token rotation).
///
/// `pub(crate)` so cross-module tests can pin the value and catch
/// a silent regression to the old 24h window.
pub(crate) const SESSION_LIFETIME: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// Step-up elevation window. Required for high-risk operations
/// (terminal attach, structured view command execution, file writes,
/// destructive session ops). See #1131.
const ELEVATION_LIFETIME: Duration = Duration::from_secs(15 * 60);

/// Maximum concurrent login sessions before evicting the oldest.
const MAX_SESSIONS: usize = 50;

/// Minimum recommended passphrase length.
const MIN_PASSPHRASE_LENGTH: usize = 8;

/// Length in raw bytes of the client-generated device binding secret.
/// 32 bytes (256 bits) of entropy from `crypto.getRandomValues`. We
/// reject shorter or longer payloads to catch typos and tampering.
const BINDING_SECRET_BYTES: usize = 32;

/// Filename for the persisted login-session store under the app dir.
/// Owner-only (0600); survives daemon restart so signed-in devices are
/// not re-prompted for the passphrase on every `aoe serve` bounce. See
/// #1235.
const SESSIONS_FILE: &str = "login_sessions.toml";

/// Schema version stamped into the persisted store. Bump on a breaking
/// layout change; an older/newer/unparseable file is dropped (start
/// empty) rather than migrated, since the cost of a re-login is one
/// passphrase prompt.
const SESSIONS_SCHEMA_VERSION: u32 = 1;

/// The sliding window refreshes `expires_at` on every authenticated
/// request, but rewriting the store on every request is unacceptable
/// write amplification. Instead we only re-persist a refreshed deadline
/// once it has advanced more than this threshold since the last write.
/// Worst case after a crash: a session expires up to this much earlier
/// than it would have in memory, which is invisible against the 30-day
/// window. Mutating events (create / invalidate / elevation) always
/// persist immediately. See #1235.
const REFRESH_PERSIST_THRESHOLD: Duration = Duration::from_secs(24 * 60 * 60);

struct LoginSession {
    expires_at: Instant,
    /// SHA-256 hash of the client-presented device binding secret.
    /// Constant-time compared on validation. We never store or log
    /// the raw secret; a server-side leak of `LoginManager` state
    /// must not be replayable.
    binding_hash: [u8; 32],
    /// Step-up elevation deadline. `None` (or in the past) means the
    /// session can browse the dashboard but cannot reach the
    /// high-risk routes guarded by `is_elevated`. See #1131. Never
    /// persisted: a daemon restart is a legitimate recency-break, so
    /// a high-risk action re-prompts for the passphrase. See #1235.
    elevated_until: Option<Instant>,
    /// Per-session failed elevation attempts since the last reset.
    /// Bound to the session id (not the client IP) so an attacker who
    /// holds session + binding cannot defeat the rate limiter by
    /// rotating IPs (mobile carrier, Tor, residential proxies).
    /// Reset on a successful elevation or full lockout expiry.
    elevation_failures: u32,
    /// Lockout deadline that gates further `/api/login/elevate`
    /// attempts for this session. `None` outside an active lockout.
    elevation_locked_until: Option<Instant>,
    /// Wall-clock creation time, for the connected-devices view. Kept
    /// as `SystemTime` (not `Instant`) so it round-trips across a
    /// daemon restart. See #1235.
    created_at: SystemTime,
    /// Client IP at creation, display-only telemetry for the devices
    /// view. Never consulted for auth (#1131 removed IP binding).
    created_ip: String,
    /// User-agent string captured at login, for a friendly device
    /// label in the devices view. Display-only.
    user_agent: String,
    /// Deadline value at the last time this session was persisted to
    /// disk. Drives the `REFRESH_PERSIST_THRESHOLD` coalescing so a
    /// sliding refresh only triggers a write once it has moved far
    /// enough. Never serialized. See #1235.
    last_persisted_expires_at: Instant,
}

/// Threshold for the per-session elevation rate limiter. Tighter than
/// the per-IP limiter on `/api/login` because the attacker must
/// already hold a valid session + binding to even reach the elevate
/// endpoint, so each failed attempt is a stronger signal of brute
/// force. Three attempts trips a 15-minute lockout.
const MAX_ELEVATION_FAILURES: u32 = 3;
const ELEVATION_LOCKOUT: Duration = Duration::from_secs(15 * 60);

/// Manages passphrase verification and login session lifecycle.
pub struct LoginManager {
    passphrase_hash: Option<String>,
    sessions: RwLock<HashMap<String, LoginSession>>,
    /// Path to the on-disk session store, when persistence is enabled
    /// and an app dir is available. `None` disables persistence (tests,
    /// `auth.persist_sessions = false`, or no resolvable app dir). See
    /// #1235.
    sessions_path: Option<PathBuf>,
}

/// Argon2 hash of a passphrase with a fresh random salt. The PHC string
/// carries its own salt, so a later `argon2_verify` against it detects
/// a passphrase change across restarts even though two hashes of the
/// same passphrase never compare byte-equal. See #1235.
fn hash_passphrase(passphrase: &str) -> String {
    use argon2::password_hash::SaltString;
    use argon2::{Argon2, PasswordHasher};
    use rand::RngExt;

    let mut salt_bytes = [0u8; 16];
    rand::rng().fill(&mut salt_bytes);
    let salt = SaltString::encode_b64(&salt_bytes).expect("salt encoding must succeed");
    Argon2::default()
        .hash_password(passphrase.as_bytes(), &salt)
        .expect("argon2 hashing must not fail")
        .to_string()
}

/// Verify a passphrase against a stored argon2 PHC hash string.
fn argon2_verify(passphrase: &str, hash: &str) -> bool {
    use argon2::password_hash::PasswordHash;
    use argon2::{Argon2, PasswordVerifier};

    let Ok(parsed) = PasswordHash::new(hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(passphrase.as_bytes(), &parsed)
        .is_ok()
}

impl LoginManager {
    /// Create a new login manager without persistence. If `passphrase`
    /// is `Some`, hash it with argon2. Used by tests and any caller that
    /// does not want an on-disk store.
    pub fn new(passphrase: Option<&str>) -> Self {
        Self {
            passphrase_hash: passphrase.map(hash_passphrase),
            sessions: RwLock::new(HashMap::new()),
            sessions_path: None,
        }
    }

    /// Create a login manager that persists sessions under `app_dir`,
    /// rehydrating any previously stored sessions whose passphrase still
    /// matches and whose sliding window has not lapsed. A missing,
    /// unreadable, corrupt, or wrong-permission store starts empty
    /// (logged), never an error: the worst case is one extra passphrase
    /// prompt. See #1235.
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

        // Rewrite the store once at startup so the passphrase hash is
        // refreshed (rotates the salt) and any dropped/expired entries
        // are pruned on disk. Synchronous: a one-time tiny write, and no
        // tokio lock is held yet. Skip the rewrite when the path is
        // insecure (symlink, loose perms, untrusted parent dir): writing
        // there would be the same fail-open the load-side rejection
        // guards against. See #1235.
        match check_path_security(&sessions_path) {
            Ok(()) => {
                let snapshot = build_persisted(&passphrase_hash, &sessions);
                write_sessions(&sessions_path, &snapshot);
            }
            Err(e) => {
                tracing::warn!(
                    target: "auth.passphrase",
                    error = %e,
                    "refusing to write persisted login sessions to an insecure path"
                );
            }
        }

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

    /// Create a new login session bound to a device. Returns the
    /// session ID (64-char hex). `binding_secret_bytes` is the raw 32
    /// random bytes the client generated; only its SHA-256 hash is
    /// retained. `created_ip` and `user_agent` are display-only metadata
    /// for the connected-devices view; neither is consulted for auth.
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

    /// Validate a session. Checks existence, expiry, and a
    /// constant-time match against the stored device binding hash.
    /// On success, extends the sliding window. IP is no longer
    /// consulted, mobile network rotation is a normal pattern and
    /// the device-binding secret carries the identity instead. See
    /// #1131.
    pub async fn validate_session(&self, session_id: &str, presented_binding: &[u8]) -> bool {
        if session_id.is_empty() || presented_binding.len() != BINDING_SECRET_BYTES {
            return false;
        }

        let presented_hash = hash_binding_secret(presented_binding);

        // `needs_persist` is set under the lock, acted on after release:
        // a sliding refresh only re-persists once the deadline has moved
        // past `REFRESH_PERSIST_THRESHOLD`, and an expiry eviction
        // persists immediately. See #1235.
        let mut needs_persist = false;
        // `Some(deadline)` when this validation refreshed the sliding
        // window far enough to re-persist. The watermark is advanced only
        // after a successful write (below), so a failed persist keeps
        // retrying on later refreshes instead of suppressing them for the
        // next ~24h. See #1235.
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
                // Constant-time compare. `Choice::unwrap_u8()` gives a
                // 0/1 we interpret as `bool` without branching on the
                // comparison result.
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

    /// Mark a session as elevated (passphrase confirmed) for
    /// `ELEVATION_LIFETIME`. Caller is responsible for verifying the
    /// passphrase before calling. Also resets the per-session
    /// elevation failure counter, since a successful confirmation
    /// proves the legitimate user is driving the prompt.
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

    /// Whether the session's elevation endpoint is locked out. Returns
    /// the remaining seconds on the lockout window when active. Bound
    /// to the session id (not IP) so an attacker who holds session +
    /// binding can't defeat the limiter by rotating IPs. See #1131.
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

    /// Record a failed passphrase entry on `/api/login/elevate` for
    /// this session. Increments the per-session counter and arms a
    /// `ELEVATION_LOCKOUT` window when the threshold is crossed.
    /// Returns true when this call triggered a fresh lockout.
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

    /// Read elevation state. Returns `(elevated, elevated_until_secs)`:
    /// the bool reflects whether the elevation window is still open,
    /// the optional seconds-from-now value is what `/api/login/status`
    /// surfaces to the client. Returns `(false, None)` for an unknown
    /// or expired session.
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

    /// Whether the session is currently elevated. Auth middleware
    /// calls this to gate sensitive routes.
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
    /// Returns whether a session was actually removed. Same effect as a
    /// logout, but initiated by another (elevated) device. See #1235.
    pub async fn revoke_session(&self, session_id: &str) -> bool {
        let removed = self.sessions.write().await.remove(session_id).is_some();
        if removed {
            self.persist().await;
        }
        removed
    }

    /// Sign out every device: drop all sessions and persist the empty
    /// store. The escape hatch that replaces the implicit "restart logs
    /// everyone out" behavior persistence removes. Returns the number of
    /// sessions cleared. See #1235.
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
    /// `last_seen` is derived from the sliding deadline
    /// (`expires_at - SESSION_LIFETIME`), so it reflects the most recent
    /// authenticated request without a dedicated field. The session that
    /// owns `current_session_id` is flagged so the UI can label "this
    /// device". See #1235.
    pub async fn device_snapshot(&self, current_session_id: Option<&str>) -> Vec<DeviceSession> {
        let sessions = self.sessions.read().await;
        let now_inst = Instant::now();
        let now_sys = SystemTime::now();
        let mut out: Vec<DeviceSession> = sessions
            .iter()
            .map(|(id, s)| {
                // last_seen = now - (time remaining until the deadline
                // minus the full lifetime). Clamp to avoid a future
                // timestamp on a freshly refreshed session.
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
    /// Returns whether the store is now durable: `true` on a successful
    /// write (or when persistence is disabled, nothing to do), `false`
    /// when the write failed. Errors are logged, never propagated: a
    /// failed write must not break an in-flight login. Runs the blocking
    /// file write on a blocking thread so the async runtime is never
    /// stalled. See #1235.
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

/// Hash a device binding secret with SHA-256. The input has 256 bits
/// of entropy from the client's `crypto.getRandomValues`, so plain
/// SHA-256 is sufficient and avoids needing a process-scoped secret.
fn hash_binding_secret(secret: &[u8]) -> [u8; 32] {
    Sha256::digest(secret).into()
}

// ── Persistence ──────────────────────────────────────────────────────────────

/// A login session as surfaced to the connected-devices view. Derived
/// from the live in-memory sessions, so it survives a daemon restart
/// once persistence rehydrates them. See #1235.
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

/// On-disk shape of `login_sessions.toml`. `passphrase_hash` is the
/// argon2 PHC string of the passphrase in force when the file was last
/// written; on load it gates whether the sessions are rehydrated (same
/// passphrase) or dropped (changed). See #1235.
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

fn system_time_to_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn now_ms() -> u64 {
    system_time_to_ms(SystemTime::now())
}

/// Convert an in-memory `Instant` deadline to a wall-clock epoch-ms
/// value for persistence. Returns 0 when the deadline is already in the
/// past.
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

/// Atomically write the session store, owner-only (0600). Returns
/// whether the write succeeded. Errors are logged, never propagated: a
/// failed persist must not break a login, but the boolean lets callers
/// avoid advancing the on-disk-expiry watermark on a failed write.
fn write_sessions(path: &Path, file: &PersistedFile) -> bool {
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

/// Fail-closed check that the sessions store is safe to read from or
/// write to: the parent dir must exist, not be a symlink, and not be
/// group/world writable; the file itself (when it exists) must be a
/// regular, owner-only (0600) file, not a symlink. Shared by the load
/// path and the startup rewrite so neither fails open on a tampered or
/// misconfigured app dir. A missing file is fine, it has not been
/// created yet. See #1235.
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

/// Load and rehydrate persisted sessions. Returns an empty map (not an
/// error) for the benign cases: file missing, schema mismatch, or
/// passphrase changed. Returns `Err` only for states that warrant a
/// visible warning (symlink, loose perms, unreadable, unparseable), in
/// which case the caller also starts empty. See #1235.
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
/// Returns the raw bytes only when they decode to exactly
/// `BINDING_SECRET_BYTES`. Both padded and unpadded base64url are
/// accepted because browser base64url emitters disagree on padding.
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
    /// Base64url encoding of 32 random bytes the client persists in
    /// `localStorage`. Required since #1131; without it the session
    /// cannot be device-bound and the response is 400.
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
        // Treat malformed bindings as a usage error (the client sent
        // garbage), not a failed login attempt: no rate-limiter
        // increment, no audit log.
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

        // Fire-and-forget push to every existing subscriber: a new
        // device just signed in. This is the operational mitigation
        // for the one attack neither device binding nor step-up auth
        // can prevent: an attacker who has both the first-factor token
        // URL AND the passphrase (e.g. shoulder-surf + URL share). The
        // legitimate owner sees the notification on their existing
        // device and can rotate credentials. See #1131.
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
///
/// Re-verifies the passphrase against the configured hash and, on
/// success, sets the calling session's elevation window. Sensitive
/// routes (terminal attach, structured view command execution, file writes)
/// gate on the resulting `is_elevated` flag in the auth middleware.
/// Already requires a valid token, login session cookie, and device
/// binding by the time the handler runs (the middleware enforces all
/// of those). See #1131.
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

    // Two rate limiters guard this endpoint:
    //   - Per-IP via `state.rate_limiter`, shared with `/api/login`.
    //   - Per-session via `LoginManager::elevation_lockout_remaining`,
    //     so an attacker who already holds session + binding can't
    //     defeat the per-IP limiter by rotating IPs (mobile carrier,
    //     Tor, residential proxies). See #1131.
    // The session lockout is checked first so a locked session shows
    // a consistent message regardless of the caller's IP.
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
///
/// Lists the persisted login sessions as connected devices for the
/// settings devices view. The session making the request is flagged
/// `current` so the UI can label "this device". See #1235.
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
///
/// Signs every device out: drops all persisted login sessions. The
/// escape hatch that replaces the implicit "daemon restart logs
/// everyone out" behavior persistence removes. Elevation-gated by the
/// auth middleware (`requires_elevation`). Also clears the caller's own
/// cookie, since its session is among those dropped. See #1235.
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
///
/// Revokes a single device's login session from the devices view.
/// Elevation-gated. When the caller revokes its own session, the
/// response also clears the cookie. See #1235.
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
///
/// Returns whether passphrase login is required, whether the caller
/// currently holds a valid login session, and the elevation state
/// (used by the frontend to decide whether to prompt for the
/// passphrase again before a high-risk action). `authenticated` is
/// only true when both the session cookie AND the device binding
/// secret match, mirroring the auth middleware's enforcement (#1131).
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

/// Fire a fire-and-forget web push to every existing subscriber that
/// a new dashboard login just succeeded. Best-effort: any failure
/// (no push state, no subscribers, network error, encryption error)
/// is swallowed. The payload never includes the binding secret,
/// session id, auth token, or passphrase; only the user-agent string
/// truncated for display. See #1131.
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

    #[test]
    fn login_manager_disabled_when_no_passphrase() {
        let mgr = LoginManager::new(None);
        assert!(!mgr.is_enabled());
    }

    #[test]
    fn login_manager_enabled_when_passphrase_set() {
        let mgr = LoginManager::new(Some("test123"));
        assert!(mgr.is_enabled());
    }

    #[test]
    fn verify_correct_passphrase() {
        let mgr = LoginManager::new(Some("my_secret"));
        assert!(mgr.verify_passphrase("my_secret"));
    }

    #[test]
    fn verify_incorrect_passphrase() {
        let mgr = LoginManager::new(Some("my_secret"));
        assert!(!mgr.verify_passphrase("wrong"));
    }

    #[test]
    fn verify_empty_passphrase() {
        let mgr = LoginManager::new(Some("my_secret"));
        assert!(!mgr.verify_passphrase(""));
    }

    #[test]
    fn verify_fails_when_disabled() {
        let mgr = LoginManager::new(None);
        assert!(!mgr.verify_passphrase("anything"));
    }

    fn binding(byte: u8) -> Vec<u8> {
        vec![byte; BINDING_SECRET_BYTES]
    }

    #[tokio::test]
    async fn create_and_validate_session() {
        let mgr = LoginManager::new(Some("test"));
        let secret = binding(0xAA);
        let session_id = mgr.create_session(&secret, "127.0.0.1", "test-agent").await;
        assert!(mgr.validate_session(&session_id, &secret).await);
    }

    #[tokio::test]
    async fn validate_rejects_wrong_binding() {
        let mgr = LoginManager::new(Some("test"));
        let secret = binding(0xAA);
        let other = binding(0xBB);
        let session_id = mgr.create_session(&secret, "127.0.0.1", "test-agent").await;
        assert!(!mgr.validate_session(&session_id, &other).await);
    }

    #[tokio::test]
    async fn validate_accepts_after_ip_change_when_binding_matches() {
        // Regression for #1131: a mobile client whose public IP rotates
        // (Wi-Fi -> cellular handoff, CGNAT, iCloud Private Relay) must
        // not be logged out as long as the device-binding secret still
        // matches. The session has no IP field anymore; just verify
        // back-to-back validations on the same secret keep working.
        let mgr = LoginManager::new(Some("test"));
        let secret = binding(0xCC);
        let session_id = mgr.create_session(&secret, "127.0.0.1", "test-agent").await;
        assert!(mgr.validate_session(&session_id, &secret).await);
        assert!(mgr.validate_session(&session_id, &secret).await);
    }

    #[tokio::test]
    async fn validate_rejects_missing_or_empty() {
        let mgr = LoginManager::new(Some("test"));
        let secret = binding(0xDD);
        let _session_id = mgr.create_session(&secret, "127.0.0.1", "test-agent").await;
        assert!(!mgr.validate_session("nonexistent", &secret).await);
        assert!(!mgr.validate_session("", &secret).await);
    }

    #[tokio::test]
    async fn validate_rejects_wrong_length_binding() {
        let mgr = LoginManager::new(Some("test"));
        let secret = binding(0xEE);
        let session_id = mgr.create_session(&secret, "127.0.0.1", "test-agent").await;
        // 31 bytes -> rejected even though the prefix matches.
        let short = vec![0xEE; BINDING_SECRET_BYTES - 1];
        assert!(!mgr.validate_session(&session_id, &short).await);
    }

    #[tokio::test]
    async fn invalidate_session_removes_it() {
        let mgr = LoginManager::new(Some("test"));
        let secret = binding(0x11);
        let session_id = mgr.create_session(&secret, "127.0.0.1", "test-agent").await;
        mgr.invalidate_session(&session_id).await;
        assert!(!mgr.validate_session(&session_id, &secret).await);
    }

    #[tokio::test]
    async fn invalidate_unknown_session_is_noop() {
        let mgr = LoginManager::new(Some("test"));
        mgr.invalidate_session("nonexistent").await;
    }

    #[tokio::test]
    async fn elevation_starts_false_and_can_be_set() {
        let mgr = LoginManager::new(Some("test"));
        let secret = binding(0x22);
        let session_id = mgr.create_session(&secret, "127.0.0.1", "test-agent").await;
        assert!(!mgr.is_elevated(&session_id).await);
        assert!(mgr.elevate_session(&session_id).await);
        let (elevated, remaining) = mgr.elevation_state(&session_id).await;
        assert!(elevated);
        assert!(remaining.is_some());
    }

    #[tokio::test]
    async fn elevation_rejects_unknown_session() {
        let mgr = LoginManager::new(Some("test"));
        assert!(!mgr.elevate_session("nope").await);
        assert!(!mgr.is_elevated("nope").await);
    }

    #[tokio::test]
    async fn elevation_lockout_arms_after_threshold() {
        // Regression for #1131 follow-up: per-session lockout so an
        // attacker with stolen session + binding can't rotate IPs to
        // defeat the per-IP rate limit while brute-forcing elevation.
        let mgr = LoginManager::new(Some("test"));
        let secret = binding(0x77);
        let session_id = mgr.create_session(&secret, "127.0.0.1", "test-agent").await;
        assert!(mgr.elevation_lockout_remaining(&session_id).await.is_none());
        for _ in 0..(MAX_ELEVATION_FAILURES - 1) {
            assert!(!mgr.record_elevation_failure(&session_id).await);
        }
        // Threshold-crossing failure arms the lockout.
        assert!(mgr.record_elevation_failure(&session_id).await);
        assert!(mgr.elevation_lockout_remaining(&session_id).await.is_some());
        // Additional failures while locked don't extend the window.
        assert!(!mgr.record_elevation_failure(&session_id).await);
    }

    #[tokio::test]
    async fn elevation_success_clears_failure_counter() {
        let mgr = LoginManager::new(Some("test"));
        let secret = binding(0x88);
        let session_id = mgr.create_session(&secret, "127.0.0.1", "test-agent").await;
        assert!(!mgr.record_elevation_failure(&session_id).await);
        assert!(mgr.elevate_session(&session_id).await);
        // Counter reset; the next failure budget starts fresh.
        for _ in 0..(MAX_ELEVATION_FAILURES - 1) {
            assert!(!mgr.record_elevation_failure(&session_id).await);
        }
        assert!(mgr.elevation_lockout_remaining(&session_id).await.is_none());
    }

    #[tokio::test]
    async fn elevation_failure_unknown_session_is_noop() {
        let mgr = LoginManager::new(Some("test"));
        assert!(!mgr.record_elevation_failure("nope").await);
        assert!(mgr.elevation_lockout_remaining("nope").await.is_none());
    }

    #[tokio::test]
    async fn elevation_expires() {
        let mgr = LoginManager::new(Some("test"));
        let secret = binding(0x33);
        let session_id = mgr.create_session(&secret, "127.0.0.1", "test-agent").await;
        assert!(mgr.elevate_session(&session_id).await);
        // Manually rewind the deadline into the past.
        {
            let mut sessions = mgr.sessions.write().await;
            if let Some(s) = sessions.get_mut(&session_id) {
                s.elevated_until = Some(Instant::now() - Duration::from_secs(1));
            }
        }
        assert!(!mgr.is_elevated(&session_id).await);
    }

    #[tokio::test]
    async fn max_sessions_evicts_oldest() {
        let mgr = LoginManager::new(Some("test"));
        let secret = binding(0x44);
        let mut first_id = String::new();
        for i in 0..MAX_SESSIONS {
            let id = mgr.create_session(&secret, "127.0.0.1", "test-agent").await;
            if i == 0 {
                first_id = id;
            }
        }
        assert!(mgr.validate_session(&first_id, &secret).await);
        let _new_id = mgr.create_session(&secret, "127.0.0.1", "test-agent").await;
        let sessions = mgr.sessions.read().await;
        assert_eq!(sessions.len(), MAX_SESSIONS);
    }

    #[tokio::test]
    async fn cleanup_expired_removes_stale() {
        let mgr = LoginManager::new(Some("test"));
        let secret = binding(0x55);
        let session_id = mgr.create_session(&secret, "127.0.0.1", "test-agent").await;
        {
            let mut sessions = mgr.sessions.write().await;
            if let Some(s) = sessions.get_mut(&session_id) {
                s.expires_at = Instant::now() - Duration::from_secs(1);
            }
        }
        mgr.cleanup_expired().await;
        assert!(!mgr.validate_session(&session_id, &secret).await);
    }

    #[test]
    fn decode_binding_secret_accepts_url_safe_no_pad() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;
        let raw = [0xAB; BINDING_SECRET_BYTES];
        let encoded = URL_SAFE_NO_PAD.encode(raw);
        let decoded = decode_binding_secret(&encoded).expect("decodes");
        assert_eq!(decoded, raw);
    }

    #[test]
    fn decode_binding_secret_rejects_wrong_length() {
        use base64::engine::general_purpose::URL_SAFE_NO_PAD;
        use base64::Engine;
        let too_short = URL_SAFE_NO_PAD.encode([0xAB; 16]);
        assert!(decode_binding_secret(&too_short).is_none());
        let too_long = URL_SAFE_NO_PAD.encode([0xAB; 64]);
        assert!(decode_binding_secret(&too_long).is_none());
    }

    #[test]
    fn decode_binding_secret_rejects_garbage() {
        assert!(decode_binding_secret("").is_none());
        assert!(decode_binding_secret("!@#$%^&*()").is_none());
    }

    #[test]
    fn passphrase_strength_short() {
        assert!(check_passphrase_strength("short").is_some());
    }

    #[test]
    fn passphrase_strength_adequate() {
        assert!(check_passphrase_strength("longenough").is_none());
    }

    #[test]
    fn build_cookie_without_secure() {
        let cookie = build_login_cookie("abc123", false);
        assert!(cookie.contains("aoe_session=abc123"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("Max-Age=2592000"));
        assert!(!cookie.contains("Secure"));
    }

    #[test]
    fn build_cookie_with_secure() {
        let cookie = build_login_cookie("abc123", true);
        assert!(cookie.contains("Secure"));
    }

    #[test]
    fn extract_session_from_cookie_header() {
        let request = axum::http::Request::builder()
            .header(header::COOKIE, "aoe_token=foo; aoe_session=bar123")
            .body(axum::body::Body::empty())
            .unwrap();

        assert_eq!(extract_login_session(&request), Some("bar123".to_string()));
    }

    #[test]
    fn extract_session_missing_cookie() {
        let request = axum::http::Request::builder()
            .header(header::COOKIE, "aoe_token=foo")
            .body(axum::body::Body::empty())
            .unwrap();

        assert_eq!(extract_login_session(&request), None);
    }

    #[test]
    fn extract_session_no_cookie_header() {
        let request = axum::http::Request::builder()
            .body(axum::body::Body::empty())
            .unwrap();

        assert_eq!(extract_login_session(&request), None);
    }

    // ── Persistence (#1235) ─────────────────────────────────────────────────

    #[tokio::test]
    async fn persisted_session_survives_restart() {
        // Regression for #1235: a logged-in device must not be logged out
        // when the daemon restarts. Fails on the in-memory-only tree
        // because a fresh manager starts with an empty session map.
        let dir = tempfile::tempdir().unwrap();
        let secret = binding(0xA1);

        let session_id = {
            let mgr = LoginManager::with_persistence(Some("hunter2"), dir.path());
            let id = mgr.create_session(&secret, "127.0.0.1", "test-agent").await;
            assert!(mgr.validate_session(&id, &secret).await);
            id
        };

        // Simulate a daemon restart: a brand-new manager over the same
        // app dir and passphrase.
        let mgr2 = LoginManager::with_persistence(Some("hunter2"), dir.path());
        assert!(
            mgr2.validate_session(&session_id, &secret).await,
            "session should rehydrate across restart"
        );
    }

    #[tokio::test]
    async fn passphrase_change_drops_persisted_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let secret = binding(0xB2);

        let session_id = {
            let mgr = LoginManager::with_persistence(Some("first-pass"), dir.path());
            mgr.create_session(&secret, "127.0.0.1", "test-agent").await
        };

        // Restart with a different passphrase: the persisted sessions
        // must be invalidated.
        let mgr2 = LoginManager::with_persistence(Some("second-pass"), dir.path());
        assert!(
            !mgr2.validate_session(&session_id, &secret).await,
            "changing the passphrase must drop persisted sessions"
        );
    }

    #[tokio::test]
    async fn elevation_does_not_survive_restart() {
        // A daemon restart is a legitimate step-up recency break: the
        // session stays valid but must require re-elevation. See #1235.
        let dir = tempfile::tempdir().unwrap();
        let secret = binding(0xC3);

        let session_id = {
            let mgr = LoginManager::with_persistence(Some("pass"), dir.path());
            let id = mgr.create_session(&secret, "127.0.0.1", "test-agent").await;
            assert!(mgr.elevate_session(&id).await);
            assert!(mgr.is_elevated(&id).await);
            id
        };

        let mgr2 = LoginManager::with_persistence(Some("pass"), dir.path());
        assert!(mgr2.validate_session(&session_id, &secret).await);
        assert!(
            !mgr2.is_elevated(&session_id).await,
            "elevation must not persist across restart"
        );
    }

    #[tokio::test]
    async fn lockout_state_survives_restart() {
        // A restart must not reset an attacker's elevation failure budget.
        let dir = tempfile::tempdir().unwrap();
        let secret = binding(0xD4);

        let session_id = {
            let mgr = LoginManager::with_persistence(Some("pass"), dir.path());
            let id = mgr.create_session(&secret, "127.0.0.1", "test-agent").await;
            for _ in 0..MAX_ELEVATION_FAILURES {
                mgr.record_elevation_failure(&id).await;
            }
            assert!(mgr.elevation_lockout_remaining(&id).await.is_some());
            id
        };

        let mgr2 = LoginManager::with_persistence(Some("pass"), dir.path());
        assert!(
            mgr2.elevation_lockout_remaining(&session_id)
                .await
                .is_some(),
            "an armed lockout must survive a restart"
        );
    }

    #[tokio::test]
    async fn logout_all_clears_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let secret = binding(0xE5);

        let session_id = {
            let mgr = LoginManager::with_persistence(Some("pass"), dir.path());
            let id = mgr.create_session(&secret, "127.0.0.1", "test-agent").await;
            assert_eq!(mgr.logout_all().await, 1);
            assert!(!mgr.validate_session(&id, &secret).await);
            id
        };

        // The cleared state must also be persisted, not just in memory.
        let mgr2 = LoginManager::with_persistence(Some("pass"), dir.path());
        assert!(!mgr2.validate_session(&session_id, &secret).await);
    }

    #[tokio::test]
    async fn revoke_session_removes_one() {
        let mgr = LoginManager::new(Some("pass"));
        let secret = binding(0xF6);
        let id = mgr.create_session(&secret, "127.0.0.1", "test-agent").await;
        assert!(mgr.revoke_session(&id).await);
        assert!(!mgr.validate_session(&id, &secret).await);
        // Revoking a gone session reports false.
        assert!(!mgr.revoke_session(&id).await);
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
        let d = &devices[0];
        assert_eq!(d.session_id, id);
        assert_eq!(d.created_ip, "10.0.0.9");
        assert_eq!(d.user_agent, "Mozilla/5.0 Firefox/123");
        assert!(d.current, "the requesting session is flagged current");

        let others = mgr.device_snapshot(Some("someone-else")).await;
        assert!(!others[0].current);
    }

    #[tokio::test]
    async fn load_drops_expired_entries() {
        // Hand-craft a store whose single session has a past deadline; it
        // must be dropped on load while the file itself stays valid.
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
        // `new` (no persistence) must never touch disk.
        let dir = tempfile::tempdir().unwrap();
        let mgr = LoginManager::new(Some("pass"));
        let secret = binding(0x19);
        mgr.create_session(&secret, "127.0.0.1", "ua").await;
        assert!(
            !dir.path().join(SESSIONS_FILE).exists(),
            "no persistence path means no file"
        );
    }

    #[cfg(unix)]
    #[test]
    fn check_path_security_rejects_symlink_and_loose_perms() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();

        // A symlink at the sessions path is rejected (no following).
        let target = dir.path().join("real.toml");
        std::fs::write(&target, "x").unwrap();
        let link = dir.path().join("link.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(
            check_path_security(&link).is_err(),
            "symlink must be rejected"
        );

        // A world/group-accessible file is rejected.
        let loose = dir.path().join("loose.toml");
        std::fs::write(&loose, "x").unwrap();
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            check_path_security(&loose).is_err(),
            "0644 file must be rejected"
        );

        // A 0600 file under a private dir passes.
        let ok = dir.path().join("ok.toml");
        std::fs::write(&ok, "x").unwrap();
        std::fs::set_permissions(&ok, std::fs::Permissions::from_mode(0o600)).unwrap();
        // tempfile dirs are 0700, so the parent check passes too.
        assert!(check_path_security(&ok).is_ok(), "0600 file should pass");

        // A missing file (not yet created) passes: the parent is fine.
        assert!(check_path_security(&dir.path().join("missing.toml")).is_ok());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn with_persistence_skips_rewrite_on_symlinked_path() {
        // Regression for the fail-closed contract: a symlinked store must
        // not be rewritten (which would write through the link). See #1235.
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("outside.toml");
        std::fs::write(&target, "original").unwrap();
        let store = dir.path().join(SESSIONS_FILE);
        std::os::unix::fs::symlink(&target, &store).unwrap();

        let _mgr = LoginManager::with_persistence(Some("pass"), dir.path());

        // The symlink target is untouched: the startup rewrite was skipped.
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "original");
    }
}
