//! The dashboard access token: generation, validation, rotation, and the
//! file it is persisted to.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::info;

use crate::server::auth;

pub(super) struct TokenState {
    current: Option<String>,
    previous: Option<String>,
    grace_expires: Option<tokio::time::Instant>,
    lifetime: Duration,
    grace: Duration,
}

/// Manages auth tokens with rotation and grace periods.
pub struct TokenManager {
    state: RwLock<TokenState>,
}

pub(super) const DEFAULT_TOKEN_GRACE: Duration = Duration::from_secs(300);

impl TokenManager {
    pub fn new(initial_token: Option<String>, lifetime: Duration) -> Self {
        Self::with_grace(initial_token, lifetime, DEFAULT_TOKEN_GRACE)
    }

    pub fn with_grace(initial_token: Option<String>, lifetime: Duration, grace: Duration) -> Self {
        Self {
            state: RwLock::new(TokenState {
                current: initial_token,
                previous: None,
                grace_expires: None,
                lifetime,
                grace,
            }),
        }
    }

    /// Check if auth is disabled (no-auth mode).
    pub async fn is_no_auth(&self) -> bool {
        self.state.read().await.current.is_none()
    }

    /// Validate a token against current and previous (grace period).
    /// Returns `(is_valid, needs_cookie_upgrade)`.
    pub async fn validate(&self, token: &str) -> (bool, bool) {
        let state = self.state.read().await;

        if let Some(ref current) = state.current {
            if auth::constant_time_eq(token, current) {
                return (true, false);
            }
        }

        // Check previous token within grace period
        if let Some(ref previous) = state.previous {
            if let Some(grace_expires) = state.grace_expires {
                if tokio::time::Instant::now() < grace_expires
                    && auth::constant_time_eq(token, previous)
                {
                    return (true, true);
                }
            }
        }

        (false, false)
    }

    /// Get the current token value (for setting cookies).
    pub async fn current_token(&self) -> Option<String> {
        self.state.read().await.current.clone()
    }

    pub async fn lifetime_secs(&self) -> u64 {
        self.state.read().await.lifetime.as_secs()
    }

    /// How long a rotated-out token stays valid. The rotation loop reads it
    /// here so its cleanup deadline cannot drift from what `validate` accepts.
    pub(super) async fn grace(&self) -> Duration {
        self.state.read().await.grace
    }

    /// Clear the previous token once its grace window has closed.
    pub async fn clear_previous(&self) {
        let mut state = self.state.write().await;
        state.previous = None;
        state.grace_expires = None;
    }

    /// Whether a rotated-out token is still held, so the rotation loop's
    /// cleanup deadline can be asserted.
    #[cfg(test)]
    pub(super) async fn holds_previous(&self) -> bool {
        self.state.read().await.previous.is_some()
    }

    /// Rotate: generate new token, move current to previous with grace period.
    pub async fn rotate(&self) {
        let mut state = self.state.write().await;
        let new_token = generate_token();
        let grace = state.grace;

        state.previous = state.current.take();
        state.current = Some(new_token.clone());
        state.grace_expires = Some(tokio::time::Instant::now() + grace);

        // Persist to disk
        if let Ok(app_dir) = crate::session::get_app_dir() {
            write_secret_file(&app_dir.join("serve.token"), &new_token).await;
        }

        info!(
            target: "auth.token",
            grace_secs = grace.as_secs(),
            "auth token rotated"
        );
    }

    /// Spawn a background rotation task. Production paths only call this
    /// from the `--remote` branch; debug builds also call it when the
    /// `AOE_TEST_TOKEN_LIFETIME_SECS` env override is set, so live e2e
    /// specs can observe the grace window without waiting hours.
    pub fn spawn_rotation_task(self: &Arc<Self>) {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                let (lifetime, grace) = {
                    let state = manager.state.read().await;
                    (state.lifetime, state.grace)
                };
                tokio::time::sleep(lifetime).await;
                manager.rotate().await;

                // After grace period, clear previous
                tokio::time::sleep(grace).await;
                {
                    let mut state = manager.state.write().await;
                    state.previous = None;
                    state.grace_expires = None;
                }
            }
        });
    }
}

/// Read `AOE_TEST_TOKEN_LIFETIME_SECS`. Debug builds only; ignored in
/// release so production cannot be forced into a short rotation cycle
/// by a stray env var.
#[cfg(debug_assertions)]
pub(super) fn test_token_lifetime_override() -> Option<Duration> {
    std::env::var("AOE_TEST_TOKEN_LIFETIME_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .map(Duration::from_secs)
}

#[cfg(not(debug_assertions))]
pub(super) fn test_token_lifetime_override() -> Option<Duration> {
    None
}

/// Read `AOE_TEST_TOKEN_GRACE_SECS`. Debug builds only.
#[cfg(debug_assertions)]
pub(super) fn test_token_grace_override() -> Option<Duration> {
    std::env::var("AOE_TEST_TOKEN_GRACE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .map(Duration::from_secs)
}

#[cfg(not(debug_assertions))]
pub(super) fn test_token_grace_override() -> Option<Duration> {
    None
}

/// Write a file with owner-only permissions (0600) to protect secrets.
#[cfg(unix)]
pub(super) async fn write_secret_file(path: &std::path::Path, contents: &str) {
    use tokio::io::AsyncWriteExt;
    let opts = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .await;
    if let Ok(mut file) = opts {
        let _ = file.write_all(contents.as_bytes()).await;
    }
}

#[cfg(not(unix))]
pub(super) async fn write_secret_file(path: &std::path::Path, contents: &str) {
    let _ = tokio::fs::write(path, contents).await;
}

/// Generate a cryptographically random 64-character hex token (256 bits of entropy).
pub(crate) fn generate_token() -> String {
    use rand::RngExt;
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Validate that a token matches the expected format.
/// Accepts 64-char hex (new) or 32-char alphanumeric (legacy).
pub(super) fn is_valid_token_format(token: &str) -> bool {
    let len = token.len();
    (len == 64 || len == 32)
        && token
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c.is_ascii_lowercase())
}

/// Load an existing auth token from disk if it was last used less than 24
/// hours ago, otherwise generate a fresh one and persist it.
///
/// "Last used" is the file's mtime, which we refresh on every reuse. The age
/// window is therefore idle-based: a server that is restarted at least once a
/// day keeps the same token indefinitely, and only a token untouched for 24h
/// rotates. This is deliberate. A token change forces the rotation-prune path
/// (`retain_owners`) to drop push subscriptions bound to the now-stale hash,
/// so if the window were measured from creation, every restart after the first
/// day would rotate the token and silently kill push notifications until each
/// device re-subscribed (#3386). Rotation while the server runs continuously
/// is still driven by the scheduled rotation loop, not by this function.
pub(super) async fn load_or_generate_token() -> anyhow::Result<String> {
    let app_dir = crate::session::get_app_dir()?;
    let max_age = std::time::Duration::from_secs(24 * 60 * 60);
    Ok(load_or_generate_token_at(&app_dir.join("serve.token"), max_age).await)
}

pub(super) async fn load_or_generate_token_at(
    token_path: &std::path::Path,
    max_age: std::time::Duration,
) -> String {
    // Try to reuse existing token if it was used recently enough.
    if let Ok(metadata) = tokio::fs::metadata(&token_path).await {
        if let Ok(modified) = metadata.modified() {
            let age = std::time::SystemTime::now()
                .duration_since(modified)
                .unwrap_or_default();
            if age < max_age {
                if let Ok(token) = tokio::fs::read_to_string(&token_path).await {
                    let token = token.trim().to_string();
                    if !token.is_empty() && is_valid_token_format(&token) {
                        // Refresh the mtime so this reuse resets the idle
                        // window; the token stays stable across restarts.
                        write_secret_file(token_path, &token).await;
                        return token;
                    }
                }
            }
        }
    }

    let token = generate_token();
    write_secret_file(token_path, &token).await;
    token
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_token_correct_length_and_charset() {
        let token = generate_token();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn valid_token_format_accepts_hex_64() {
        assert!(is_valid_token_format(
            "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"
        ));
    }

    #[test]
    fn valid_token_format_accepts_legacy_32() {
        assert!(is_valid_token_format("abcdef0123456789abcdef0123456789"));
    }

    #[test]
    fn valid_token_format_rejects_garbage() {
        assert!(!is_valid_token_format("short"));
        assert!(!is_valid_token_format(""));
        assert!(!is_valid_token_format("ZZZZ0000111122223333444455556666"));
    }

    #[tokio::test]
    async fn token_manager_validates_current() {
        let mgr = TokenManager::new(Some("abc123".to_string()), Duration::from_secs(3600));
        let (valid, upgrade) = mgr.validate("abc123").await;
        assert!(valid);
        assert!(!upgrade);
    }

    #[tokio::test]
    async fn token_manager_rejects_invalid() {
        let mgr = TokenManager::new(Some("abc123".to_string()), Duration::from_secs(3600));
        let (valid, _) = mgr.validate("wrong").await;
        assert!(!valid);
    }

    #[tokio::test]
    async fn token_manager_validates_previous_in_grace() {
        let mgr = TokenManager::new(Some("old_token".to_string()), Duration::from_secs(3600));
        mgr.rotate().await;

        // Old token should still be valid during grace period
        let (valid, upgrade) = mgr.validate("old_token").await;
        assert!(valid);
        assert!(upgrade); // needs cookie upgrade

        // New token should also be valid
        let current = mgr.current_token().await.unwrap();
        let (valid, upgrade) = mgr.validate(&current).await;
        assert!(valid);
        assert!(!upgrade);
    }

    #[tokio::test]
    async fn token_manager_rotate_changes_token() {
        let mgr = TokenManager::new(Some("original".to_string()), Duration::from_secs(3600));
        let before = mgr.current_token().await;
        mgr.rotate().await;
        let after = mgr.current_token().await;
        assert_ne!(before, after);
    }

    #[tokio::test]
    async fn load_or_generate_token_is_stable_across_restarts_but_rotates_when_idle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serve.token");
        let day = std::time::Duration::from_secs(24 * 60 * 60);

        // First start generates and persists a token.
        let first = load_or_generate_token_at(&path, day).await;
        assert!(is_valid_token_format(&first));

        // A restart within the window reuses the token AND refreshes its mtime,
        // so the idle clock is measured from last use, not creation. Backdate
        // the file to simulate a day-old server; the restart must reuse the
        // token and reset its age near zero. Before #3386 the mtime was set
        // only at creation, so a day-old token rotated on the very next restart
        // and silently killed push. (Use a generous window here so the reuse
        // path runs regardless of how precisely the sandbox fs honors the
        // backdate; the rotation case below is asserted deterministically.)
        let backdated = std::time::SystemTime::now() - std::time::Duration::from_secs(23 * 60 * 60);
        std::fs::File::open(&path)
            .unwrap()
            .set_modified(backdated)
            .unwrap();
        let reused = load_or_generate_token_at(&path, day * 100).await;
        assert_eq!(
            reused, first,
            "a restart within the window must reuse the token"
        );
        let age = std::fs::metadata(&path)
            .unwrap()
            .modified()
            .unwrap()
            .elapsed()
            .unwrap();
        assert!(
            age < std::time::Duration::from_secs(60),
            "reuse must refresh the mtime, got age {age:?}"
        );

        // A token older than the window rotates. Drive this with a zero-length
        // window so any existing file counts as stale, independent of the
        // filesystem's mtime precision: the next start must generate a fresh one.
        let rotated = load_or_generate_token_at(&path, std::time::Duration::ZERO).await;
        assert_ne!(rotated, first, "a token idle past the window rotates");
    }

    #[tokio::test]
    async fn token_manager_no_auth_mode() {
        let mgr = TokenManager::new(None, Duration::from_secs(3600));
        assert!(mgr.is_no_auth().await);
    }
}
