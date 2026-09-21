//! The dashboard access token.

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

    /// Rotate.
    pub async fn rotate(&self) -> tokio::time::Instant {
        let mut state = self.state.write().await;
        let new_token = generate_token();
        let grace = state.grace;
        let grace_expires = tokio::time::Instant::now() + grace;

        state.previous = state.current.take();
        state.current = Some(new_token.clone());
        state.grace_expires = Some(grace_expires);

        // Persist to disk
        if let Ok(app_dir) = crate::session::get_app_dir() {
            write_secret_file(&app_dir.join("serve.token"), &new_token).await;
        }

        info!(
            target: "auth.token",
            grace_secs = grace.as_secs(),
            "auth token rotated"
        );
        grace_expires
    }

    /// Spawn a background rotation task.
    pub fn spawn_rotation_task(self: &Arc<Self>) {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                let lifetime = manager.state.read().await.lifetime;
                tokio::time::sleep(lifetime).await;
                let grace_expires = manager.rotate().await;
                tokio::time::sleep_until(grace_expires).await;
                manager.clear_previous().await;
            }
        });
    }
}

/// Read `AOE_TEST_TOKEN_LIFETIME_SECS`.
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
        // write_all can return while the blocking write is still queued.
        let _ = file.flush().await;
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
pub(super) fn is_valid_token_format(token: &str) -> bool {
    let len = token.len();
    (len == 64 || len == 32)
        && token
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c.is_ascii_lowercase())
}

/// Load an existing auth token from disk if it was last used less than 24 hours ago,
/// otherwise generate a fresh one and persist it.
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
    fn token_format_accepts_hex_64_and_legacy_32() {
        let generated = generate_token();
        assert_eq!(generated.len(), 64);
        assert!(is_valid_token_format(&generated));
        assert!(is_valid_token_format("abcdef0123456789abcdef0123456789"));
        for bad in ["short", "", "ZZZZ0000111122223333444455556666"] {
            assert!(!is_valid_token_format(bad), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn token_manager_validates_only_the_current_token() {
        let mgr = TokenManager::new(Some("abc123".to_string()), Duration::from_secs(3600));
        assert_eq!(mgr.validate("abc123").await, (true, false));
        assert!(!mgr.validate("wrong").await.0);
        assert!(
            TokenManager::new(None, Duration::from_secs(3600))
                .is_no_auth()
                .await
        );
    }

    #[tokio::test]
    async fn token_manager_validates_previous_in_grace() {
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let mgr = TokenManager::new(Some("old_token".to_string()), Duration::from_secs(3600));
        mgr.rotate().await;

        // The old token stays valid through the grace window, flagged for cookie upgrade.
        assert_eq!(mgr.validate("old_token").await, (true, true));

        // The rotation minted a different token, and it validates without an upgrade.
        let current = mgr.current_token().await.unwrap();
        assert_ne!(current, "old_token");
        assert_eq!(mgr.validate(&current).await, (true, false));
    }

    #[cfg(unix)]
    #[test]
    fn write_secret_file_returns_after_contents_are_visible() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let (open_tx, open_rx) = std::sync::mpsc::channel::<()>();
            let opener_gate = tokio::task::spawn_blocking(move || {
                let _ = open_rx.recv();
            });
            let mut write = std::pin::pin!(write_secret_file(&path, "fixture-secret"));
            assert!(futures_util::poll!(write.as_mut()).is_pending());

            // Queue this gate after open, before the writer can submit its write.
            let (write_tx, write_rx) = std::sync::mpsc::channel::<()>();
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let writer_gate = tokio::task::spawn_blocking(move || {
                let _ = entered_tx.send(());
                let _ = write_rx.recv();
            });
            drop(open_tx);
            opener_gate.await.unwrap();
            entered_rx.await.unwrap();

            let ready = futures_util::poll!(write.as_mut()).is_ready();
            let visible_at_return = ready.then(|| std::fs::read_to_string(&path).unwrap());
            drop(write_tx);
            writer_gate.await.unwrap();
            if !ready {
                write.await;
            }
            assert_eq!(
                visible_at_return.unwrap_or_else(|| std::fs::read_to_string(&path).unwrap()),
                "fixture-secret"
            );
        });
    }

    #[tokio::test]
    async fn load_or_generate_token_is_stable_across_restarts_but_rotates_when_idle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("serve.token");
        let day = std::time::Duration::from_secs(24 * 60 * 60);

        // First start generates and persists a token.
        let first = load_or_generate_token_at(&path, day).await;
        assert!(is_valid_token_format(&first));

        // Reuse refreshes last use, rather than retaining the creation timestamp.
        let backdated = std::time::SystemTime::now() - std::time::Duration::from_secs(23 * 60 * 60);
        std::fs::File::open(&path)
            .unwrap()
            .set_modified(backdated)
            .unwrap();
        let reused = load_or_generate_token_at(&path, day).await;
        assert_eq!(
            reused, first,
            "a restart within the window must reuse the token"
        );
        let refreshed = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert!(refreshed > backdated, "reuse must refresh the mtime");

        // A zero window makes expiration independent of filesystem precision.
        let rotated = load_or_generate_token_at(&path, std::time::Duration::ZERO).await;
        assert_ne!(rotated, first, "a token idle past the window rotates");
    }
}
