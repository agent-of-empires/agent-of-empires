//! Web dashboard for remote agent session access
//!
//! Provides an embedded axum web server that serves a responsive dashboard
//! for monitoring and interacting with agent sessions from any browser.

#[cfg(feature = "serve")]
pub mod acp_reconciler;
#[cfg(feature = "serve")]
pub mod acp_ws;
pub mod api;
pub mod auth;
pub mod live_ws;
pub mod login;
mod pane;
pub mod push;
pub mod push_send;
pub mod rate_limit;
pub mod tunnel;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use rust_embed::Embed;
use tokio::sync::{broadcast, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{info, Instrument};

use self::push::{PushState, StatusChange, STATUS_CHANNEL_CAPACITY};

#[cfg(feature = "serve")]
const ACP_CHANNEL_CAPACITY: usize = 256;

/// Re-export of the broadcast frame defined in `crate::acp::protocol`,
/// kept under `crate::server::` so existing supervisor/WS call sites keep
/// resolving without churn. The canonical definition lives in protocol.rs
/// so the daemon and any client share a single source of truth.
#[cfg(feature = "serve")]
pub use crate::acp::protocol::AcpBroadcastFrame;

use crate::file_watch::{FileMatcher, FileWatchService, SubscriptionHandle, WatchSpec};
use crate::session::Instance;
use crate::session::Status;
use crate::session::Storage;

use self::rate_limit::RateLimiter;

#[derive(Embed)]
#[folder = "web/dist/"]
struct StaticAssets;

struct TokenState {
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

const DEFAULT_TOKEN_GRACE: Duration = Duration::from_secs(300);

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

    /// Clear the previous token after the grace period has expired.
    /// Used by the rotation task after the 5-minute grace window.
    pub async fn clear_previous(&self) {
        let mut state = self.state.write().await;
        state.previous = None;
        state.grace_expires = None;
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
fn test_token_lifetime_override() -> Option<Duration> {
    std::env::var("AOE_TEST_TOKEN_LIFETIME_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .map(Duration::from_secs)
}

#[cfg(not(debug_assertions))]
fn test_token_lifetime_override() -> Option<Duration> {
    None
}

/// Read `AOE_TEST_TOKEN_GRACE_SECS`. Debug builds only.
#[cfg(debug_assertions)]
fn test_token_grace_override() -> Option<Duration> {
    std::env::var("AOE_TEST_TOKEN_GRACE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|&n| n > 0)
        .map(Duration::from_secs)
}

#[cfg(not(debug_assertions))]
fn test_token_grace_override() -> Option<Duration> {
    None
}

/// Per-profile cleanup defaults with a refresh timestamp. Re-resolved from
/// disk after `CLEANUP_DEFAULTS_TTL`.
pub struct CleanupDefaultsCache {
    pub refreshed_at: std::time::Instant,
    pub entries: std::collections::HashMap<String, api::CleanupDefaults>,
}

pub const CLEANUP_DEFAULTS_TTL: std::time::Duration = std::time::Duration::from_secs(30);

impl CleanupDefaultsCache {
    pub fn stale(&self) -> bool {
        self.refreshed_at.elapsed() >= CLEANUP_DEFAULTS_TTL
    }
}

/// A cached branch-diff scan (`compute_changed_files`) with its refresh
/// timestamp. The scan is the heavy part of every diff request, and both the
/// file-list endpoint and the per-file endpoint need the identical result, so
/// rapidly clicking through the sidebar would otherwise re-scan the whole tree
/// per file. The TTL is short because the working tree changes as the agent
/// edits; it only dedupes bursts of requests, and `compute_file_contents`
/// always reads the live working tree, so file contents are never served stale.
struct ChangedFilesEntry {
    refreshed_at: std::time::Instant,
    files: Vec<crate::git::diff::DiffFile>,
}

pub const CHANGED_FILES_TTL: std::time::Duration = std::time::Duration::from_millis(1500);

/// Per-profile entry tracking a live `FileWatchService` subscription and the
/// `tokio::spawn`ed forwarder that drains its receiver into
/// `AppState::disk_changed`. Stored under `AppState::disk_watch_handles`.
///
/// Teardown drops `SubscriptionHandle` first so the dispatcher
/// deregisters this id and no further events are queued, then aborts
/// `forwarder`. Aborting first would race a buffered `try_send`
/// already in flight before the deregister.
pub(crate) struct DiskWatchEntry {
    /// RAII guard from `subscribe_channel`. Drop unsubscribes and unwatches
    /// the directory if its refcount drops to zero.
    handle: SubscriptionHandle,
    /// Abort handle for the forwarder task that drains the per-profile
    /// receiver into `disk_changed`.
    forwarder: tokio::task::AbortHandle,
}

/// Whether the caller has applied tmux scrape (and suppression) to
/// `fresh.status`. `status_poll_loop` passes `TmuxApplied`; the watcher
/// consumer passes `DiskOnly`.
#[derive(Copy, Clone, Debug)]
pub(crate) enum StatusSource {
    /// Caller already scraped tmux into `fresh.status` and applied
    /// `recently_restarted` suppression. The helper trusts `fresh.status`
    /// for existing ids.
    TmuxApplied,
    /// `fresh` was loaded from disk only. Prior in-memory `status` and
    /// `idle_entered_at` win for existing ids; new ids surface with disk
    /// values.
    DiskOnly,
}

/// Shared application state accessible by all request handlers.
pub struct AppState {
    pub profile: String,
    pub read_only: bool,
    pub instances: RwLock<Vec<Instance>>,
    pub token_manager: Arc<TokenManager>,
    pub login_manager: Arc<login::LoginManager>,
    pub rate_limiter: Arc<RateLimiter>,
    pub behind_tunnel: bool,
    /// Coarse auth mode resolved once at launch (`"token"` / `"passphrase"` /
    /// `"none"`). `/api/about` and the opt-in telemetry snapshot both read this
    /// single value rather than re-deriving it; immutable for the daemon's
    /// lifetime. Never the token or passphrase itself, only the mode.
    pub auth_mode: &'static str,
    /// Coarse exposure mode resolved once at launch from the active transport
    /// (`"tunnel"` / `"tailscale"` / `"local"`), fed to the telemetry snapshot.
    /// Never a tunnel name, hostname, or `.ts.net` URL, only the mode.
    pub serve_mode: &'static str,
    /// Per-instance mutex guarding mutations that must not interleave
    /// (e.g. `ensure_session` decide-and-restart). Entries are created on
    /// first use and live for the lifetime of the process — there are only
    /// as many as the user has sessions.
    pub instance_locks: RwLock<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Session ids with an in-flight smart-rename one-shot, so a burst of rapid
    /// first prompts cannot spawn concurrent title generators for the same
    /// session. Synchronous mutex: critical sections are tiny and never span an
    /// `await`. See `session::smart_rename`.
    pub smart_rename_inflight: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Session ids that have already had a smart-rename one-shot attempt this
    /// process lifetime (success or failure). A failed or unusable first try
    /// leaves the name default, so without this every later prompt would
    /// respawn a one-shot agent; one attempt per session bounds that cost and
    /// clears the `pending` sidebar chip once an attempt has run.
    pub smart_rename_attempted: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Global cap on concurrent smart-rename one-shots so a burst of new
    /// sessions cannot fan out into N host processes each holding a slot for
    /// up to `ONESHOT_TIMEOUT`. Held only across the child spawn + wait. See
    /// `session::smart_rename` and #2348.
    pub smart_rename_semaphore: tokio::sync::Semaphore,
    /// Suppression set for the startup-recovery cascade. While an entry is
    /// present and younger than `recovery::RECENTLY_RESTARTED_TTL`, the
    /// `status_poll_loop` skips `update_status_with_metadata` for that
    /// instance and surfaces `Status::Starting` instead. Without this,
    /// `last_start_time` (which is `#[serde(skip)]`) is lost on the loop's
    /// `load_all_instances` reload, and a freshly-recovered session
    /// transitions to `Status::Error` for up to 8 seconds while the agent
    /// is still settling. Periodically GC'd by a background task.
    pub recently_restarted: crate::session::recovery::RecentlyRestarted,
    /// Ids whose startup-recovery cascade is scheduled but not yet complete.
    /// Phase A seeds it; each Phase B worker drains its id on completion. The
    /// background refresher walks it to keep queued candidates' marks in
    /// `recently_restarted` fresh past `RECENTLY_RESTARTED_TTL`, closing the
    /// race where a candidate waiting on a `STARTUP_RECOVERY_CONCURRENCY`
    /// permit ages out of suppression and trips a phantom `Status::Error`.
    pub recovery_pending: crate::session::recovery::RecoveryPending,
    /// Cached per-profile cleanup defaults for the delete dialog, with a
    /// timestamp so we re-resolve after config changes (see
    /// `CLEANUP_DEFAULTS_TTL`).
    pub cleanup_defaults_cache: RwLock<CleanupDefaultsCache>,
    /// Cached remote owner per repo path. Remote owners don't change, so
    /// entries live for the lifetime of the process.
    pub remote_owner_cache: RwLock<std::collections::HashMap<String, Option<String>>>,
    /// Short-TTL cache of `compute_changed_files` keyed by `(repo_path,
    /// base_branch)`, shared by the file-list and per-file diff endpoints so a
    /// burst of file switches reuses one branch scan. See `ChangedFilesEntry`.
    changed_files_cache:
        std::sync::RwLock<std::collections::HashMap<(String, String), ChangedFilesEntry>>,
    /// Broadcasts session status transitions to consumers (currently the
    /// push-notification module). Emitted from `status_poll_loop` after
    /// each tmux scrape when `old != new`. Keep the Sender around even
    /// when no receivers exist so callers can emit without checking.
    pub status_tx: broadcast::Sender<StatusChange>,
    /// Web Push state: VAPID keypair, subscription store, VAPID subject.
    /// None when `web.notifications_enabled` is false at startup (the
    /// feature is fully off and endpoints return 404).
    pub push: Option<Arc<PushState>>,
    /// Cached value of `web.notifications_enabled` at startup. Changes
    /// to the config flag require a server restart to take effect; this
    /// is a documented limitation of the toggle for v1.
    pub push_enabled: bool,
    /// Snapshot of the resolved WebConfig at startup. Consumed by the
    /// push consumer task to evaluate per-event-type defaults.
    pub web_config: crate::session::config::WebConfig,
    /// Broadcasts acp events to subscribed WebSocket clients. The
    /// channel carries `(session_id, serialized event JSON)` frames so
    /// clients can filter by session. Empty when no clients are
    /// connected; senders never need to check before emitting.
    #[cfg(feature = "serve")]
    pub acp_events_tx: broadcast::Sender<AcpBroadcastFrame>,
    /// Disk-backed acp event log. The single source of truth for
    /// replay: `ChannelSink::publish` writes here on every event, the
    /// WS-on-connect drain reads from here, the `/acp/replay` REST
    /// endpoint reads from here, and `Supervisor::next_seqs` is seeded
    /// from here at startup so a fresh publish gets `max_seq + 1`
    /// rather than 1.
    #[cfg(feature = "serve")]
    pub acp_event_store: Arc<crate::acp::event_store::EventStore>,
    /// Owns the per-session ACP agent subprocesses.
    #[cfg(feature = "serve")]
    pub acp_supervisor:
        Arc<crate::acp::supervisor::Supervisor<crate::acp::supervisor::ChannelSink>>,
    /// The Tier 1 plugin worker host. `None` in test harnesses that do not
    /// stand up a host; `Some` in a live daemon.
    #[cfg(feature = "serve")]
    pub plugin_host: Option<Arc<crate::plugin::host::PluginHost>>,
    /// Tracks in-flight web plugin install / update / uninstall jobs so the
    /// dashboard can tail their host-side log. In-memory; see
    /// `api::plugins::PluginJobRegistry`.
    pub plugin_jobs: Arc<crate::server::api::plugins::PluginJobRegistry>,
    /// Epoch-millis timestamp of the most recent authenticated API request.
    /// Updated by auth middleware on every successful auth. The push consumer
    /// checks this to suppress notifications when someone is actively using
    /// the web dashboard (on any device).
    pub last_web_activity: std::sync::atomic::AtomicI64,
    /// Allowlisted usage-signal counters: per-signal counts of browser reports
    /// that a surface (web dashboard / acp web UI) was opened, so the next
    /// opt-in telemetry snapshot can carry the `usage_seen` map. Monotonic
    /// counters rather than flags so the snapshot loop can decrement by exactly
    /// what it reported (like the create counter): an open that lands during an
    /// in-flight send is preserved for the next snapshot instead of being cleared
    /// away. The browser never posts to the telemetry backend; it pings the local
    /// daemon (`POST /api/telemetry/seen`), which folds the count in here.
    /// Instrumenting a new surface is one entry in `telemetry::usage_signals`.
    pub telemetry_usage_seen: crate::telemetry::usage_signals::UsageSeenCounters,
    /// Per-form-factor open counts for the web dashboard / acp, layered on
    /// the `usage_seen` registry counts above so the snapshot can report which
    /// client classes (desktop / mobile / PWA) used each surface. The registry
    /// counts the open; a classified open additionally bumps the matching class
    /// here. An unclassified open (older frontend, no `form_factor`) is counted
    /// only by the registry. See `telemetry::form_factor` and #1883.
    pub telemetry_web_clients: FormFactorCounters,
    pub telemetry_structured_clients: FormFactorCounters,
    /// Sessions created since the last opt-in telemetry snapshot. Feeds the
    /// `session_creates_since_last_snapshot` trend counter so short-lived sessions
    /// that start and end between two snapshots are still counted. Decremented (by
    /// the value reported) only after a confirmed send, so a failed send retains
    /// the count for the next snapshot instead of silently dropping it.
    pub telemetry_session_creates: std::sync::atomic::AtomicU32,
    /// Aggregate structured-interaction tallies for the next opt-in snapshot
    /// (approvals decision mix, agent/substrate switches, plan-mode, queued
    /// prompts). Same monotonic-counter, decrement-by-reported discipline as
    /// the `telemetry_*_seen` counters, so an interaction that lands during an
    /// in-flight send survives to the next snapshot. In-memory on purpose, like
    /// the `seen` counters: these are coarse opt-in adoption signals, so losing
    /// a partial window on a rare daemon crash is acceptable, and durability
    /// would be a deliberate cross-cutting change for all telemetry counters,
    /// not a per-feature one.
    pub telemetry_structured: StructuredTelemetryCounters,
    /// What the most recent serve snapshot reported, held until its send is
    /// confirmed so the originating signals (the `usage_seen` counts and the
    /// create counter) are cleared only on success. The telemetry loop is the
    /// sole reader/writer, so it never overlaps an in-flight build.
    telemetry_last_reported: std::sync::Mutex<Option<ReportedServeSignals>>,
    /// Resolved when the daemon receives SIGINT/SIGTERM/SIGHUP. Long-lived
    /// handlers (acp WS, terminal WS) clone this and `select!` on
    /// `cancelled()` so they exit promptly instead of holding axum's
    /// graceful drain open until the browser tab decides to disconnect.
    /// See #1198.
    pub shutdown: CancellationToken,
    /// Process-wide file-watch primitive. Threaded into `Storage::new` so
    /// in-process writes surface immediately via `notify_local_change`,
    /// and used to register per-profile `subscribe_channel` watches that
    /// fan into `disk_changed`.
    pub(crate) file_watch: Arc<FileWatchService>,
    /// Wakeup signal for `disk_watcher_consumer`. Per-profile forwarder
    /// tasks call `notify_one()` on every received `FileEvent`; the
    /// consumer task awaits `notified()` and reloads `state.instances`.
    /// `notify_waiters` is intentionally NOT used: the consumer does a
    /// single-receiver wait and we want at-least-once wake semantics.
    pub(crate) disk_changed: Arc<tokio::sync::Notify>,
    /// Per-profile disk-watch subscriptions plus their forwarder tasks.
    /// Keyed by profile name. Mutated by `init_disk_watch_subscriptions`
    /// at startup and by the profile create / delete REST handlers.
    pub(crate) disk_watch_handles:
        Arc<tokio::sync::Mutex<std::collections::HashMap<String, DiskWatchEntry>>>,
}

impl AppState {
    /// Read-through cache over `compute_changed_files`. Returns a fresh scan
    /// when the cached entry is missing or older than `CHANGED_FILES_TTL`;
    /// errors are never cached. Safe to call from `spawn_blocking` (the lock is
    /// a `std::sync::RwLock`, held only across the map lookup/insert).
    pub fn changed_files_cached(
        &self,
        repo_path: &std::path::Path,
        base_branch: &str,
    ) -> crate::git::error::Result<Vec<crate::git::diff::DiffFile>> {
        let key = (
            repo_path.to_string_lossy().into_owned(),
            base_branch.to_string(),
        );
        if let Ok(cache) = self.changed_files_cache.read() {
            if let Some(entry) = cache.get(&key) {
                if entry.refreshed_at.elapsed() < CHANGED_FILES_TTL {
                    return Ok(entry.files.clone());
                }
            }
        }
        let files = crate::git::diff::compute_changed_files(repo_path, base_branch)?;
        if let Ok(mut cache) = self.changed_files_cache.write() {
            // Drop expired entries while we hold the write lock so the map can't
            // grow without bound across stale (repo, base) combinations.
            cache.retain(|_, e| e.refreshed_at.elapsed() < CHANGED_FILES_TTL);
            cache.insert(
                key,
                ChangedFilesEntry {
                    refreshed_at: std::time::Instant::now(),
                    files: files.clone(),
                },
            );
        }
        Ok(files)
    }

    /// Get or create the per-instance serialization mutex. The outer
    /// `RwLock` is only held long enough to insert/lookup the `Arc<Mutex>`;
    /// the caller awaits the inner mutex without holding the map lock.
    pub async fn instance_lock(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        {
            let guard = self.instance_locks.read().await;
            if let Some(lock) = guard.get(id) {
                return lock.clone();
            }
        }
        let mut guard = self.instance_locks.write().await;
        guard
            .entry(id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Record that an authenticated web client just made a request.
    pub fn touch_web_activity(&self) {
        self.last_web_activity
            .store(epoch_millis(), std::sync::atomic::Ordering::Relaxed);
    }

    /// Returns true if an authenticated web request arrived within `threshold`.
    pub fn web_active_within(&self, threshold: std::time::Duration) -> bool {
        let last = self
            .last_web_activity
            .load(std::sync::atomic::Ordering::Relaxed);
        if last == 0 {
            return false;
        }
        let elapsed_ms = epoch_millis() - last;
        elapsed_ms >= 0 && (elapsed_ms as u64) < threshold.as_millis() as u64
    }
}

fn epoch_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Raise the soft `RLIMIT_NOFILE` so the server can sustain many WS
/// terminals at once. macOS's default soft cap of 256 is exhausted
/// quickly: each WS terminal consumes ~3 file descriptors (PTY master +
/// cloned reader + writer) plus tokio plumbing, so a handful of mobile
/// reconnect bursts leaves `openpty` and the child-spawn `dup` calls
/// failing with EMFILE.
///
/// Targets the smaller of 8192 and the hard limit. Setting soft = hard
/// directly is unreliable on macOS where the hard limit reports as
/// `RLIM_INFINITY` but the kernel caps allocation at
/// `kern.maxfilesperproc`; clamping to a known-good value avoids the
/// `setrlimit` rejection.
#[cfg(unix)]
fn raise_fd_limit() {
    use nix::sys::resource::{getrlimit, setrlimit, Resource};
    const TARGET: u64 = 8192;
    match getrlimit(Resource::RLIMIT_NOFILE) {
        Ok((soft, hard)) => {
            let target = TARGET.min(hard).max(soft);
            if target > soft {
                if let Err(e) = setrlimit(Resource::RLIMIT_NOFILE, target, hard) {
                    tracing::warn!(target: "http.middleware", "Failed to raise RLIMIT_NOFILE to {}: {}", target, e);
                } else {
                    info!(
                        "Raised RLIMIT_NOFILE soft limit from {} to {}",
                        soft, target
                    );
                }
            }
        }
        Err(e) => tracing::warn!(target: "http.middleware", "Failed to read RLIMIT_NOFILE: {}", e),
    }
}

#[cfg(not(unix))]
fn raise_fd_limit() {}

pub struct ServerConfig<'a> {
    pub profile: &'a str,
    pub host: &'a str,
    pub port: u16,
    pub no_auth: bool,
    pub read_only: bool,
    pub remote: bool,
    pub tunnel_name: Option<&'a str>,
    pub tunnel_url: Option<&'a str>,
    pub no_tailscale: bool,
    pub is_daemon: bool,
    pub passphrase: Option<&'a str>,
    /// True when the server sits behind an external reverse proxy
    /// that terminates TLS. Forces cookies to `; Secure` and trusts
    /// `X-Forwarded-For` / `cf-connecting-ip` from loopback peers,
    /// same surface as `remote`, without spawning a tunnel.
    pub behind_proxy: bool,
    pub open_browser: bool,
}

/// Resolve the coarse auth-mode label the same way `/api/about` reports it, so
/// the value is derived once from a single place. Token auth wins over a
/// passphrase second factor when both are configured.
pub(crate) async fn resolve_auth_mode(
    token_manager: &TokenManager,
    login_manager: &login::LoginManager,
) -> &'static str {
    if !token_manager.is_no_auth().await {
        "token"
    } else if login_manager.is_enabled() {
        "passphrase"
    } else {
        "none"
    }
}

pub async fn start_server(config: ServerConfig<'_>) -> anyhow::Result<()> {
    let ServerConfig {
        profile,
        host,
        port,
        no_auth,
        read_only,
        remote,
        tunnel_name,
        tunnel_url,
        no_tailscale,
        is_daemon,
        passphrase,
        behind_proxy,
        open_browser,
    } = config;

    raise_fd_limit();

    // Single live `FileWatchService` per daemon. Threaded into AppState
    // and into every `Storage::new` call so in-process writes surface via
    // `notify_local_change` and per-profile subscriptions multiplex
    // through one kernel watcher.
    let file_watch = FileWatchService::new().unwrap_or_else(|e| {
        tracing::warn!(
            target: "server.file_watch",
            error = %e,
            "FileWatchService::new failed; falling back to noop"
        );
        FileWatchService::noop()
    });

    let instances = load_all_instances(&file_watch)?;

    // Load or generate auth token
    let auth_token = if no_auth {
        eprintln!(
            "WARNING: Running without authentication. \
             Anyone with network access to this port can control your agent sessions."
        );
        None
    } else {
        Some(load_or_generate_token().await?)
    };

    let token_lifetime = test_token_lifetime_override().unwrap_or_else(|| {
        if remote {
            Duration::from_secs(4 * 60 * 60) // 4 hours
        } else {
            Duration::from_secs(24 * 60 * 60) // 24 hours (existing behavior)
        }
    });
    let token_grace = test_token_grace_override().unwrap_or(DEFAULT_TOKEN_GRACE);

    let token_manager = Arc::new(TokenManager::with_grace(
        auth_token.clone(),
        token_lifetime,
        token_grace,
    ));
    let config = crate::session::profile_config::resolve_config_or_warn(profile);
    // Feed the unread-feature gate from this daemon's resolved config. Like
    // `push_enabled`, this is read once at startup; a config change needs a
    // restart to take effect. The TUI process maintains its own copy.
    crate::session::set_unread_enabled(config.session.unread_indicator);

    // Login sessions persist across daemon restarts by default (#1235) so
    // signed-in devices are not re-prompted for the passphrase on every
    // bounce. The owner-only store lives in the app dir; fall back to an
    // in-memory manager when persistence is disabled or no app dir
    // resolves.
    let login_manager = Arc::new(if config.auth.persist_sessions {
        match crate::session::get_app_dir() {
            Ok(app_dir) => login::LoginManager::with_persistence(passphrase, &app_dir),
            Err(e) => {
                tracing::warn!(
                    target: "auth.passphrase",
                    error = %e,
                    "auth.persist_sessions is on but the app dir is unavailable; \
                     login sessions will be in-memory only and will not survive a restart"
                );
                login::LoginManager::new(passphrase)
            }
        }
    } else {
        login::LoginManager::new(passphrase)
    });
    let rate_limiter = Arc::new(RateLimiter::new());

    if login_manager.is_enabled() {
        info!("Passphrase login enabled (second-factor authentication)");
    }

    // Persist the plaintext passphrase so the TUI can display it on
    // reopen, including after a TUI restart or when the daemon was
    // started from the CLI. Owner-only perms; cleaned up on shutdown.
    if let Some(pp) = passphrase {
        if let Ok(app_dir) = crate::session::get_app_dir() {
            write_secret_file(&app_dir.join("serve.passphrase"), pp).await;
        }
    }

    // Push notifications: initialize only when the operator flag is on at
    // startup. Flipping it later requires a server restart to take effect.
    let push_enabled = config.web.notifications_enabled;
    let push_state = if push_enabled {
        match crate::session::get_app_dir() {
            Ok(dir) => match PushState::init(&dir) {
                Ok(s) => Some(Arc::new(s)),
                Err(e) => {
                    tracing::warn!(target: "http.middleware",
                        "Push notifications disabled: failed to init VAPID/state: {}",
                        e
                    );
                    None
                }
            },
            Err(e) => {
                tracing::warn!(target: "http.middleware", "Push notifications disabled: app_dir unavailable: {}", e);
                None
            }
        }
    } else {
        info!("Push notifications disabled by web.notifications_enabled=false");
        None
    };

    #[cfg(feature = "serve")]
    let acp_events_tx = broadcast::channel(ACP_CHANNEL_CAPACITY).0;
    #[cfg(feature = "serve")]
    let acp_event_store = {
        let app_dir = crate::session::get_app_dir().context("acp event store: resolve app dir")?;
        let db_path = app_dir.join("acp_events.db");
        Arc::new(
            crate::acp::event_store::EventStore::open(&db_path, config.acp.replay_events as usize)
                .context("acp event store: open")?,
        )
    };
    #[cfg(feature = "serve")]
    let acp_supervisor = {
        // Approval pushes are dispatched from `acp_event_listener`,
        // which subscribes to the broadcast that ChannelSink::publish
        // feeds and has `Arc<AppState>` in scope without a closure
        // dance through the supervisor. See #1038.
        let sink = std::sync::Arc::new(crate::acp::supervisor::ChannelSink {
            tx: acp_events_tx.clone(),
            event_store: acp_event_store.clone(),
        });
        let supervisor = std::sync::Arc::new(crate::acp::supervisor::Supervisor::with_capacity(
            sink,
            config.acp.max_concurrent_workers,
        ));
        // Seed the seq counter from disk so fresh publishes don't
        // collide with restored history. Without this, after a
        // restart the first publish would be seq=1 — duplicate of
        // the row already on disk — and INSERT OR IGNORE would
        // silently drop it.
        supervisor.hydrate_seqs(acp_event_store.all_session_seqs());
        supervisor
    };
    // The Tier 1 plugin worker host. Opening it (the plugin event-bus database,
    // the worker log dir) is cheap and side-effect-free until workers launch,
    // which happens after the daemon is up. A failure here is logged, not fatal:
    // the daemon serves fine without plugin workers.
    // The host API includes mutating session.meta.set/cas, so a read-only
    // daemon must not run plugin workers at all: gate the host on !read_only.
    #[cfg(feature = "serve")]
    let plugin_host = if read_only {
        tracing::info!(target: "plugin.host", "plugin host disabled in read-only serve mode");
        None
    } else {
        match crate::session::get_app_dir() {
            Ok(app_dir) => match crate::plugin::host::PluginHost::new(&app_dir, profile) {
                Ok(host) => Some(host),
                Err(e) => {
                    tracing::warn!(target: "plugin.host", "plugin host disabled: {e:#}");
                    None
                }
            },
            Err(e) => {
                tracing::warn!(target: "plugin.host", "plugin host disabled: {e:#}");
                None
            }
        }
    };

    // Telemetry (opt-in, no-op otherwise): announce the serve surface on boot.
    // The boot announcement fires here, before transport setup, so a launch
    // attempt is still recorded even if a remote tunnel later fails to come up.
    // The periodic `usage_snapshot` loop is spawned only after the transport is
    // resolved (below), so its first tick can report the real `serve_mode`.
    crate::telemetry::spawn_process_start(crate::telemetry::Surface::Serve);

    // Resolve the coarse auth mode once at launch; `/api/about` and the
    // telemetry snapshot both read this single value.
    let auth_mode = resolve_auth_mode(&token_manager, &login_manager).await;

    let addr = format!("{}:{}", host, port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    let local_port = listener.local_addr()?.port();

    // Start tunnel if remote mode. Preference order:
    //  1. User-specified named Cloudflare tunnel (stable, explicit choice).
    //  2. Tailscale Funnel if tailscale is installed and logged in
    //     (stable .ts.net URL, installable PWAs keep working).
    //  3. Cloudflare quick tunnel (fallback; URL rotates per restart,
    //     which breaks installed PWAs).
    // Capture the Tailscale probe result before the branch so the
    // debug log shows why we did or didn't take the Tailscale path.
    // The probe itself also logs details about each underlying call.
    let tailscale_ok = if remote && !no_tailscale {
        let available = tunnel::tailscale_available().await;
        tracing::debug!(target: "http.middleware",
            no_tailscale,
            tailscale_available = available,
            "tunnel: choosing transport"
        );
        available
    } else {
        if remote && no_tailscale {
            tracing::debug!(target: "http.middleware", "tunnel: --no-tailscale set, skipping Tailscale auto-detection");
        }
        false
    };

    let tunnel_handle = if remote {
        let handle = if let (Some(name), Some(url)) = (tunnel_name, tunnel_url) {
            tunnel::TunnelHandle::spawn_named(name, url, local_port).await?
        } else if tailscale_ok {
            info!("Tailscale detected; using Tailscale Funnel for stable HTTPS origin");
            // Do NOT fall back to Cloudflare on Tailscale failure: the
            // user is on the Tailscale path because they want the
            // stable-URL benefit, and silently downgrading to a rotating
            // Cloudflare URL would break the feature they wanted. Bail
            // with the real error; the user fixes Tailscale or passes
            // --no-tailscale to explicitly opt into Cloudflare.
            tunnel::TunnelHandle::spawn_tailscale(local_port)
                .await
                .map_err(|e| {
                    anyhow::anyhow!(
                        "Tailscale Funnel setup failed: {e}\n\n\
                         aoe detected a logged-in Tailscale on this host and did not \
                         fall back to Cloudflare, because doing so silently would \
                         give you a rotating URL that breaks installed PWAs (the \
                         reason Tailscale is the preferred transport).\n\n\
                         Ways to move forward:\n  \
                         - Fix the Tailscale issue above and re-run `aoe serve --remote`.\n  \
                         - Re-run with `aoe serve --remote --no-tailscale` to use \
                         Cloudflare intentionally (quick-tunnel URL rotates on restart).\n  \
                         - Re-run with `--tunnel-name <name> --tunnel-url <host>` \
                         to use a named Cloudflare tunnel."
                    )
                })?
        } else {
            tunnel::TunnelHandle::spawn_quick(local_port).await?
        };

        let tunnel_url_with_token = if let Some(ref token) = auth_token {
            format!("{}/?token={}", handle.url, token)
        } else {
            handle.url.clone()
        };

        // Print QR code unless running as daemon
        if !is_daemon {
            eprintln!(
                "Remote access via {} (URL is {}).",
                match handle.mode_label() {
                    "tailscale" => "Tailscale Funnel",
                    "tunnel" => "Cloudflare tunnel",
                    other => other,
                },
                if handle.is_stable_origin() {
                    "stable across restarts"
                } else {
                    "temporary; rotates on restart"
                }
            );
            tunnel::print_qr_code(&tunnel_url_with_token);
            if !handle.is_stable_origin() {
                eprintln!(
                    "\nNote: this Cloudflare quick tunnel URL changes on every restart.\n\
                     Installed PWAs (home-screen apps) break when the URL changes.\n\
                     For a stable installable dashboard, install Tailscale and run\n\
                     `tailscale up` on this host before `aoe serve --remote`, or use\n\
                     a named Cloudflare tunnel via --tunnel-name/--tunnel-url.\n"
                );
            }
        }

        // Write tunnel URL for daemon discovery. Single-line content:
        // backward-compatible with any consumer that does `head -1 serve.url`,
        // and the TUI parses both single- and multi-URL formats.
        if let Ok(app_dir) = crate::session::get_app_dir() {
            write_secret_file(&app_dir.join("serve.url"), &tunnel_url_with_token).await;
            // serve.mode lets the TUI reattach to a running daemon and
            // render the right transport label: "tunnel" for Cloudflare,
            // "tailscale" for Tailscale Funnel, "local" for local-only.
            let mode = format!("{}\n", handle.mode_label());
            if let Err(e) = tokio::fs::write(app_dir.join("serve.mode"), mode).await {
                tracing::debug!(target: "http.middleware", "Failed to write serve.mode: {e}");
            }
        }

        // Start health monitor (uses CancellationToken internally)
        handle.spawn_health_monitor();

        Some(handle)
    } else {
        // Local mode: print URLs as before.
        let make_url = |h: &str| {
            if let Some(ref token) = auth_token {
                format!("http://{}:{}/?token={}", h, port, token)
            } else {
                format!("http://{}:{}/", h, port)
            }
        };

        // Collect labeled URLs in preference order (Tailscale > LAN > localhost).
        // When bound to 0.0.0.0 we're reachable on all three; on a specific
        // host we just surface that one.
        let labeled_urls: Vec<(IpKind, String)> = if host == "0.0.0.0" {
            let mut urls: Vec<(IpKind, String)> = discover_tagged_ips()
                .into_iter()
                .map(|(kind, ip)| (kind, make_url(&ip.to_string())))
                .collect();
            urls.push((IpKind::Loopback, make_url("localhost")));
            urls
        } else {
            vec![(IpKind::Loopback, make_url(host))]
        };

        println!("aoe web dashboard running at:");
        for (_, u) in &labeled_urls {
            println!("  {}", u);
        }
        if auth_token.is_some() {
            println!();
            println!(
                "Open any URL above in a browser. Share it to access from other devices on your network."
            );
        }

        if open_browser && !is_daemon {
            if let Some((_, primary)) = labeled_urls.first() {
                maybe_open_browser(primary);
            }
        }

        // serve.url: primary URL on line 1 (unlabeled, backward-compatible
        // with any `head -1 serve.url` consumer). Alternates below as
        // `kind\turl` so the TUI can cycle them. Always owner-only perms
        // since the URL embeds the auth token.
        if let Ok(app_dir) = crate::session::get_app_dir() {
            let mut contents = String::new();
            if let Some((_, primary)) = labeled_urls.first() {
                contents.push_str(primary);
                contents.push('\n');
            }
            for (kind, url) in labeled_urls.iter().skip(1) {
                contents.push_str(kind.label());
                contents.push('\t');
                contents.push_str(url);
                contents.push('\n');
            }
            write_secret_file(&app_dir.join("serve.url"), &contents).await;
            if let Err(e) = tokio::fs::write(app_dir.join("serve.mode"), "local\n").await {
                tracing::debug!(target: "http.middleware", "Failed to write serve.mode: {e}");
            }
        }

        None
    };

    // Coarse exposure label for telemetry, read straight from the resolved
    // transport so it cannot drift from what was actually spawned: the tunnel
    // handle reports "tunnel" (Cloudflare quick or named) or "tailscale", and a
    // local-only daemon has no handle. Named-tunnel names never leak; only the
    // coarse mode is taken.
    let serve_mode: &'static str = tunnel_handle
        .as_ref()
        .map(|h| h.mode_label())
        .unwrap_or("local");

    let state = Arc::new(AppState {
        profile: profile.to_string(),
        read_only,
        instances: RwLock::new(instances),
        token_manager: Arc::clone(&token_manager),
        login_manager: Arc::clone(&login_manager),
        rate_limiter: Arc::clone(&rate_limiter),
        behind_tunnel: remote || behind_proxy,
        auth_mode,
        serve_mode,
        instance_locks: RwLock::new(std::collections::HashMap::new()),
        smart_rename_inflight: std::sync::Mutex::new(std::collections::HashSet::new()),
        smart_rename_attempted: std::sync::Mutex::new(std::collections::HashSet::new()),
        smart_rename_semaphore: tokio::sync::Semaphore::new(
            crate::session::smart_rename::MAX_CONCURRENT,
        ),
        recently_restarted: crate::session::recovery::new_recently_restarted(),
        recovery_pending: crate::session::recovery::new_recovery_pending(),
        cleanup_defaults_cache: RwLock::new(CleanupDefaultsCache {
            // Seed with an already-stale timestamp so the first request
            // forces a fresh resolve instead of handing out an empty map.
            refreshed_at: std::time::Instant::now() - CLEANUP_DEFAULTS_TTL,
            entries: std::collections::HashMap::new(),
        }),
        remote_owner_cache: RwLock::new(std::collections::HashMap::new()),
        changed_files_cache: std::sync::RwLock::new(std::collections::HashMap::new()),
        status_tx: broadcast::channel(STATUS_CHANNEL_CAPACITY).0,
        #[cfg(feature = "serve")]
        acp_events_tx: acp_events_tx.clone(),
        #[cfg(feature = "serve")]
        acp_event_store: acp_event_store.clone(),
        #[cfg(feature = "serve")]
        acp_supervisor: acp_supervisor.clone(),
        #[cfg(feature = "serve")]
        plugin_host: plugin_host.clone(),
        plugin_jobs: Arc::new(api::plugins::PluginJobRegistry::new()),
        push: push_state,
        push_enabled,
        web_config: config.web.clone(),
        last_web_activity: std::sync::atomic::AtomicI64::new(0),
        telemetry_usage_seen: crate::telemetry::usage_signals::UsageSeenCounters::new(),
        telemetry_web_clients: FormFactorCounters::default(),
        telemetry_structured_clients: FormFactorCounters::default(),
        telemetry_session_creates: std::sync::atomic::AtomicU32::new(0),
        telemetry_structured: StructuredTelemetryCounters::default(),
        telemetry_last_reported: std::sync::Mutex::new(None),
        shutdown: CancellationToken::new(),
        file_watch: Arc::clone(&file_watch),
        disk_changed: Arc::new(tokio::sync::Notify::new()),
        disk_watch_handles: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
    });

    let app = build_router(state.clone());

    // Acp workers for persisted sessions get auto-spawned by the
    // reconciler in `status_poll_loop`. The poll interval's first tick
    // fires immediately, so on cold startup this is equivalent to the
    // old in-place loop here, while also covering sessions added via
    // `aoe add --acp` while serve is already running.

    // Seed acp sessions' status from the on-disk event log before
    // any background task runs. The status_poll_loop overlay reads
    // `state.instances` and the acp_event_listener only sees
    // live transitions, so a session that was mid-turn when the
    // previous daemon died otherwise renders Idle until the next
    // lifecycle event arrives. See #1103.
    seed_acp_statuses(state.clone()).await;

    // Two-phase startup recovery. Phase A runs synchronously (acquire
    // lock, snapshot candidates, mark them in `recently_restarted`) so
    // that the marks are in place before `status_poll_loop` is spawned
    // and its first tick fires; otherwise the first poll could observe
    // missing tmux state and broadcast a phantom Idle->Error transition.
    // Phase B (the cascade workers) runs in a spawned task and holds
    // the lock until done.
    let recovery_inputs = daemon_startup_recovery_mark(state.clone()).await;

    // Periodic opt-in `usage_snapshot` loop. Spawned after the transport is
    // resolved (so the first, immediate tick reports the real `serve_mode` and a
    // daemon whose tunnel failed to start emits nothing) and after acp
    // status seeding plus the synchronous recovery marking (so that first tick's
    // session counts reflect the restored state rather than a half-loaded one).
    spawn_serve_snapshot_loop(state.clone());

    // GC the recently_restarted suppression map periodically; the TTL
    // check on read filters but does not remove entries. Without this,
    // a long-running daemon's map grows unbounded.
    {
        let gc_map = state.recently_restarted.clone();
        let shutdown = state.shutdown.clone();
        crate::task_util::spawn_supervised(
            "server.gc.recently_restarted",
            crate::task_util::PanicPolicy::Log,
            async move {
                let mut interval =
                    tokio::time::interval(crate::session::recovery::RECENTLY_RESTARTED_GC_INTERVAL);
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            crate::session::recovery::gc_recently_restarted(&gc_map);
                        }
                        _ = shutdown.cancelled() => break,
                    }
                }
            },
        );
    }

    // Trash retention sweep: auto-purge trashed sessions past their
    // retention window. First tick fires immediately (startup sweep), then
    // hourly. The daemon is the sole enforcer so there is no multi-process
    // purge race; without a daemon, expired trash is purged on the next
    // daemon start or by an explicit manual purge. Skipped entirely in
    // read-only mode: a read-only daemon must not permanently delete
    // sessions in the background, since that bypasses every handler's
    // read-only guard. See #2489.
    if !state.read_only {
        let sweep_state = state.clone();
        let shutdown = state.shutdown.clone();
        crate::task_util::spawn_supervised(
            "server.trash_retention_sweep",
            crate::task_util::PanicPolicy::Log,
            async move {
                // One-shot startup backfill: relocate trashed worktrees still
                // in the active dir (rows trashed before relocation existed)
                // and heal any pointer a crash left stale. See #2522.
                crate::server::api::reconcile_trashed_worktrees(&sweep_state).await;
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(60 * 60));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            crate::server::api::purge_expired_trash(&sweep_state).await;
                        }
                        _ = shutdown.cancelled() => break,
                    }
                }
            },
        );
    }

    if let Some((lock, candidates)) = recovery_inputs {
        // Background mark-refresher (#1264). Re-stamps every still-pending
        // candidate in `recently_restarted` every RECENTLY_RESTARTED_TTL / 2
        // so a candidate queued past the TTL behind a
        // STARTUP_RECOVERY_CONCURRENCY permit does not age out of suppression
        // and trip a phantom Status::Error in status_poll_loop. Exits once the
        // pending set drains (every worker finished) or on shutdown.
        {
            let pending = state.recovery_pending.clone();
            let recently = state.recently_restarted.clone();
            let shutdown = state.shutdown.clone();
            crate::task_util::spawn_supervised(
                "server.startup_recovery_refresher",
                crate::task_util::PanicPolicy::Log,
                async move {
                    let mut interval =
                        tokio::time::interval(crate::session::recovery::RECENTLY_RESTARTED_TTL / 2);
                    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    // First tick fires immediately; skip past it so we don't
                    // redundantly re-stamp the marks Phase A just wrote.
                    interval.tick().await;
                    loop {
                        tokio::select! {
                            _ = interval.tick() => {
                                if !crate::session::recovery::refresh_recovery_pending(
                                    &pending, &recently,
                                ) {
                                    break;
                                }
                            }
                            _ = shutdown.cancelled() => break,
                        }
                    }
                },
            );
        }

        let cascade_state = state.clone();
        crate::task_util::spawn_supervised(
            "server.startup_recovery_cascade",
            crate::task_util::PanicPolicy::Log,
            async move {
                daemon_startup_recovery_cascade(cascade_state, lock, candidates).await;
            },
        );
    }

    // Spawn background tasks
    let poll_state = state.clone();
    crate::task_util::spawn_supervised(
        "server.status_poll_loop",
        crate::task_util::PanicPolicy::Log,
        async move {
            status_poll_loop(poll_state).await;
        },
    );

    // File-watch wire-up: register the initial per-profile subscriptions
    // BEFORE the server starts serving requests so cold-start writes do not
    // rely solely on the 2s polling fallback. Per-profile subscribe errors
    // are still logged and skipped; polling stays canonical when a watch
    // cannot be installed.
    init_disk_watch_subscriptions(state.clone()).await;
    {
        let consumer_state = state.clone();
        crate::task_util::spawn_supervised(
            "server.disk_watcher_consumer",
            crate::task_util::PanicPolicy::Log,
            async move {
                disk_watcher_consumer(consumer_state).await;
            },
        );
    }

    // Acp broadcast listener: a single subscriber that handles
    // every in-process consumer of acp events. Status mirroring
    // (sidebar dot, push-notification source) and ACP-session-id
    // persistence (so `session/load` works across restart) used to be
    // two separate subscribers, which doubled the broadcast clone
    // count and locked `state.instances` twice for the events that
    // matter to both (e.g. AcpSessionAssigned).
    {
        let listener_state = state.clone();
        crate::task_util::spawn_supervised(
            "server.acp_event_listener",
            crate::task_util::PanicPolicy::Log,
            async move {
                acp_event_listener(listener_state).await;
            },
        );
    }

    // Push-notification consumer: subscribes to status_tx, applies
    // dwell + cooldown, sends pushes. No-op when push_state is None
    // (feature disabled via web.notifications_enabled=false).
    push::spawn_consumer(state.clone());

    // Launch plugin workers for every active plugin that declares a runtime.
    // Non-blocking: each worker runs in its own supervised task. A daemon with
    // no community plugin workers (the common case) does nothing here.
    #[cfg(feature = "serve")]
    if let Some(host) = state.plugin_host.clone() {
        host.start(&crate::plugin::registry()).await;
    }

    // Opt-in clean-only plugin auto-update sweep (off by default). Spawned
    // non-blocking so daemon startup never waits on git/network; freshly applied
    // updates are picked up on the next daemon restart. The plugin host (when
    // running) is passed as the notifier so a consent-needed skip surfaces as a
    // dashboard notification, not just a log line.
    let update_notifier = state
        .plugin_host
        .clone()
        .map(|h| h as std::sync::Arc<dyn crate::plugin::auto_update::UpdateNotifier>);
    crate::plugin::auto_update::spawn_if_enabled(
        &crate::session::Config::load_or_warn(),
        update_notifier,
    );

    rate_limiter.spawn_cleanup_task(state.shutdown.clone());
    login_manager.spawn_cleanup_task(state.shutdown.clone());

    if remote {
        // Inline the rotation loop here rather than calling
        // token_manager.spawn_rotation_task() so we can also invalidate
        // push subscriptions whose owner hash is no longer valid after
        // rotation. Behavior otherwise matches the original: wait one
        // lifetime, rotate, wait 300s grace, clear previous.
        let rot_state = state.clone();
        let rot_shutdown = state.shutdown.clone();
        // The tunnel URL is stable across the daemon's lifetime (Tailscale
        // and named CF tunnels are stable; quick CF rotates only on
        // restart, which is outside this task's scope). Capture once so
        // the rotation task can rebuild `serve.url` with the new token.
        let rot_base_url: Option<String> = tunnel_handle.as_ref().map(|h| h.url.clone());
        tokio::spawn(async move {
            loop {
                let lifetime = rot_state.token_manager.lifetime_secs().await;
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(lifetime)) => {}
                    _ = rot_shutdown.cancelled() => break,
                }

                // Capture the hashes of the current and (about-to-be)
                // previous tokens BEFORE rotating, so we know which
                // owner-hashes are still valid in the store.
                let pre_rotate_current = rot_state.token_manager.current_token().await;
                rot_state.token_manager.rotate().await;
                let post_rotate_current = rot_state.token_manager.current_token().await;

                // Refresh `serve.url` so the TUI display and the QR-code
                // URL stay in sync with the rotated token. Without this
                // the TUI keeps showing `?token=<old>`, which is invalid
                // 5 minutes after rotation (end of grace period).
                if let (Some(base_url), Some(token)) =
                    (rot_base_url.as_ref(), post_rotate_current.as_ref())
                {
                    let url_with_token = format!("{}/?token={}", base_url, token);
                    if let Ok(app_dir) = crate::session::get_app_dir() {
                        write_secret_file(&app_dir.join("serve.url"), &url_with_token).await;
                    }
                }

                if let Some(push) = rot_state.push.as_ref() {
                    let mut valid_hashes: Vec<[u8; 32]> = Vec::new();
                    if let Some(t) = &post_rotate_current {
                        valid_hashes.push(push::sha256_token(t));
                    }
                    if let Some(t) = &pre_rotate_current {
                        // The old token remains in the grace period (5m)
                        // so devices that haven't yet picked up the new
                        // token should keep receiving pushes.
                        valid_hashes.push(push::sha256_token(t));
                    }
                    // In no-auth mode the token is None and we use a
                    // zero hash; preserve that so zero-hash subs survive.
                    if valid_hashes.is_empty() {
                        valid_hashes.push([0u8; 32]);
                    }
                    match push.store.retain_owners(&valid_hashes).await {
                        Ok(0) => {}
                        Ok(n) => tracing::info!(target: "http.middleware",
                            removed = n,
                            "push: dropped subscriptions whose owner-hash is no longer valid after rotation"
                        ),
                        Err(e) => {
                            tracing::warn!(target: "http.middleware", error = %e, "push: retain_owners failed")
                        }
                    }
                }

                // After grace period, the previous token becomes invalid.
                // Clear it AND drop any subscriptions that were bound
                // only to the old hash (retain_owners with only the new).
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(300)) => {}
                    _ = rot_shutdown.cancelled() => break,
                }
                // Clear previous token inside TokenManager. Reuse its
                // internal state access via a tiny helper on the manager.
                rot_state.token_manager.clear_previous().await;

                if let Some(push) = rot_state.push.as_ref() {
                    let mut valid_hashes: Vec<[u8; 32]> = Vec::new();
                    if let Some(t) = rot_state.token_manager.current_token().await {
                        valid_hashes.push(push::sha256_token(&t));
                    }
                    if valid_hashes.is_empty() {
                        valid_hashes.push([0u8; 32]);
                    }
                    let _ = push.store.retain_owners(&valid_hashes).await;
                }
            }
        });
    } else if test_token_lifetime_override().is_some() && auth_token.is_some() {
        // Debug-build test path: live Playwright specs set
        // AOE_TEST_TOKEN_LIFETIME_SECS (and optionally AOE_TEST_TOKEN_GRACE_SECS)
        // so they can observe the rotation grace window without waiting hours.
        // Skips the remote-only serve.url rewrite and push retain steps because
        // neither exists in the local test setup.
        token_manager.spawn_rotation_task();
    }

    // Graceful shutdown: SIGINT (Ctrl-C), SIGTERM (`aoe serve --stop`),
    // and SIGHUP (parent session died). Without these, the default handler
    // kills the process immediately, skipping PID/URL file cleanup.
    //
    // After the signal fires the future:
    //   1. Cancels `state.shutdown` so long-lived WS handlers (acp +
    //      terminal) wake from their `select!` and close cleanly,
    //      letting `axum::serve` return promptly instead of blocking
    //      on the open WebSockets the browser hasn't disconnected.
    //   2. Spawns a 5s deadline as the safety net: if any handler
    //      somehow ignores the cancel, the process force-exits so
    //      `Ctrl-C` and `aoe serve --stop` never hang. See #1198.
    //
    // Note: this future is awaited by `with_graceful_shutdown`, which
    // signals axum to stop accepting new connections once the future
    // resolves. Wrapping `axum::serve(...).await` itself in a
    // `tokio::time::timeout` would cap TOTAL server lifetime instead
    // of just the post-signal drain, which is wrong (the server would
    // exit after 5s of normal uptime). The deadline lives inside the
    // signal handler so the clock only starts after the signal fires.
    const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
    let shutdown_state = state.clone();
    let shutdown_signal = async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut sigterm = signal(SignalKind::terminate()).ok();
            let mut sighup = signal(SignalKind::hangup()).ok();
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!(target: "serve.shutdown", signal = "SIGINT", "received signal, shutting down");
                }
                _ = async { match sigterm { Some(ref mut s) => { s.recv().await; } None => std::future::pending().await } } => {
                    tracing::info!(target: "serve.shutdown", signal = "SIGTERM", "received signal, shutting down");
                }
                _ = async { match sighup { Some(ref mut s) => { s.recv().await; } None => std::future::pending().await } } => {
                    tracing::info!(target: "serve.shutdown", signal = "SIGHUP", "received signal, shutting down");
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!(target: "serve.shutdown", "received ctrl-c, shutting down");
        }
        shutdown_state.shutdown.cancel();
        // Reap plugin workers before the force-exit deadline so no worker tree
        // is left behind when the daemon stops.
        #[cfg(feature = "serve")]
        if let Some(host) = shutdown_state.plugin_host.clone() {
            host.shutdown().await;
        }
        tokio::spawn(async {
            tokio::time::sleep(SHUTDOWN_GRACE).await;
            tracing::warn!(
                target: "shutdown",
                grace_secs = SHUTDOWN_GRACE.as_secs(),
                "graceful shutdown exceeded grace window, forcing exit"
            );
            // Force-exit skips the post-`axum::serve` cleanup block below
            // (acp detach, tunnel SIGTERM of cloudflared, removal of
            // serve.passphrase). The PID file is swept by `daemon_pid`'s
            // stale-PID check on the next start, but a leftover cloudflared
            // subprocess and residual passphrase file may survive a forced
            // exit. The common path (handlers honor cancel) returns from
            // `axum::serve` normally and runs the full cleanup.
            std::process::exit(0);
        });
    };

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal)
    .await?;

    // Detach (but do NOT kill) every acp ACP worker. The per-session
    // `aoe __acp-runner` shims outlive this daemon: a fresh
    // `aoe serve` reattaches via the reconciler on startup, so in-flight
    // turns survive `aoe serve --stop`. To actually terminate workers,
    // use `aoe acp stop [--all]`.
    acp_supervisor.detach_all().await;

    // Clean up tunnel (cancels health monitor, then sends SIGTERM to cloudflared)
    if let Some(handle) = tunnel_handle {
        handle.shutdown().await;
    }

    if let Ok(app_dir) = crate::session::get_app_dir() {
        let _ = tokio::fs::remove_file(app_dir.join("serve.passphrase")).await;
    }

    Ok(())
}

fn build_router(state: Arc<AppState>) -> Router {
    use axum::routing::{delete, get, patch, post, put};

    let app = Router::new()
        // Sessions
        .route(
            "/api/sessions",
            get(api::list_sessions).post(api::create_session),
        )
        // Static segment; registered before /api/sessions/{id} so the
        // literal "search" never resolves as a session id. See #2515.
        .route("/api/sessions/search", get(api::search_sessions))
        .route("/api/recent-projects", get(api::get_recent_projects))
        .route(
            "/api/workspace-ordering",
            put(api::update_workspace_ordering),
        )
        // Unified MCP management surface (#1996)
        .route("/api/mcp/servers", get(api::get_mcp_servers))
        .route(
            "/api/mcp/servers/{name}/resolve",
            post(api::resolve_mcp_conflict),
        )
        .route("/api/mcp/servers/{name}/keep", post(api::keep_mcp_server))
        .route("/api/mcp/servers/{name}/drop", post(api::drop_mcp_server))
        .route(
            "/api/sessions/{id}",
            patch(api::rename_session).delete(api::delete_session),
        )
        .route("/api/sessions/{id}/group", patch(api::update_session_group))
        .route(
            "/api/sessions/{id}/diff/files",
            get(api::session_diff_files),
        )
        .route("/api/sessions/{id}/diff/file", get(api::session_diff_file))
        .route(
            "/api/sessions/{id}/artifacts/{*path}",
            get(api::serve_session_artifact),
        )
        .route("/api/sessions/{id}/ensure", post(api::ensure_session))
        .route("/api/sessions/{id}/send", post(api::send_message))
        .route(
            "/api/sessions/{id}/paste-image",
            // A base64 screenshot blows past the global 1 MiB cap. 8 MiB
            // leaves headroom for the 5 MiB decoded cap (enforced in the
            // handler) plus base64's ~33% overhead and JSON framing.
            post(api::paste_image).layer(axum::extract::DefaultBodyLimit::max(8 * 1024 * 1024)),
        )
        .route("/api/sessions/{id}/output", get(api::read_output))
        .route(
            "/api/sessions/{id}/notifications",
            patch(api::update_session_notifications),
        )
        .route(
            "/api/sessions/{id}/diff-base",
            patch(api::update_session_diff_base),
        )
        .route(
            "/api/sessions/{id}/worktree-name",
            patch(api::set_worktree_name),
        )
        .route("/api/sessions/{id}/pin", patch(api::update_session_pin))
        .route(
            "/api/sessions/{id}/archive",
            patch(api::update_session_archive),
        )
        .route(
            "/api/sessions/{id}/snooze",
            patch(api::update_session_snooze),
        )
        .route("/api/sessions/{id}/trash", post(api::trash_session))
        .route("/api/sessions/{id}/restore", post(api::restore_session))
        .route(
            "/api/sessions/{id}/unread",
            patch(api::update_session_unread),
        )
        .route("/api/sessions/{id}/stop", post(api::stop_session))
        .route(
            "/api/sessions/{id}/smart-rename",
            post(api::force_smart_rename),
        )
        .route("/api/sessions/{id}/start", post(api::start_session))
        .route(
            "/api/sessions/{id}/terminal",
            post(api::ensure_terminal).delete(api::kill_terminal),
        )
        .route(
            "/api/sessions/{id}/container-terminal",
            post(api::ensure_container_terminal),
        )
        // Agents
        .route("/api/agents", get(api::list_agents))
        // Profiles
        .route(
            "/api/profiles",
            get(api::list_profiles).post(api::create_profile),
        )
        .route("/api/profiles/{name}", delete(api::delete_profile))
        .route(
            "/api/profiles/{name}/settings",
            get(api::get_profile_settings).patch(api::update_profile_settings),
        )
        .route("/api/profiles/{name}/rename", patch(api::rename_profile))
        .route("/api/default-profile", patch(api::default_profile))
        .route("/api/filesystem/browse", get(api::browse_filesystem))
        .route("/api/filesystem/home", get(api::filesystem_home))
        .route("/api/git/branches", get(api::list_branches))
        .route("/api/git/clone", post(api::clone_repo))
        .route("/api/groups", get(api::list_groups))
        .route(
            "/api/projects",
            get(api::list_projects).post(api::create_project),
        )
        .route(
            "/api/projects/{name}",
            patch(api::update_project).delete(api::delete_project),
        )
        .route("/api/docker/status", get(api::docker_status))
        // Settings + themes
        .route(
            "/api/settings",
            get(api::get_settings).patch(api::update_settings),
        )
        .route("/api/settings/schema", get(api::get_settings_schema))
        .route("/api/settings/resolved", get(api::get_settings_resolved))
        .route("/api/tips", get(api::get_tips))
        .route("/api/tips/show", post(api::set_show_tips))
        .route("/api/app-state/tip-seen", post(api::mark_tip_seen))
        // Plugin management. The enable/disable toggle gates on read-only +
        // elevation inside the handler.
        .route("/api/plugins", get(api::list_plugins))
        .route("/api/plugins/{id}/icon", get(api::serve_plugin_icon))
        .route("/api/plugins/commands", get(api::plugin_commands))
        .route("/api/plugins/ui-state", get(api::plugin_ui_state))
        .route("/api/plugins/updates", get(api::plugin_updates))
        .route("/api/plugins/discover", get(api::plugin_discover))
        .route("/api/plugins/details", get(api::plugin_details))
        .route("/api/plugins/{id}/enabled", post(api::set_plugin_enabled))
        .route("/api/plugins/{id}/action", post(api::invoke_plugin_action))
        .route(
            "/api/plugins/install/preview",
            post(api::preview_plugin_install),
        )
        .route("/api/plugins/install", post(api::start_plugin_install))
        .route(
            "/api/plugins/{id}/uninstall",
            post(api::start_plugin_uninstall),
        )
        .route("/api/plugins/jobs/{job_id}", get(api::plugin_job_status))
        .route(
            "/api/plugins/{id}/update/preview",
            get(api::plugin_update_preview),
        )
        .route(
            "/api/plugins/{id}/update/apply",
            post(api::apply_plugin_update),
        )
        .route(
            "/api/plugins/{id}/update/dismiss",
            post(api::dismiss_plugin_update),
        )
        .route(
            "/api/app-state/web-tour-seen",
            post(api::mark_web_tour_seen),
        )
        .route("/api/app-state/dismiss-update", post(api::dismiss_update))
        .route(
            "/api/app-state/web-ui-state",
            get(api::get_web_ui_state).patch(api::patch_web_ui_state),
        )
        .route(
            "/api/app-state/volume-ignores-globs-acknowledged",
            post(api::mark_volume_ignores_globs_acknowledged),
        )
        .route(
            "/api/sandbox/volume-ignores-preview",
            get(api::preview_volume_ignores_globs),
        )
        .route("/api/themes", get(api::list_themes))
        .route("/api/themes/{name}", get(api::get_resolved_theme))
        .route("/api/theme/current", get(api::get_current_theme))
        // Dedicated, non-elevated global-theme write: a cosmetic theme change
        // must not trip the passphrase wall on `PATCH /api/settings`.
        .route("/api/theme", patch(api::update_theme))
        .route("/api/sounds", get(api::list_sounds))
        .route("/api/sounds/file/{name}", get(api::serve_sound_file))
        // Push notifications
        .route("/api/push/status", get(push::get_status))
        .route(
            "/api/push/vapid-public-key",
            get(push::get_vapid_public_key),
        )
        .route("/api/push/subscribe", post(push::subscribe))
        .route("/api/push/unsubscribe", post(push::unsubscribe))
        .route("/api/push/test", post(push::test))
        // Login (second-factor auth)
        .route("/api/login", post(login::login_handler))
        .route("/api/login/elevate", post(login::elevate_handler))
        .route("/api/logout", post(login::logout_handler))
        .route("/api/login/status", get(login::login_status_handler))
        // Sign out every device (elevation-gated). See #1235.
        .route("/api/login/logout-all", post(login::logout_all_handler))
        // Revoke a single device's login session (elevation-gated).
        .route(
            "/api/login/sessions/{id}",
            delete(login::revoke_session_handler),
        )
        // Devices: the connected-devices view is backed by persisted
        // login sessions (#1235), not the old IP/UA request tracker.
        .route("/api/devices", get(login::devices_handler))
        // About (version, auth status, read-only state)
        .route("/api/about", get(api::get_about))
        // Update status (latest release, available flag)
        .route("/api/system/update-status", get(api::get_update_status))
        .route(
            "/api/log-level",
            get(api::get_log_level).patch(api::patch_log_level),
        )
        .route("/api/client-log", post(api::post_client_log))
        // Telemetry consent (browser manages opt-in via the daemon; it never
        // posts to the telemetry backend directly).
        .route("/api/telemetry/status", get(api::get_telemetry_status))
        .route("/api/telemetry/consent", post(api::set_telemetry_consent))
        .route("/api/telemetry/seen", post(api::post_telemetry_seen))
        .route(
            "/api/telemetry/structured-interaction",
            post(api::post_telemetry_structured_interaction),
        )
        // Terminal WebSockets (capture-streaming live view; the agent pane and
        // the paired host/container shells). The xterm PTY relay was removed.
        .route("/sessions/{id}/live-ws", get(live_ws::live_terminal_ws))
        .route(
            "/sessions/{id}/terminal/live-ws",
            get(live_ws::live_paired_terminal_ws),
        )
        .route(
            "/sessions/{id}/container-terminal/live-ws",
            get(live_ws::live_container_terminal_ws),
        );

    #[cfg(feature = "serve")]
    let app = app
        .route("/sessions/{id}/acp/ws", get(acp_ws::acp_ws))
        .route("/api/sessions/{id}/acp/spawn", post(api::spawn_acp))
        .route(
            "/api/sessions/{id}/acp/install-agent",
            post(api::install_agent),
        )
        .route("/api/sessions/{id}/acp", delete(api::shutdown_acp))
        .route(
            "/api/sessions/{id}/acp/switch-agent",
            post(api::switch_acp_agent),
        )
        .route(
            "/api/sessions/{id}/acp/prompt",
            // Prompt bodies carry inline base64 attachments, which blow
            // past the global 1 MiB cap. Raise the limit on this route
            // only; the server-side decoded-size caps in
            // `validate_attachments` are the real guard. 28 MiB leaves
            // headroom for the 20 MiB total decoded cap plus base64's
            // ~33% overhead and JSON framing. See #1000 / #965.
            post(api::acp_prompt).layer(axum::extract::DefaultBodyLimit::max(28 * 1024 * 1024)),
        )
        .route(
            "/api/sessions/{id}/acp/attachments/{attachment_id}",
            get(api::acp_attachment),
        )
        .route(
            "/api/sessions/{id}/acp/prompt/diff-comments",
            post(api::acp_prompt_diff_comments),
        )
        .route("/api/sessions/{id}/acp/cancel", post(api::acp_cancel))
        .route(
            "/api/sessions/{id}/acp/force_end_turn",
            post(api::acp_force_end_turn),
        )
        .route("/api/sessions/{id}/acp/files", get(api::acp_files))
        .route(
            "/api/sessions/{id}/acp/worker-log",
            get(api::acp_worker_log),
        )
        .route("/api/sessions/{id}/acp/replay", get(api::acp_replay))
        .route(
            "/api/sessions/{id}/acp/context-primer",
            get(api::acp_context_primer),
        )
        .route("/api/sessions/{id}/acp/mode", post(api::acp_set_mode))
        .route(
            "/api/sessions/{id}/acp/config-option",
            post(api::acp_set_config_option),
        )
        .route("/api/sessions/{id}/acp/enable", post(api::acp_enable))
        .route("/api/sessions/{id}/acp/disable", post(api::acp_disable))
        .route(
            "/api/sessions/{id}/acp/approvals/{nonce}",
            post(api::resolve_approval),
        )
        .route(
            "/api/sessions/{id}/acp/elicitations/{nonce}",
            post(api::resolve_elicitation),
        )
        .route("/api/acp/agents", get(api::list_acp_agents))
        .route("/api/acp/option-catalog", get(api::get_option_catalog))
        .route("/api/claude-sessions", get(api::list_claude_sessions));

    app
        // Static assets (Vite build output: assets/, manifest.json, sw.js, icons)
        .route("/assets/{*path}", get(serve_asset))
        .route("/manifest.json", get(serve_public_file))
        .route("/sw.js", get(serve_public_file))
        .route("/icon-192.png", get(serve_public_file))
        .route("/icon-512.png", get(serve_public_file))
        // SPA fallback: all other GET routes serve index.html
        .fallback(get(serve_index))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ))
        .layer(axum::middleware::from_fn(security_headers))
        .layer(axum::middleware::from_fn(http_request_span))
        .layer(axum::extract::DefaultBodyLimit::max(1024 * 1024))
        .with_state(state)
}

/// Middleware that wraps every request in an `http.request` span with a
/// generated or echoed `X-Request-Id`, then emits one completion event at
/// the level matching the response status. Logs fired inside the request
/// (auth middleware, route handlers, downstream `tracing` events) inherit
/// the span fields, so a single grep on `request_id` reconstructs the call.
///
/// Successful completions (2xx/3xx) emit at `debug`, not `info`: the web
/// UI polls `/api/sessions` every ~2s, so an info-level success log here
/// would flood `debug.log` at the default `info` filter. Users who want
/// to see every request can dial `http.request=debug` from settings;
/// 4xx (`warn`) and 5xx (`error`) stay visible at the default level.
async fn http_request_span(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let rid = request
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let span = tracing::debug_span!(
        target: "http.request",
        "http_request",
        request_id = %rid,
        method = %method,
        path = %path,
    );
    let start = std::time::Instant::now();
    let mut response = next.run(request).instrument(span.clone()).await;
    let latency_ms = start.elapsed().as_millis() as u64;
    let status = response.status().as_u16();
    span.in_scope(|| {
        if status >= 500 {
            tracing::error!(target: "http.request", status, latency_ms, "completed");
        } else if status >= 400 {
            tracing::warn!(target: "http.request", status, latency_ms, "completed");
        } else {
            tracing::debug!(target: "http.request", status, latency_ms, "completed");
        }
    });
    if let Ok(value) = rid.parse() {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

/// Content-Security-Policy for the dashboard.
///
/// - `default-src 'self'`: deny everything we don't explicitly allow.
/// - `script-src 'self' 'wasm-unsafe-eval'`: scripts are bundled by
///   Vite from the same origin; no inline scripts, no `eval`. The
///   `'wasm-unsafe-eval'` source is the CSP3 opt-in for WebAssembly
///   compilation; Shiki's Oniguruma regex engine ships as WASM, so
///   the diff syntax highlighter falls over without it (PR #1275
///   dropped this when wterm was replaced with xterm.js on the
///   incorrect premise that nothing else still needed WASM).
/// - `style-src 'self' 'unsafe-inline'`: React writes to element.style at
///   runtime (terminal font-size updates) and Tailwind v4 emits inline
///   `<style>` blocks in dev. Blocking inline styles breaks xterm.js's
///   rendered viewport.
/// - `img-src 'self' data: https://github.com https://avatars.githubusercontent.com https://raw.githubusercontent.com`:
///   repo-owner avatars are loaded from `github.com/{user}.png` which 302s
///   to `avatars.githubusercontent.com`; CSP checks both URLs across the
///   redirect, so both hosts must be allowed. `data:` covers inline icons.
///   `raw.githubusercontent.com` serves plugin screenshots resolved by the
///   plugin detail endpoint (#2484).
/// - `font-src 'self'`: Geist fonts are bundled under /fonts/.
/// - `connect-src 'self' ws: wss:`: REST + PTY WebSocket to same origin.
/// - `frame-ancestors 'none'`: CSP-native equivalent of X-Frame-Options.
/// - `base-uri 'self'`, `form-action 'self'`, `object-src 'none'`: tighten
///   the usual attack surfaces on injection bugs.
const CSP: &str = "default-src 'self'; \
    script-src 'self' 'wasm-unsafe-eval'; \
    style-src 'self' 'unsafe-inline'; \
    img-src 'self' data: https://github.com https://avatars.githubusercontent.com https://raw.githubusercontent.com; \
    font-src 'self'; \
    connect-src 'self' ws: wss:; \
    frame-ancestors 'none'; \
    base-uri 'self'; \
    form-action 'self'; \
    object-src 'none'";

/// Middleware that adds security headers to all responses.
async fn security_headers(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert("x-frame-options", "DENY".parse().unwrap());
    headers.insert("x-content-type-options", "nosniff".parse().unwrap());
    headers.insert("referrer-policy", "no-referrer".parse().unwrap());
    headers.insert("content-security-policy", CSP.parse().unwrap());
    response
}

async fn serve_index(
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
) -> impl axum::response::IntoResponse {
    let path = uri.path().trim_start_matches('/');
    if !path.is_empty()
        && path != "index.html"
        && path.contains('.')
        && StaticAssets::get(path).is_some()
    {
        return serve_embedded_file(path, &headers);
    }
    serve_embedded_file("index.html", &headers)
}

async fn serve_asset(
    axum::extract::Path(path): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> impl axum::response::IntoResponse {
    serve_embedded_file(&format!("assets/{}", path), &headers)
}

async fn serve_public_file(
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
) -> impl axum::response::IntoResponse {
    // Strip leading slash to match rust-embed paths
    let path = uri.path().trim_start_matches('/');
    serve_embedded_file(path, &headers)
}

/// Best-effort launch of `url` in the user's default browser. Suppressed
/// in environments where opening a browser is not useful: SSH sessions
/// (the user is on another host) and Linux/BSD without a display server.
/// Failures are logged but never propagate; the server keeps running.
fn maybe_open_browser(url: &str) {
    if std::env::var_os("SSH_CONNECTION").is_some() || std::env::var_os("SSH_TTY").is_some() {
        tracing::info!(target: "http.middleware", "--open ignored: running over SSH");
        return;
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "openbsd"))]
    {
        if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            tracing::info!(target: "http.middleware", "--open ignored: no DISPLAY or WAYLAND_DISPLAY set");
            return;
        }
    }

    if let Err(e) = webbrowser::open(url) {
        tracing::warn!(target: "http.middleware", "--open: failed to launch browser: {e}");
    }
}

/// The content-hashed entry bundle filename (`index-<hash>.js`) baked
/// into the embedded `index.html`. This is the dashboard's build
/// identity: the client compares its own entry script's filename
/// against this value (via `GET /api/about`) and offers a reload when
/// they differ. Installed PWAs (especially iOS) resume a long-lived
/// page with no refresh affordance, so without this prompt a phone can
/// keep running a stale dashboard for weeks after the binary updates.
pub fn web_build_id() -> Option<&'static str> {
    static ID: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        let index = StaticAssets::get("index.html")?;
        let html = std::str::from_utf8(index.data.as_ref()).ok()?;
        extract_web_build_id(html)
    })
    .as_deref()
}

/// Pull `index-<hash>.js` out of the built index.html. Vite names the
/// entry chunk `assets/index-<hash>.js`; lazy chunks get their own
/// names, so the first match is the entry.
fn extract_web_build_id(html: &str) -> Option<String> {
    let start = html.find("assets/index-")?;
    let name = &html[start + "assets/".len()..];
    let end = name.find(".js")?;
    Some(format!("{}.js", &name[..end]))
}

fn serve_embedded_file(
    path: &str,
    request_headers: &axum::http::HeaderMap,
) -> axum::response::Response {
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;

    match StaticAssets::get(path) {
        Some(file) => {
            // Strong ETag from rust-embed's content hash, so `no-cache`
            // revalidation costs a 304 instead of a re-download.
            let etag = {
                let hash = file.metadata.sha256_hash();
                let mut s = String::with_capacity(hash.len() * 2 + 2);
                s.push('"');
                for b in hash {
                    use std::fmt::Write;
                    let _ = write!(s, "{:02x}", b);
                }
                s.push('"');
                s
            };
            if request_headers
                .get(header::IF_NONE_MATCH)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|inm| {
                    inm.split(',')
                        .any(|t| t.trim().trim_start_matches("W/") == etag)
                })
            {
                return (
                    StatusCode::NOT_MODIFIED,
                    [
                        (header::ETAG, etag),
                        (header::CACHE_CONTROL, cache_control_for(path).to_string()),
                    ],
                )
                    .into_response();
            }
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, mime.as_ref().to_string()),
                    (header::ETAG, etag),
                    (header::CACHE_CONTROL, cache_control_for(path).to_string()),
                ],
                file.data.to_vec(),
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "Not found").into_response(),
    }
}

/// Cache policy for embedded dashboard files. Vite content-hashes
/// everything under `assets/`, so those are immutable; everything else
/// (index.html, sw.js, manifest, icons, fonts) must revalidate on every
/// load or an installed PWA keeps booting a stale shell long after the
/// binary shipped new assets. Revalidation is cheap: the ETag above
/// turns it into a 304.
fn cache_control_for(path: &str) -> &'static str {
    if is_content_hashed_asset(path) {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

/// True for `assets/<name>-<hash>.<ext>` where `<hash>` is a Rollup
/// content hash: 8 chars (the default length) of the base64url
/// alphabet, immediately preceded by `-`. The `assets/` prefix alone is
/// not enough: should a non-hashed file ever land there through a Vite
/// config change, a year of `immutable` would pin clients to it.
/// Misclassifying a hashed file the other way is harmless; it just
/// revalidates via ETag like everything else.
fn is_content_hashed_asset(path: &str) -> bool {
    let Some(name) = path.strip_prefix("assets/") else {
        return false;
    };
    let Some((stem, _ext)) = name.rsplit_once('.') else {
        return false;
    };
    let bytes = stem.as_bytes();
    if bytes.len() < 9 || bytes[bytes.len() - 9] != b'-' {
        return false;
    }
    bytes[bytes.len() - 8..]
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-')
}

/// Kind tag for a local IPv4 address. Ordering in this enum is also the
/// preference order for picking the "primary" URL to show in a QR: when
/// the user serves on a Tailnet, that's almost always the one they want
/// a phone (on cellular) to scan, not the LAN IP behind their NAT.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum IpKind {
    Tailscale,
    Lan,
    Loopback,
}

impl IpKind {
    pub fn label(self) -> &'static str {
        match self {
            IpKind::Tailscale => "tailscale",
            IpKind::Lan => "lan",
            IpKind::Loopback => "localhost",
        }
    }
}

/// Classify a v4 address into Tailscale (CGNAT 100.64.0.0/10, which is
/// what Tailscale hands out), regular LAN (RFC1918), or loopback.
/// Public non-RFC1918 / non-CGNAT addresses are rare on an `aoe serve`
/// host (would mean serving directly on the open internet) and fall
/// through to `Lan` so we still surface them.
pub fn classify_ip(ip: std::net::Ipv4Addr) -> IpKind {
    let octets = ip.octets();
    if ip.is_loopback() {
        return IpKind::Loopback;
    }
    // CGNAT 100.64.0.0/10 (RFC 6598). Second octet is 64..=127.
    if octets[0] == 100 && (64..=127).contains(&octets[1]) {
        return IpKind::Tailscale;
    }
    IpKind::Lan
}

/// Discover non-loopback IPv4 addresses on all network interfaces,
/// tagged by kind and sorted so the preferred URL (Tailscale > LAN)
/// is first. Caller decides whether to include loopback.
pub fn discover_tagged_ips() -> Vec<(IpKind, std::net::Ipv4Addr)> {
    let mut out: Vec<(IpKind, std::net::Ipv4Addr)> = Vec::new();
    if let Ok(addrs) = nix::ifaddrs::getifaddrs() {
        for ifaddr in addrs {
            if let Some(addr) = ifaddr.address {
                if let Some(sockaddr) = addr.as_sockaddr_in() {
                    let ip = sockaddr.ip();
                    if ip.is_loopback() {
                        continue;
                    }
                    if !out.iter().any(|(_, existing)| *existing == ip) {
                        out.push((classify_ip(ip), ip));
                    }
                }
            }
        }
    }
    out.sort_by_key(|(k, _)| *k);
    out
}

/// Write a file with owner-only permissions (0600) to protect secrets.
#[cfg(unix)]
async fn write_secret_file(path: &std::path::Path, contents: &str) {
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
async fn write_secret_file(path: &std::path::Path, contents: &str) {
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
fn is_valid_token_format(token: &str) -> bool {
    let len = token.len();
    (len == 64 || len == 32)
        && token
            .chars()
            .all(|c| c.is_ascii_hexdigit() || c.is_ascii_lowercase())
}

/// Load an existing auth token from disk if it's less than 24 hours old,
/// otherwise generate a fresh one and persist it.
async fn load_or_generate_token() -> anyhow::Result<String> {
    let app_dir = crate::session::get_app_dir()?;
    let token_path = app_dir.join("serve.token");

    // Try to reuse existing token if fresh enough
    if let Ok(metadata) = tokio::fs::metadata(&token_path).await {
        if let Ok(modified) = metadata.modified() {
            let age = std::time::SystemTime::now()
                .duration_since(modified)
                .unwrap_or_default();
            if age < std::time::Duration::from_secs(24 * 60 * 60) {
                if let Ok(token) = tokio::fs::read_to_string(&token_path).await {
                    let token = token.trim().to_string();
                    if !token.is_empty() && is_valid_token_format(&token) {
                        return Ok(token);
                    }
                }
            }
        }
    }

    let token = generate_token();
    write_secret_file(&token_path, &token).await;
    Ok(token)
}

/// Load sessions from all profiles, matching the TUI's "all profiles" view.
fn load_all_instances(file_watch: &Arc<FileWatchService>) -> anyhow::Result<Vec<Instance>> {
    let profiles = match crate::session::list_profiles() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                target: "server.file_watch",
                error = %e,
                "list_profiles failed; load_all_instances returning empty set"
            );
            return Ok(Vec::new());
        }
    };
    let mut all = Vec::new();
    for profile in &profiles {
        match Storage::new(profile, file_watch.clone()).and_then(|s| s.load()) {
            Ok(mut instances) => {
                for inst in &mut instances {
                    inst.source_profile = profile.clone();
                }
                all.extend(instances);
            }
            Err(e) => {
                tracing::warn!(
                    target: "server.file_watch",
                    profile = %profile,
                    error = %e,
                    "load_all_instances skipped profile; sessions for this profile will be \
                     absent from state until next successful reload"
                );
            }
        }
    }
    Ok(all)
}

/// Carry over the in-memory-only fields from the prior `state.instances`
/// entry into the freshly-loaded one. These fields are `#[serde(skip)]`
/// on `Instance` and would otherwise be reset to default every 2 s when
/// `status_poll_loop` reloads from disk. Adding a new `#[serde(skip)]`
/// field on `Instance` requires extending this function or the field is
/// silently wiped on every poll tick.
fn merge_runtime_fields(prior: Instance, mut fresh: Instance) -> Instance {
    fresh.last_error_check = prior.last_error_check;
    fresh.last_start_time = prior.last_start_time;
    // Only preserve `last_error` while the session is still in Error. A healthy
    // `fresh` clears it in `update_status_with_metadata_inner`; carrying the
    // prior string over unconditionally would re-stick a stale error on a now-green
    // session every poll tick when a healthy transition happened through a path that
    // did not explicitly null `last_error` in-memory (issue #1271).
    if fresh.status == Status::Error {
        fresh.last_error = prior.last_error;
    }
    fresh.session_id_poller = prior.session_id_poller;
    fresh.retroactive_capture_excludes = prior.retroactive_capture_excludes;
    fresh
}

// INVARIANTS for `reload_state_instances_from_disk` (do not break without
// revisiting `tests/serve_disk_reload_helper_equivalence.rs`):
// 1. Both call sites (`status_poll_loop` and `disk_watcher_consumer`) must
//    invoke this helper. They differ in cadence, in what they do BEFORE
//    calling it (tmux scrape lives only in `status_poll_loop`), and in
//    the StatusSource they pass.
// 2. `merge_runtime_fields` is mandatory per-id. Skipping it wipes the
//    five #[serde(skip)] runtime fields (`last_error_check`,
//    `last_start_time`, `last_error`, `session_id_poller`,
//    `retroactive_capture_excludes`) that disk reload zeroes by design.
// 3. `merge_runtime_fields` does NOT carry `status`, `last_accessed_at`,
//    or `idle_entered_at`. Those three are handled per StatusSource:
//    DiskOnly takes prior.status and `prior.idle_entered_at.or(fresh.idle_entered_at)`,
//    TmuxApplied takes fresh's. `last_accessed_at` is monotonic-max
//    regardless.
// 4. The acp overlay filter is `inst.is_structured()`, never the lazy
//    ACP session id. The latter is set lazily by the ACP handshake
//    and is None for newly-spawned acp sessions; using it as the
//    filter would silently drop overlay coverage for pre-handshake
//    rows.
// 5. `prior_by_id` is built with `.drain(..)` once, then read with
//    `.get()` rather than `.remove()` in the merge loop, so the same map is
//    still populated when `apply_acp_overlay_inplace` runs.
// 6. Polling is canonical. The watcher path
//    adds latency reduction; correctness still holds when it fails.
// 7. `status_poll_loop` and `disk_watcher_consumer` may interleave
//    per-tick; both serialise on `state.instances.write().await`. A
//    DiskOnly merge between a TmuxApplied write and a subsequent tmux
//    scrape can briefly carry the prior status; it self-corrects on
//    the next 2s tick. Polling is canonical (invariant 6) so this is
//    acceptable.

/// Reload `state.instances` by merging caller-supplied `fresh` against the
/// prior in-memory snapshot per id, then reapplying the acp overlay.
/// The caller is responsible for the disk read and, on the
/// `TmuxApplied` path only, for emitting `state.status_tx`
/// diffs BEFORE invoking the helper.
/// Snapshot of the prior in-memory `state.instances` keyed by id, used
/// for per-id merging in `reload_state_instances_from_disk` and the
/// acp-overlay pass. Intentionally exposes only `drain_from` and `get`;
/// no `remove` method, because invariant 5 of the merge contract
/// requires the same map to be populated when
/// `apply_acp_overlay_inplace` runs after the merge loop. The compiler
/// rejects any future `.remove()` call instead of relying on prose.
struct PriorById(std::collections::HashMap<String, Instance>);

impl PriorById {
    fn drain_from(current: &mut Vec<Instance>) -> Self {
        Self(
            current
                .drain(..)
                .map(|inst| (inst.id.clone(), inst))
                .collect(),
        )
    }

    fn get(&self, id: &str) -> Option<&Instance> {
        self.0.get(id)
    }
}

#[doc(hidden)]
pub(crate) async fn reload_state_instances_from_disk(
    state: &Arc<AppState>,
    fresh: Vec<Instance>,
    status_source: StatusSource,
) {
    // Snapshot suppression here so a worker that unmarks between the
    // caller's input build and the per-id decision cannot combine a
    // cleared mark with a stale row to re-emit the phantom Error
    // transition the suppression exists to prevent. Idempotent on the
    // poll path, where the caller already applied the same override
    // inside `spawn_blocking`.
    let suppressed_ids =
        crate::session::recovery::snapshot_recently_restarted(&state.recently_restarted);

    let mut current = state.instances.write().await;
    let prior_by_id = PriorById::drain_from(&mut current);

    let mut merged: Vec<Instance> = Vec::with_capacity(fresh.len());
    for mut row in fresh {
        if let Some(prior) = prior_by_id.get(&row.id).cloned() {
            let prior_status = prior.status;
            let prior_last_accessed = prior.last_accessed_at;
            let prior_idle_entered = prior.idle_entered_at;
            row = merge_runtime_fields(prior, row);
            match status_source {
                StatusSource::DiskOnly => {
                    row.status = prior_status;
                    row.idle_entered_at = prior_idle_entered.or(row.idle_entered_at);
                }
                StatusSource::TmuxApplied => {
                    // Caller already applied tmux scrape to fresh.status;
                    // that is the authoritative value. idle_entered_at is
                    // recomputed by upstream status-transition logic;
                    // trust fresh.
                }
            }
            row.last_accessed_at = prior_last_accessed.max(row.last_accessed_at);
        }
        if suppressed_ids.contains(&row.id) {
            row.status = Status::Starting;
        }
        merged.push(row);
    }

    #[cfg(feature = "serve")]
    apply_acp_overlay_inplace(&prior_by_id, &mut merged);

    *current = merged;
}

/// Apply the acp status / timestamps overlay to `merged`, sourcing
/// values from `prior_by_id`. The merge loop above uses `.get()` (NOT
/// `.remove()`), so this lookup still finds entries here. Filter is
/// `inst.is_structured()` per the invariant above; filtering on
/// the lazy session id would silently drop overlay coverage for
/// pre-handshake rows.
#[cfg(feature = "serve")]
fn apply_acp_overlay_inplace(prior_by_id: &PriorById, merged: &mut [Instance]) {
    for inst in merged.iter_mut() {
        if !inst.is_structured() {
            continue;
        }
        let Some(prior) = prior_by_id.get(&inst.id) else {
            continue;
        };
        inst.status = prior.status;
        inst.last_accessed_at = prior.last_accessed_at;
        inst.idle_entered_at = prior.idle_entered_at;
    }
}

/// Build a per-profile disk-watch entry: register a `subscribe_channel`
/// against `<profile_dir>/{sessions,groups}.json` and spawn a forwarder
/// task that drains the receiver into `state.disk_changed`. Returns
/// `None` when the profile dir cannot be resolved or `subscribe_channel`
/// fails; both cases are logged. Polling stays canonical, so a `None`
/// here degrades propagation to the 2s tick rather than failing closed.
async fn build_disk_watch_entry(state: &Arc<AppState>, profile: &str) -> Option<DiskWatchEntry> {
    let profile_dir = match crate::session::get_profile_dir_path(profile) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                target: "server.file_watch",
                profile = %profile,
                error = %e,
                "could not resolve profile dir; live propagation disabled"
            );
            return None;
        }
    };
    let sessions_path = profile_dir.join("sessions.json");
    let groups_path = profile_dir.join("groups.json");
    let spec = WatchSpec {
        dir: profile_dir,
        matcher: FileMatcher::AnyOf(vec![sessions_path, groups_path]),
        debounce: Some(std::time::Duration::from_millis(75)),
    };
    let (mut rx, handle) = match state.file_watch.subscribe_channel(spec, 16) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                target: "server.file_watch",
                profile = %profile,
                error = %e,
                "subscribe_channel failed; live propagation disabled for this profile"
            );
            return None;
        }
    };
    let signal = state.disk_changed.clone();
    let profile = profile.to_owned();
    let shutdown = state.shutdown.clone();
    let join = crate::task_util::spawn_supervised(
        "server.disk_watch.forwarder",
        crate::task_util::PanicPolicy::Log,
        async move {
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    ev = rx.recv() => match ev {
                        Some(_) => signal.notify_one(),
                        None => break,
                    }
                }
            }
            tracing::debug!(
                target: "server.file_watch",
                profile = %profile,
                "disk-watch forwarder exit"
            );
        },
    );
    // Test-only barrier: when armed, signals `entered` after the
    // subscription is built and parks on `release`. The enclosing
    // `add_profile_disk_watch` / `rename_profile_disk_watch` hold `disk_watch_handles` through the
    // build, so a task parked here also holds that lock; this lets a
    // controlled-ordering test drive a concurrent same-profile remove
    // against a known mid-build state.
    #[cfg(any(test, feature = "test-support"))]
    {
        let armed = disk_watch_build_barrier().lock().unwrap().clone();
        if let Some(barrier) = armed {
            barrier.entered.notify_one();
            barrier.release.notified().await;
        }
    }
    Some(DiskWatchEntry {
        handle,
        forwarder: join.abort_handle(),
    })
}

/// Test-only barrier installed inside `build_disk_watch_entry` to
/// deterministically pin a building task at a known point so a
/// concurrent same-profile remove can run against it. Not compiled
/// into production builds.
#[cfg(any(test, feature = "test-support"))]
pub(crate) struct DiskWatchBuildBarrier {
    pub(crate) entered: tokio::sync::Notify,
    pub(crate) release: tokio::sync::Notify,
    #[cfg(test)]
    pub(crate) armed: tokio::sync::Notify,
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn disk_watch_build_barrier(
) -> &'static std::sync::Mutex<Option<Arc<DiskWatchBuildBarrier>>> {
    static BARRIER: std::sync::OnceLock<std::sync::Mutex<Option<Arc<DiskWatchBuildBarrier>>>> =
        std::sync::OnceLock::new();
    BARRIER.get_or_init(|| std::sync::Mutex::new(None))
}

/// RAII guard for the test barrier slot: installs on construction and
/// clears unconditionally on drop, so a panicking test cannot leave
/// the slot armed for subsequent tests in the same process.
#[cfg(test)]
pub(crate) struct DiskWatchBuildBarrierGuard;

#[cfg(test)]
impl DiskWatchBuildBarrierGuard {
    pub(crate) fn install(barrier: Arc<DiskWatchBuildBarrier>) -> Self {
        *disk_watch_build_barrier().lock().unwrap() = Some(barrier);
        Self
    }
}

#[cfg(test)]
impl Drop for DiskWatchBuildBarrierGuard {
    fn drop(&mut self) {
        *disk_watch_build_barrier().lock().unwrap() = None;
    }
}

/// Drop the subscription handle FIRST so the dispatcher stops queuing
/// events on this id, then abort the forwarder; aborting first would
/// race a buffered `try_send`. Centralized so every teardown path keeps
/// the same canonical order.
fn drop_disk_watch_entry(entry: DiskWatchEntry) {
    let DiskWatchEntry { handle, forwarder } = entry;
    drop(handle);
    forwarder.abort();
}

/// Install a disk-watch subscription for `profile` under one critical
/// section. If a prior entry exists for the same name, it is replaced
/// (drop handle, abort forwarder, then install the new entry).
///
/// Holding `disk_watch_handles` across `build_disk_watch_entry` is the
/// linearisation point: a concurrent `remove_profile_disk_watch` for
/// the same name cannot interleave between "subscription created" and
/// "entry installed" and silently leave a stale watcher behind for a
/// profile that was just removed.
pub(crate) async fn add_profile_disk_watch(state: &Arc<AppState>, profile: &str) {
    let mut handles = state.disk_watch_handles.lock().await;
    let Some(entry) = build_disk_watch_entry(state, profile).await else {
        return;
    };
    if let Some(prior) = handles.remove(profile) {
        drop_disk_watch_entry(prior);
    }
    handles.insert(profile.to_owned(), entry);
    tracing::debug!(
        target: "server.file_watch",
        profile = %profile,
        op = "add",
        "disk-watch subscription registered"
    );
}

/// Remove the disk-watch subscription for `profile` (no-op if absent).
pub(crate) async fn remove_profile_disk_watch(state: &Arc<AppState>, profile: &str) {
    let mut handles = state.disk_watch_handles.lock().await;
    if let Some(entry) = handles.remove(profile) {
        drop_disk_watch_entry(entry);
        tracing::debug!(
            target: "server.file_watch",
            profile = %profile,
            op = "remove",
            "disk-watch subscription removed"
        );
    }
}

/// Swap the disk-watch subscription from `old` to `new` under one
/// critical section. Concurrent same-name add/remove cannot interleave
/// between the two halves; this is concurrent-atomic, not
/// failure-atomic. On `build_disk_watch_entry` failure the `old` entry
/// is still removed because the production caller (rename_profile) has
/// already moved the on-disk directory and the old kernel watch points
/// at a path that no longer exists.
pub(crate) async fn rename_profile_disk_watch(state: &Arc<AppState>, old: &str, new: &str) {
    if old == new {
        return;
    }
    let mut handles = state.disk_watch_handles.lock().await;
    if let Some(entry) = handles.remove(old) {
        drop_disk_watch_entry(entry);
    }
    let Some(entry) = build_disk_watch_entry(state, new).await else {
        return;
    };
    if let Some(prior) = handles.remove(new) {
        drop_disk_watch_entry(prior);
    }
    handles.insert(new.to_owned(), entry);
    tracing::debug!(
        target: "server.file_watch",
        old = %old,
        new = %new,
        op = "rename",
        "disk-watch subscription renamed"
    );
}

/// Wire up disk-watch subscriptions for every currently-active profile.
/// Called during startup before request serving begins so the initial
/// watcher set is in place before any handler mutates storage. Per-profile
/// `subscribe_channel` errors are logged and skipped; polling stays
/// canonical so propagation degrades to the 2s tick rather than failing
/// closed. Emits one bootstrap wake at the end so any write that landed
/// while we were walking the profile list is reconciled immediately once
/// the consumer begins awaiting `disk_changed`.
pub(crate) async fn init_disk_watch_subscriptions(state: Arc<AppState>) {
    init_disk_watch_subscriptions_inner(state, |_: &str| {}, false).await;
}

/// Test-only variant that runs `hook` after each profile's subscription
/// is installed, so a test can drive disk writes between iterations to
/// exercise the bootstrap reconciliation path.
#[cfg(test)]
async fn init_disk_watch_subscriptions_with_hook<F>(state: Arc<AppState>, hook: F)
where
    F: FnMut(&str) + Send,
{
    init_disk_watch_subscriptions_inner(state, hook, true).await;
}

async fn init_disk_watch_subscriptions_inner<F>(state: Arc<AppState>, mut hook: F, with_hook: bool)
where
    F: FnMut(&str) + Send,
{
    let profiles = crate::session::list_profiles().unwrap_or_default();
    let count = profiles.len();
    for profile in &profiles {
        add_profile_disk_watch(&state, profile).await;
        hook(profile);
    }
    state.disk_changed.notify_one();
    let suffix = if with_hook { " (with hook)" } else { "" };
    tracing::info!(
        target: "server.file_watch",
        profiles_count = count,
        "disk-watch subscriptions initialized{suffix}",
    );
}

/// Background task: reload `state.instances` from disk on every wake of
/// `state.disk_changed`. Mirrors `status_poll_loop`'s lock-acquisition
/// pattern but does NOT touch tmux or `state.status_tx`. Polling stays
/// canonical; this task is pure latency reduction.
async fn disk_watcher_consumer(state: Arc<AppState>) {
    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => break,
            _ = state.disk_changed.notified() => {}
        }
        let started = std::time::Instant::now();
        let file_watch_for_load = state.file_watch.clone();
        let fresh =
            match tokio::task::spawn_blocking(move || load_all_instances(&file_watch_for_load))
                .await
            {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => {
                    tracing::warn!(
                        target: "server.file_watch",
                        error = %e,
                        "disk reload failed"
                    );
                    continue;
                }
                Err(e) => {
                    tracing::warn!(
                        target: "server.file_watch",
                        error = %e,
                        "spawn_blocking joined with error"
                    );
                    continue;
                }
            };
        let count = fresh.len();
        reload_state_instances_from_disk(&state, fresh, StatusSource::DiskOnly).await;
        tracing::trace!(
            target: "server.file_watch",
            latency_us = started.elapsed().as_micros() as u64,
            instance_count = count,
            "disk reload completed"
        );
    }
}

/// Background task: emit an opt-in telemetry `usage_snapshot` immediately and
/// every ~4 hours (jittered), plus a final one on graceful shutdown. The boot
/// `process_start` is emitted separately by the caller before transport setup.
/// All sends are best-effort and swallow errors; nothing leaves the box unless
/// the user opted in and an endpoint is configured.
fn spawn_serve_snapshot_loop(state: Arc<AppState>) {
    tokio::spawn(async move {
        // Jittered period (4h + up to 30m) so installs that boot together don't
        // snapshot in lockstep; the first tick is still immediate (boot
        // snapshot). `Delay` avoids a burst of catch-up ticks after a stall.
        let mut interval = tokio::time::interval(crate::telemetry::snapshot_interval());
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Sample the live session list more often than we send, folding each
        // sample into a window aggregate so short-lived sessions' agent/model
        // mix and the concurrency peak survive into the periodic snapshot (#1870).
        // Both tickers share this one task, so a sample tick and a flush tick
        // never run concurrently: the aggregate needs no locking and a plain
        // reset after a confirmed send is race-free. `Skip` so a long suspend
        // does not fire a run of catch-up samples on wake.
        let mut sample = tokio::time::interval(std::time::Duration::from_secs(30 * 60));
        sample.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut aggregator = crate::telemetry::aggregate::UsageAggregator::default();
        loop {
            tokio::select! {
                _ = state.shutdown.cancelled() => {
                    // Deduped: a serve process that starts and stops between
                    // periodic ticks would otherwise emit the initial first-tick
                    // snapshot and an identical shutdown snapshot seconds apart.
                    // We exit after this, so the aggregate is dropped; no reset.
                    if let Some(snapshot) = build_serve_snapshot(&state, &mut aggregator).await {
                        let outcome = crate::telemetry::flush_snapshot_if_changed(snapshot).await;
                        clear_reported_serve_signals(&state, outcome);
                    }
                    break;
                }
                _ = interval.tick() => {
                    if let Some(snapshot) = build_serve_snapshot(&state, &mut aggregator).await {
                        // Awaited (not detached) so the reported signals are
                        // cleared only after a confirmed send. A failed send
                        // retains the usage_seen counts / the create counter
                        // for the next snapshot instead of dropping them.
                        let outcome = if crate::telemetry::send_snapshot(snapshot).await {
                            crate::telemetry::SendOutcome::Sent
                        } else {
                            crate::telemetry::SendOutcome::Failed
                        };
                        clear_reported_serve_signals(&state, outcome);
                        // Reset the window only after a confirmed send, mirroring
                        // the signal-clear discipline: a failed send keeps the
                        // aggregate so the next flush re-reports the full window.
                        if outcome == crate::telemetry::SendOutcome::Sent {
                            aggregator = crate::telemetry::aggregate::UsageAggregator::default();
                        }
                    }
                }
                _ = sample.tick() => {
                    let instances = state.instances.read().await.clone();
                    aggregator.sample(&instances);
                }
            }
        }
    });
}

/// Per-form-factor open counters for one web surface (dashboard or acp).
/// A fixed, lock-free set over the closed [`crate::telemetry::WebClientFormFactor`]
/// allowlist: the seen endpoint increments the matching class, the snapshot
/// reads exact counts, and a confirmed send decrements by exactly what it
/// reported (so an open landing during an in-flight send survives, mirroring
/// the coarse `telemetry_web_seen` counter). Named fields rather than a map so
/// no free-form string key can ever enter daemon state.
#[derive(Default)]
pub struct FormFactorCounters {
    desktop: std::sync::atomic::AtomicU32,
    desktop_pwa: std::sync::atomic::AtomicU32,
    mobile: std::sync::atomic::AtomicU32,
    mobile_pwa: std::sync::atomic::AtomicU32,
}

impl FormFactorCounters {
    fn field(&self, ff: crate::telemetry::WebClientFormFactor) -> &std::sync::atomic::AtomicU32 {
        use crate::telemetry::WebClientFormFactor::*;
        match ff {
            Desktop => &self.desktop,
            DesktopPwa => &self.desktop_pwa,
            Mobile => &self.mobile,
            MobilePwa => &self.mobile_pwa,
        }
    }

    /// Record one classified open of the given client class.
    pub fn increment(&self, ff: crate::telemetry::WebClientFormFactor) {
        self.field(ff)
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Point-in-time read of every class count, so the snapshot loop can later
    /// decrement by exactly the values it reported.
    fn read(&self) -> FormFactorCounts {
        use std::sync::atomic::Ordering;
        let mut counts = FormFactorCounts::default();
        for ff in crate::telemetry::WebClientFormFactor::ALL {
            counts.set(ff, self.field(ff).load(Ordering::Relaxed));
        }
        counts
    }

    /// Subtract exactly the reported counts after a confirmed send. Never zeroes,
    /// so an open that landed mid-send rolls into the next snapshot.
    fn decrement(&self, reported: &FormFactorCounts) {
        for ff in crate::telemetry::WebClientFormFactor::ALL {
            let n = reported.get(ff);
            if n > 0 {
                self.field(ff)
                    .fetch_sub(n, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
}

/// A snapshot's reported per-class counts. Plain values (not atomics) so they
/// can be stashed in [`ReportedServeSignals`] and replayed on confirm.
#[derive(Default, Clone, Copy)]
struct FormFactorCounts {
    desktop: u32,
    desktop_pwa: u32,
    mobile: u32,
    mobile_pwa: u32,
}

impl FormFactorCounts {
    fn slot(&mut self, ff: crate::telemetry::WebClientFormFactor) -> &mut u32 {
        use crate::telemetry::WebClientFormFactor::*;
        match ff {
            Desktop => &mut self.desktop,
            DesktopPwa => &mut self.desktop_pwa,
            Mobile => &mut self.mobile,
            MobilePwa => &mut self.mobile_pwa,
        }
    }

    fn set(&mut self, ff: crate::telemetry::WebClientFormFactor, n: u32) {
        *self.slot(ff) = n;
    }

    fn get(&self, ff: crate::telemetry::WebClientFormFactor) -> u32 {
        match ff {
            crate::telemetry::WebClientFormFactor::Desktop => self.desktop,
            crate::telemetry::WebClientFormFactor::DesktopPwa => self.desktop_pwa,
            crate::telemetry::WebClientFormFactor::Mobile => self.mobile,
            crate::telemetry::WebClientFormFactor::MobilePwa => self.mobile_pwa,
        }
    }

    /// Per-class was-seen map for the snapshot wire: only classes with a
    /// positive count appear, each as `true`. Empty (and so omitted) when no
    /// classified client opened the surface.
    fn seen_map(&self) -> std::collections::BTreeMap<String, bool> {
        let mut map = std::collections::BTreeMap::new();
        for ff in crate::telemetry::WebClientFormFactor::ALL {
            if self.get(ff) > 0 {
                map.insert(ff.key().to_string(), true);
            }
        }
        map
    }
}

/// Daemon-side structured-interaction tallies for the next opt-in snapshot. Each
/// is a monotonic `AtomicU32` consumed with the same decrement-by-reported
/// discipline as `telemetry_*_seen`, so an interaction that lands during an
/// in-flight send rolls into the next snapshot instead of being dropped.
///
/// `plan_mode_seen` is a counter rather than a flag for the same reason: the
/// snapshot reports the boolean `count > 0`, but consuming it by subtracting
/// the reported amount keeps a plan-mode entry that arrived mid-send.
#[derive(Default)]
pub struct StructuredTelemetryCounters {
    pub approvals_allow: std::sync::atomic::AtomicU32,
    pub approvals_allow_always: std::sync::atomic::AtomicU32,
    pub approvals_deny: std::sync::atomic::AtomicU32,
    pub agent_switches: std::sync::atomic::AtomicU32,
    pub plan_mode_seen: std::sync::atomic::AtomicU32,
    pub prompts_queued: std::sync::atomic::AtomicU32,
}

/// What a serve snapshot reported, so the originating signals can be cleared
/// only after the send is confirmed. The clear is deferred (rather than reset at
/// build time) so a failed send retains the signals for the next snapshot.
struct ReportedServeSignals {
    usage_seen: std::collections::BTreeMap<String, u32>,
    web_clients: FormFactorCounts,
    structured_clients: FormFactorCounts,
    session_creates: u32,
    acp: ReportedAcpCounts,
}

/// The raw `AtomicU32` values a snapshot folded in, kept so each can be
/// decremented by exactly the reported amount on a confirmed send. `plan_mode`
/// is the raw count (not the reported boolean) so a plan-mode entry that
/// arrived mid-send is preserved rather than wiped.
#[derive(Default, Clone, Copy)]
struct ReportedAcpCounts {
    approvals_allow: u32,
    approvals_allow_always: u32,
    approvals_deny: u32,
    agent_switches: u32,
    plan_mode: u32,
    prompts_queued: u32,
}

/// Build a serve `usage_snapshot` from the live session list, folding in the
/// `usage_seen` open counts and the session-create trend counter *without
/// resetting them*. The reported counts are stashed in `AppState` so
/// [`clear_reported_serve_signals`] can subtract exactly what was reported once
/// the send is confirmed. Returns `None` when telemetry is not opted in.
///
/// The live read is also folded into `aggregator` as the flush-moment sample,
/// then the window's peak concurrency and distinct-sessions-seen maps override
/// the point-in-time defaults `build_usage_snapshot` produced (#1870). The
/// point-in-time `session_total` and status/sandbox/yolo/acp counts keep
/// their instant-of-flush meaning.
async fn build_serve_snapshot(
    state: &AppState,
    aggregator: &mut crate::telemetry::aggregate::UsageAggregator,
) -> Option<crate::telemetry::UsageSnapshot> {
    use std::sync::atomic::Ordering;
    let usage_seen = state.telemetry_usage_seen.snapshot();
    let web_clients = state.telemetry_web_clients.read();
    let structured_clients = state.telemetry_structured_clients.read();
    let session_creates = state.telemetry_session_creates.load(Ordering::Relaxed);
    let c = &state.telemetry_structured;
    let reported_acp = ReportedAcpCounts {
        approvals_allow: c.approvals_allow.load(Ordering::Relaxed),
        approvals_allow_always: c.approvals_allow_always.load(Ordering::Relaxed),
        approvals_deny: c.approvals_deny.load(Ordering::Relaxed),
        agent_switches: c.agent_switches.load(Ordering::Relaxed),
        plan_mode: c.plan_mode_seen.load(Ordering::Relaxed),
        prompts_queued: c.prompts_queued.load(Ordering::Relaxed),
    };
    let acp = crate::telemetry::StructuredInteractionCounts {
        approvals_allow: reported_acp.approvals_allow,
        approvals_allow_always: reported_acp.approvals_allow_always,
        approvals_deny: reported_acp.approvals_deny,
        agent_switches: reported_acp.agent_switches,
        plan_mode_seen: reported_acp.plan_mode > 0,
        prompts_queued: reported_acp.prompts_queued,
    };
    let instances = state.instances.read().await.clone();
    aggregator.sample(&instances);
    let mut snapshot = crate::telemetry::build_usage_snapshot(
        crate::telemetry::Surface::Serve,
        &instances,
        usage_seen.clone(),
        session_creates,
        Some(state.auth_mode),
        Some(state.serve_mode),
        &acp,
    )?;
    // Layer the per-form-factor was-seen maps onto the snapshot. They are serve
    // only (the browser surfaces), so the pure builder leaves them empty and the
    // daemon fills them here from its client counters.
    snapshot.web_clients_seen = web_clients.seen_map();
    snapshot.structured_clients_seen = structured_clients.seen_map();
    snapshot.peak_concurrent_sessions = aggregator.peak_concurrent_sessions();
    snapshot.distinct_sessions_by_agent = aggregator.distinct_by_agent();
    snapshot.distinct_sessions_by_model_bucket = aggregator.distinct_by_model();
    *state.telemetry_last_reported.lock().unwrap() = Some(ReportedServeSignals {
        usage_seen,
        web_clients,
        structured_clients,
        session_creates,
        acp: reported_acp,
    });
    Some(snapshot)
}

/// Clear the signals a serve snapshot reported, but only when the send was
/// confirmed (`SendOutcome::Sent`). On `Deduped` the prior confirmed send
/// already cleared them; on `Failed` they are retained so the next snapshot
/// re-reports them. Every signal (the `usage_seen` open counts and the create
/// counter) is decremented by exactly what was reported, not reset to 0, so an
/// open or a create that landed during the in-flight send survives into the
/// next snapshot instead of being cleared away.
fn clear_reported_serve_signals(state: &AppState, outcome: crate::telemetry::SendOutcome) {
    let Some(reported) = state.telemetry_last_reported.lock().unwrap().take() else {
        return;
    };
    if outcome != crate::telemetry::SendOutcome::Sent {
        return;
    }
    state.telemetry_usage_seen.decrement(&reported.usage_seen);
    state.telemetry_web_clients.decrement(&reported.web_clients);
    state
        .telemetry_structured_clients
        .decrement(&reported.structured_clients);
    decrement_reported_count(&state.telemetry_session_creates, reported.session_creates);
    let c = &state.telemetry_structured;
    let rc = reported.acp;
    decrement_reported_count(&c.approvals_allow, rc.approvals_allow);
    decrement_reported_count(&c.approvals_allow_always, rc.approvals_allow_always);
    decrement_reported_count(&c.approvals_deny, rc.approvals_deny);
    decrement_reported_count(&c.agent_switches, rc.agent_switches);
    decrement_reported_count(&c.plan_mode_seen, rc.plan_mode);
    decrement_reported_count(&c.prompts_queued, rc.prompts_queued);
}

/// Decrement a reported telemetry counter by exactly `reported`, never by more.
/// Subtracting the reported amount rather than `swap(0)` preserves any
/// increments (a create, or a web/acp open, or an acp interaction) that
/// landed between the snapshot build and the confirmed send, so they roll into
/// the next snapshot instead of being dropped. A no-op when nothing was
/// reported.
///
/// The snapshot loop is the sole consumer and runs strictly sequentially (each
/// send is awaited, then cleared, before the next build), so the counter can
/// never go below `reported`. The subtraction saturates anyway as cheap
/// insurance against a future refactor that detaches sends, which would
/// otherwise be able to underflow-wrap the `AtomicU32`.
fn decrement_reported_count(counter: &std::sync::atomic::AtomicU32, reported: u32) {
    if reported == 0 {
        return;
    }
    use std::sync::atomic::Ordering;
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_sub(reported))
    });
}

/// Background task that periodically refreshes session statuses. On each
/// tick, diffs pre- and post-refresh statuses and emits a `StatusChange`
/// on `state.status_tx` for every transition. Keeping the diff here,
/// rather than pushing it into `Instance::update_status_with_metadata`,
/// leaves the session module free of any broadcast-channel dependency
/// and keeps TUI/CLI callers unchanged.
async fn status_poll_loop(state: Arc<AppState>) {
    // `Delay` re-arms the next tick `period` after the current one returns,
    // so a stall (suspend, scheduler stall, flock contention) does not drain
    // queued ticks and collapse the 2s cooldown the per-tick work expects.
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    #[cfg(feature = "serve")]
    let mut attempted_acp_spawns: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    #[cfg(feature = "serve")]
    let mut last_idle_reap: Option<std::time::Instant> = None;
    #[cfg(feature = "serve")]
    let mut last_session_idle_reap: Option<std::time::Instant> = None;
    #[cfg(feature = "serve")]
    let mut last_rate_limit_reap: Option<std::time::Instant> = None;
    // Per-session reconciler respawn budget + crash-loop park set (#1945).
    // Owned by the loop so they persist across ticks, swept against live
    // sessions inside the reconciler.
    #[cfg(feature = "serve")]
    let mut acp_respawn_history: std::collections::HashMap<String, Vec<std::time::Instant>> =
        std::collections::HashMap::new();
    #[cfg(feature = "serve")]
    let mut acp_parked: std::collections::HashSet<String> = std::collections::HashSet::new();
    loop {
        interval.tick().await;

        let prev: std::collections::HashMap<String, crate::session::Status> = {
            let instances = state.instances.read().await;
            instances.iter().map(|i| (i.id.clone(), i.status)).collect()
        };

        // Snapshot suppression BEFORE `batch_pane_metadata()` so a worker
        // that unmarks between the scrape and the per-instance decision
        // cannot combine "pane missing" metadata with a cleared mark and
        // re-emit the phantom Error transition the suppression exists to
        // prevent.
        let suppressed_ids =
            crate::session::recovery::snapshot_recently_restarted(&state.recently_restarted);
        let file_watch_for_poll = state.file_watch.clone();
        let updated = tokio::task::spawn_blocking(move || {
            let mut instances = load_all_instances(&file_watch_for_poll).unwrap_or_default();
            crate::tmux::refresh_session_cache();
            let pane_metadata = crate::tmux::batch_pane_metadata().unwrap_or_default();
            for inst in &mut instances {
                if suppressed_ids.contains(&inst.id) {
                    inst.status = Status::Starting;
                    continue;
                }
                let session_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
                let metadata = pane_metadata.get(&session_name);
                inst.update_status_with_metadata(metadata);
            }
            instances
        })
        .await;

        if let Ok(mut instances) = updated {
            // Diff BEFORE the helper: status_tx must observe the raw
            // post-suppression, post-tmux-scrape values, never the acp
            // overlay applied by the helper.
            let now = chrono::Utc::now();
            for inst in &instances {
                if let Some(old) = prev.get(&inst.id) {
                    if *old != inst.status {
                        let _ = state.status_tx.send(StatusChange {
                            instance_id: inst.id.clone(),
                            instance_title: inst.title.clone(),
                            old: *old,
                            new: inst.status,
                            at: now,
                        });
                    }
                }
            }

            // Auto-mark unread on a finished turn (Running -> Idle), the same
            // transition the TUI marks on. This is what lets a web-only user
            // (no TUI process polling) accrue the indicator. There's no
            // server-side "is being viewed" exemption: the client suppresses
            // the chip on the session it is actively viewing and clears the
            // auto marker on open. Mutate the in-memory rows we're about to
            // install AND persist per profile so the next disk reload keeps it.
            if crate::session::unread_enabled() {
                let mut newly_idle: std::collections::HashMap<String, Vec<String>> =
                    std::collections::HashMap::new();
                for inst in &mut instances {
                    let finished_turn = prev.get(&inst.id) == Some(&Status::Running)
                        && inst.status == Status::Idle
                        && !inst.unread;
                    if finished_turn {
                        inst.mark_unread();
                        newly_idle
                            .entry(inst.source_profile.clone())
                            .or_default()
                            .push(inst.id.clone());
                    }
                }
                for (profile, ids) in newly_idle {
                    let _ = api::persist_session_update(
                        profile,
                        "auto-unread",
                        state.file_watch.clone(),
                        move |insts| {
                            for inst in insts.iter_mut() {
                                if ids.contains(&inst.id) {
                                    inst.mark_unread();
                                }
                            }
                        },
                    )
                    .await;
                }
            }

            reload_state_instances_from_disk(&state, instances, StatusSource::TmuxApplied).await;

            // Drain poller observations into sessions.json so daemon-only
            // sessions (no attached TUI) persist post-`/clear` sids (#2291).
            // Snapshot + spawn_blocking + reapply, never holding AppState
            // across the flock or tmux exec, per storage.rs:46.
            let snapshot = state.instances.read().await.clone();
            let drain_state = state.clone();
            match tokio::task::spawn_blocking(move || {
                let mut snapshot = snapshot;
                let outcome = crate::session::sync::drain_and_persist_session_ids(
                    &mut snapshot,
                    &drain_state.file_watch,
                );
                (outcome, snapshot)
            })
            .await
            {
                Ok((outcome, mutated)) if outcome.touched() => {
                    // Reapply only for ids the helper actually touched, so a
                    // peer that wrote `agent_session_id` (e.g. the restart-
                    // completion path) on the live state during the
                    // spawn_blocking window is not silently reverted.
                    let touched: std::collections::HashSet<&str> = outcome
                        .applied
                        .iter()
                        .chain(outcome.rolled_back.iter())
                        .map(String::as_str)
                        .collect();
                    if !touched.is_empty() {
                        let mut guard = state.instances.write().await;
                        for src in mutated.iter().filter(|i| touched.contains(i.id.as_str())) {
                            if let Some(dst) = guard.iter_mut().find(|i| i.id == src.id) {
                                dst.agent_session_id = src.agent_session_id.clone();
                                dst.resume_probe_failed_sid = src.resume_probe_failed_sid.clone();
                            }
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::error!(
                        target: "session.sync",
                        "drain_and_persist task failed: {e}",
                    );
                }
            }

            #[cfg(feature = "serve")]
            acp_reconciler::reconcile_acp_workers(
                &state,
                &mut attempted_acp_spawns,
                &mut last_idle_reap,
                &mut last_rate_limit_reap,
                &mut acp_respawn_history,
                &mut acp_parked,
            )
            .await;

            #[cfg(feature = "serve")]
            reap_idle_sessions(&state, &mut last_session_idle_reap).await;
        }
    }
}

/// How often the serve daemon evaluates plain tmux sessions for idle
/// auto-stop. Mirrors the acp reaper's cadence so a 2s status tick does
/// not drive a storage + tmux sweep on every iteration.
#[cfg(feature = "serve")]
const SESSION_IDLE_REAP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// Cap on concurrent `perform_stop` calls during one reap pass. `Instance::stop`
/// can block ~10s on `docker stop`; without a bound, a fleet of sessions all
/// crossing the threshold on the same tick would stampede the Docker daemon.
#[cfg(feature = "serve")]
const SESSION_IDLE_REAP_MAX_CONCURRENT: usize = 4;

/// Auto-stop plain (non-acp) tmux sessions that have been `Idle` past
/// their per-profile `session.auto_stop_idle_secs` (#1690). Gated to run at
/// most once per [`SESSION_IDLE_REAP_INTERVAL`]. Each candidate is claimed
/// under the per-profile storage lock (so a concurrently running TUI cannot
/// double-stop it) and stopped on a detached task with bounded concurrency,
/// keeping the status poll loop responsive.
#[cfg(feature = "serve")]
async fn reap_idle_sessions(state: &Arc<AppState>, last_reap: &mut Option<std::time::Instant>) {
    if last_reap.is_some_and(|t| t.elapsed() < SESSION_IDLE_REAP_INTERVAL) {
        return;
    }
    *last_reap = Some(std::time::Instant::now());

    // Live attach state. If the tmux query fails, skip this pass entirely
    // rather than risk reaping a session the user is attached to.
    let attached = match tokio::task::spawn_blocking(crate::tmux::attached_session_names).await {
        Ok(Ok(set)) => set,
        _ => return,
    };

    let now = chrono::Utc::now();
    let instances = { state.instances.read().await.clone() };

    // Resolve each distinct profile's threshold once, off the async runtime:
    // `resolve_config_or_warn` reads config files from disk, so building the
    // map directly here would block the poll loop.
    let profiles: Vec<String> = instances
        .iter()
        .filter(|inst| !inst.is_structured())
        .map(|inst| inst.effective_profile())
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .collect();
    let thresholds: std::collections::HashMap<String, u32> =
        tokio::task::spawn_blocking(move || {
            profiles
                .into_iter()
                .map(|p| {
                    let secs = crate::session::profile_config::resolve_config_or_warn(&p)
                        .session
                        .auto_stop_idle_secs;
                    (p, secs)
                })
                .collect()
        })
        .await
        .unwrap_or_default();

    let candidates =
        crate::session::idle_reap::idle_reap_candidates(&instances, now, &attached, |p| {
            thresholds.get(p).copied().unwrap_or(0)
        });

    let sem = Arc::new(tokio::sync::Semaphore::new(
        SESSION_IDLE_REAP_MAX_CONCURRENT,
    ));
    for cand in candidates {
        let sem = sem.clone();
        let file_watch = state.file_watch.clone();
        tokio::spawn(async move {
            let _permit = sem.acquire().await;
            let claim = {
                let cand = cand.clone();
                let file_watch = file_watch.clone();
                tokio::task::spawn_blocking(move || {
                    crate::session::idle_reap::claim_idle_stop(
                        &cand.profile,
                        file_watch,
                        &cand.session_id,
                        now,
                        cand.threshold_secs,
                    )
                })
                .await
            };
            let instance = match claim {
                Ok(Ok(Some(instance))) => instance,
                // Not eligible anymore (peer reaper won, user woke it) or a
                // storage error already logged downstream: nothing to do.
                _ => return,
            };
            let req = crate::session::stop::StopRequest {
                session_id: cand.session_id.clone(),
                instance,
            };
            let result =
                tokio::task::spawn_blocking(move || crate::session::stop::perform_stop(&req)).await;
            match result {
                Ok(r) if r.success => {
                    tracing::info!(
                        target: "server.idle_reap",
                        session = %cand.session_id,
                        profile = %cand.profile,
                        threshold_secs = cand.threshold_secs,
                        "auto-stopped idle tmux session",
                    );
                }
                _ => {
                    // The claim already persisted `Stopped`; a failed kill
                    // means tmux/container may still be alive, so flip to
                    // `Error` (matching the manual-stop failure path) instead
                    // of leaving a sticky-but-wrong `Stopped`.
                    let id = cand.session_id.clone();
                    let profile = cand.profile.clone();
                    let file_watch_for_storage = file_watch.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        if let Ok(storage) =
                            crate::session::Storage::new(&profile, file_watch_for_storage)
                        {
                            let _ = storage.update(|instances, _groups| {
                                if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                                    inst.status = crate::session::Status::Error;
                                }
                                Ok(())
                            });
                        }
                    })
                    .await;
                    tracing::warn!(
                        target: "server.idle_reap",
                        session = %cand.session_id,
                        "idle auto-stop kill failed; marked Error",
                    );
                }
            }
        });
    }
}

/// Startup auto-recovery for AI agent sessions whose tmux pane is missing
/// after a daemon restart or system reboot.
///
/// Acquires the cross-process recovery lock; if another process holds it
/// (TUI in standalone mode, or a peer daemon), this returns without doing
/// anything. The lock is held for the entire pass so a late-starting peer
/// cannot duplicate cascades.
///
/// For each candidate:
/// 1. Acquire the per-instance `instance_lock` (serialises against any
///    `ensure_session` REST call that arrives concurrently).
/// 2. Mark `recently_restarted` BEFORE the cascade so the
///    `status_poll_loop` suppression window covers the entire ~7s
///    worst-case latency.
/// 3. Run `restart_with_size_opts(None, false)` via `spawn_blocking`.
/// 4. Update `state.instances` in place with the post-cascade `Instance`.
///
/// Concurrency is capped at `recovery::STARTUP_RECOVERY_CONCURRENCY` to
/// bound cold-start latency without thundering-herd-ing tmux at server
/// warm-up.
/// Phase A: acquire the cross-process lock, warm tmux, snapshot the
/// candidate set, and pre-mark every candidate in `recently_restarted`.
///
/// Returning the marked candidates synchronously (before
/// `status_poll_loop` is spawned) closes the first-tick race where the
/// poller's immediate first iteration could observe missing tmux state
/// and broadcast a phantom Idle->Error transition before any worker
/// has had a chance to mark.
///
/// Uses `batch_pane_metadata()` instead of per-instance probes to keep
/// the listener-bind path under ~20ms regardless of session count.
async fn daemon_startup_recovery_mark(
    state: Arc<AppState>,
) -> Option<(
    crate::session::recovery::RecoveryLock,
    Vec<crate::session::Instance>,
)> {
    let lock = match crate::session::recovery::try_acquire_recovery_lock() {
        Ok(Some(l)) => l,
        Ok(None) => {
            tracing::info!(
                target: "session.startup_recovery",
                "another process holds the recovery lock; skipping daemon startup recovery",
            );
            return None;
        }
        Err(e) => {
            tracing::warn!(
                target: "session.startup_recovery",
                error = %e,
                "failed to acquire recovery lock; skipping daemon startup recovery",
            );
            return None;
        }
    };

    crate::session::recovery::warm_tmux_server();
    crate::tmux::refresh_session_cache();
    // On probe failure we cannot distinguish "all panes dead" from "tmux
    // unreachable", and treating the latter as the former would trigger
    // spurious recovery cascades that kill possibly-alive panes. Skip
    // the entire pass on Err; the next daemon launch will retry.
    let pane_meta = match crate::tmux::batch_pane_metadata() {
        Ok(map) => map,
        Err(e) => {
            tracing::warn!(
                target: "session.startup_recovery",
                error = %e,
                "tmux probe failed at daemon startup; skipping recovery this launch",
            );
            return None;
        }
    };

    let candidates: Vec<crate::session::Instance> = {
        let instances = state.instances.read().await;
        instances
            .iter()
            .filter(|i| {
                let session_name = crate::tmux::Session::generate_name(&i.id, &i.title);
                let has_live_tmux = pane_meta
                    .get(&session_name)
                    .map(|m| !m.pane_dead)
                    .unwrap_or(false);
                !has_live_tmux && crate::session::recovery::is_recovery_candidate(i)
            })
            .cloned()
            .collect()
    };

    if candidates.is_empty() {
        return None;
    }

    for inst in &candidates {
        crate::session::recovery::mark_recently_restarted(&state.recently_restarted, &inst.id);
    }
    // Seed the pending set so the refresher (spawned between Phase A and
    // Phase B) keeps these marks fresh while candidates wait on a
    // STARTUP_RECOVERY_CONCURRENCY permit. Each worker drains its own id on
    // completion.
    crate::session::recovery::seed_recovery_pending(
        &state.recovery_pending,
        candidates.iter().map(|i| i.id.clone()),
    );

    tracing::info!(
        target: "session.startup_recovery",
        count = candidates.len(),
        "starting daemon recovery for missing tmux sessions",
    );

    Some((lock, candidates))
}

/// Phase B: drive the cascade workers for the pre-marked candidates.
async fn daemon_startup_recovery_cascade(
    state: Arc<AppState>,
    lock: crate::session::recovery::RecoveryLock,
    candidates: Vec<crate::session::Instance>,
) {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(
        crate::session::recovery::STARTUP_RECOVERY_CONCURRENCY,
    ));
    // Captured up front for the completion sweep below; the worker loop
    // consumes `candidates`.
    let all_ids: Vec<String> = candidates.iter().map(|i| i.id.clone()).collect();
    let mut tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

    for inst in candidates {
        let permit_sem = semaphore.clone();
        let inst_state = state.clone();
        let id = inst.id.clone();
        let lock_handle = inst_state.instance_lock(&id).await;
        tasks.spawn(async move {
            let _permit = permit_sem
                .acquire_owned()
                .await
                .expect("recovery semaphore not closed");
            let _guard = lock_handle.lock().await;

            // Re-check both `is_recovery_candidate` AND tmux liveness after
            // acquiring the lock: between the snapshot and this point a
            // REST handler (e.g. ensure_session) could have toggled
            // `structured view` OR brought the tmux pane back. Without the
            // tmux re-check, recovery would `kill_clean` a freshly-started
            // pane the user just attached to. The lock + this re-check
            // serialise against any other AoE writer.
            //
            // Use the fallible `batch_pane_metadata()` here so a transient
            // tmux probe failure does NOT collapse to "pane dead" and
            // wrongly proceed with the cascade: skip + unmark instead.
            // Mirrors Phase A's pattern at the mark site.
            let pane_meta = match crate::tmux::batch_pane_metadata() {
                Ok(map) => map,
                Err(e) => {
                    tracing::warn!(
                        target: "session.startup_recovery",
                        instance_id = %id,
                        error = %e,
                        "tmux probe failed during recovery re-check; skipping cascade",
                    );
                    crate::session::recovery::drain_recovery_pending(
                        &inst_state.recovery_pending,
                        &inst_state.recently_restarted,
                        &id,
                    );
                    return;
                }
            };
            let still_candidate = {
                let instances = inst_state.instances.read().await;
                instances
                    .iter()
                    .find(|i| i.id == id)
                    .map(|i| {
                        let session_name = crate::tmux::Session::generate_name(&i.id, &i.title);
                        let has_live_tmux = pane_meta
                            .get(&session_name)
                            .map(|m| !m.pane_dead)
                            .unwrap_or(false);
                        !has_live_tmux && crate::session::recovery::is_recovery_candidate(i)
                    })
                    .unwrap_or(false)
            };
            if !still_candidate {
                // Phase A pre-marked this id and seeded recovery_pending;
                // without draining, the refresher would keep re-stamping the
                // mark and status_poll_loop would suppress the real status
                // even though we are not running a cascade.
                crate::session::recovery::drain_recovery_pending(
                    &inst_state.recovery_pending,
                    &inst_state.recently_restarted,
                    &id,
                );
                return;
            }

            // Phase A already marked this id, but re-mark now to refresh
            // the timestamp so the suppression window covers the full
            // cascade latency starting from this point rather than from
            // the (possibly older) Phase A snapshot.
            crate::session::recovery::mark_recently_restarted(&inst_state.recently_restarted, &id);

            // Refresh the working snapshot from latest in-memory state.
            // Between Phase A's snapshot and acquiring instance_lock, a
            // serialised REST writer (ensure_session, set-session-id, etc.)
            // could have mutated this instance. Without the refresh, the
            // final `*slot = updated` would silently revert that writer's
            // changes (e.g. a freshly-set agent_session_id).
            let mut working = {
                let instances = inst_state.instances.read().await;
                instances
                    .iter()
                    .find(|i| i.id == id)
                    .cloned()
                    .unwrap_or(inst)
            };
            let title = working.title.clone();
            let result = tokio::task::spawn_blocking(move || {
                let res = crate::session::recovery::run_recovery_for_instance(&mut working);
                (working, res)
            })
            .await;

            match result {
                Ok((updated, Ok(outcome))) => {
                    tracing::info!(
                        target: "session.startup_recovery",
                        instance_id = %id,
                        title = %title,
                        ?outcome,
                        "recovery completed",
                    );
                    let mut instances = inst_state.instances.write().await;
                    if let Some(slot) = instances.iter_mut().find(|i| i.id == id) {
                        *slot = updated;
                    }
                    drop(instances);
                    // Release the suppression now that the cascade has
                    // succeeded and the pane is alive. Without this, the
                    // next `status_poll_loop` tick (within 2s) would force
                    // `Status::Starting` for the rest of the TTL window,
                    // broadcasting a phantom `Idle -> Starting` transition
                    // followed by `Starting -> Idle/Running` at TTL expiry.
                    // The suppression's purpose is to cover the in-cascade
                    // window where `last_start_time` is lost on the disk
                    // reload; once the cascade has finished the on-disk
                    // status is current and the poll path resolves to the
                    // correct status without help.
                    crate::session::recovery::drain_recovery_pending(
                        &inst_state.recovery_pending,
                        &inst_state.recently_restarted,
                        &id,
                    );
                }
                Ok((updated, Err(e))) => {
                    tracing::warn!(
                        target: "session.startup_recovery",
                        instance_id = %id,
                        title = %title,
                        error = %e,
                        "recovery cascade failed",
                    );
                    let mut instances = inst_state.instances.write().await;
                    if let Some(slot) = instances.iter_mut().find(|i| i.id == id) {
                        *slot = updated;
                    }
                    drop(instances);
                    // Release the suppression so the next poll respects the
                    // Error state instead of forcing Status::Starting for
                    // the rest of the TTL window.
                    crate::session::recovery::drain_recovery_pending(
                        &inst_state.recovery_pending,
                        &inst_state.recently_restarted,
                        &id,
                    );
                }
                Err(join_err) => {
                    tracing::error!(
                        target: "session.startup_recovery",
                        instance_id = %id,
                        title = %title,
                        error = %join_err,
                        "recovery worker panicked",
                    );
                    let mut instances = inst_state.instances.write().await;
                    if let Some(slot) = instances.iter_mut().find(|i| i.id == id) {
                        slot.status = crate::session::Status::Error;
                        slot.last_error = Some(format!("recovery worker panicked: {}", join_err));
                        // Same stickiness arming as the cascade-Err arm above.
                        slot.last_error_check = Some(std::time::Instant::now());
                    }
                    drop(instances);
                    // Same suppression release as above: without unmarking,
                    // the next poll forces Status::Starting and wipes the
                    // panic-specific last_error written above.
                    crate::session::recovery::drain_recovery_pending(
                        &inst_state.recovery_pending,
                        &inst_state.recently_restarted,
                        &id,
                    );
                }
            }
        });
    }

    while tasks.join_next().await.is_some() {}

    // Completion sweep: every worker drains its own id on each exit arm
    // (including the spawn_blocking panic arm), but a panic in a worker's
    // async body *outside* that match would skip its drain and leave the id
    // pending, so the refresher would re-stamp it until daemon shutdown. By
    // the time the JoinSet is fully drained every worker has terminated, so
    // sweeping all ids guarantees `recovery_pending` is empty and the
    // refresher exits on its next tick. Idempotent for ids already drained.
    for id in &all_ids {
        crate::session::recovery::drain_recovery_pending(
            &state.recovery_pending,
            &state.recently_restarted,
            id,
        );
    }
    drop(lock);
}

/// One task instead of two halves the broadcast clone count and locks
/// `state.instances` once per event instead of twice for the events
/// (e.g. `AcpSessionAssigned`) that both consumers care about.
#[cfg(feature = "serve")]
async fn acp_event_listener(state: Arc<AppState>) {
    let mut rx = state.acp_events_tx.subscribe();
    loop {
        let frame = match rx.recv().await {
            Ok(f) => f,
            // Lagged: a missed event can desync the sidebar dot or
            // skip persisting an `AcpSessionAssigned`. Status will
            // reconcile on the next event; a missed acp_session_id
            // means at most one restart loses context. Far better to
            // continue than to exit the listener entirely.
            Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(
                    target: "acp.event_listener",
                    skipped,
                    "broadcast lagged; status and acp_session_id may briefly desync"
                );
                continue;
            }
            // Closed: AppState dropped (shutdown). Exit cleanly.
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                tracing::debug!(
                    target: "acp.event_listener",
                    "broadcast channel closed; listener exiting"
                );
                return;
            }
        };

        // Detect wake-fire: a `UserPromptSent` arriving at-or-after a
        // `WakeupScheduled`'s `at` timestamp means the agent's pending
        // wake just fired. Push opt-in to the user's phone so /loop
        // dynamic runs don't need them to keep checking the dashboard.
        // See #1091.
        if matches!(
            frame.event.as_ref(),
            crate::acp::state::Event::UserPromptSent { .. }
        ) {
            match state
                .acp_event_store
                .fired_wakeup_for_prompt(&frame.session_id, frame.seq)
            {
                Some((at, reason)) => {
                    let session_id = frame.session_id.clone();
                    let session_title = state
                        .instances
                        .read()
                        .await
                        .iter()
                        .find(|i| i.id == session_id)
                        .map(|i| i.title.clone())
                        .unwrap_or_default();
                    tracing::info!(
                        target: "acp.wakeup",
                        session = %session_id,
                        prompt_seq = frame.seq,
                        wake_at = %at,
                        reason = ?reason,
                        "wake-fire detected; dispatching push notification"
                    );
                    let state_for_push = state.clone();
                    tokio::spawn(async move {
                        crate::server::push::fire_wake_fired_push(
                            state_for_push,
                            &session_id,
                            &session_title,
                            reason.as_deref(),
                        )
                        .await;
                    });
                }
                None => {
                    tracing::trace!(
                        target: "acp.wakeup",
                        session = %frame.session_id,
                        prompt_seq = frame.seq,
                        "UserPromptSent: no fired-wake match (regular follow-up)"
                    );
                }
            }
        }

        // Approval push: when the worker emits an `ApprovalRequested`
        // event, trigger a Web Push so the user sees a "needs approval"
        // alert even when the dashboard is backgrounded. Unlike the
        // status-change pushes in `push.rs`, approvals do NOT honour
        // the TUI/web active-session suppression; the service worker
        // still routes focused clients to an in-app toast via the
        // existing `aoe-push` postMessage path. See #1038.
        if let crate::acp::state::Event::ApprovalRequested { approval } = frame.event.as_ref() {
            let state_for_push = state.clone();
            let session_id = frame.session_id.clone();
            let approval_title = approval.tool_call.name.clone();
            let destructive = approval.destructive;
            let seq = frame.seq;
            tokio::spawn(async move {
                acp_ws::trigger_approval_push(
                    &state_for_push,
                    &session_id,
                    &approval_title,
                    destructive,
                    seq,
                )
                .await;
            });
        }

        // Clear push: when the approval is handled (on any device), retract
        // the "needs approval" notification that the request push raised, so
        // a backgrounded phone or second computer does not keep showing a
        // stale alert for an already-resolved request. See #2491.
        if matches!(
            frame.event.as_ref(),
            crate::acp::state::Event::ApprovalResolved { .. }
        ) {
            let state_for_push = state.clone();
            let session_id = frame.session_id.clone();
            let seq = frame.seq;
            tokio::spawn(async move {
                acp_ws::trigger_approval_clear_push(&state_for_push, &session_id, seq).await;
            });
        }

        // Question push: an `AskUserQuestion` (ElicitationRequested) blocks
        // the turn on the user just like an approval, so it gets the same
        // dedicated, suppression-bypassing push instead of only the generic
        // Waiting one. Same live-event-only path as the approval push above.
        // See #2146.
        if let crate::acp::state::Event::ElicitationRequested { elicitation } = frame.event.as_ref()
        {
            let state_for_push = state.clone();
            let session_id = frame.session_id.clone();
            let question = elicitation.message.clone();
            let seq = frame.seq;
            tokio::spawn(async move {
                acp_ws::trigger_question_push(&state_for_push, &session_id, &question, seq).await;
            });
        }

        // Clear push for an answered question, mirroring the approval clear
        // above. See #2491.
        if matches!(
            frame.event.as_ref(),
            crate::acp::state::Event::ElicitationResolved { .. }
        ) {
            let state_for_push = state.clone();
            let session_id = frame.session_id.clone();
            let seq = frame.seq;
            tokio::spawn(async move {
                acp_ws::trigger_question_clear_push(&state_for_push, &session_id, seq).await;
            });
        }

        // Recall cache: record the agent's advertised config options so the
        // per-agent defaults settings page can populate its dropdowns without a
        // live session. `record` debounces unchanged snapshots and writes off
        // the async runtime. See #2631.
        if let crate::acp::state::Event::ConfigOptionsUpdated { options } = frame.event.as_ref() {
            if !options.is_empty() {
                let agent = state
                    .instances
                    .read()
                    .await
                    .iter()
                    .find(|i| i.id == frame.session_id)
                    .map(|i| {
                        i.agent_name
                            .as_deref()
                            .filter(|s| !s.is_empty())
                            .unwrap_or(i.tool.as_str())
                            .to_string()
                    });
                if let Some(agent) = agent {
                    let options = options.clone();
                    let now = chrono::Utc::now().to_rfc3339();
                    tokio::task::spawn_blocking(move || {
                        if let Err(e) = crate::acp::option_catalog::record(&agent, &options, now) {
                            tracing::warn!(
                                target: "acp.event_listener",
                                agent = %agent,
                                error = %e,
                                "failed to record acp option catalog"
                            );
                        }
                    });
                }
            }
        }

        // Smart-rename defer: fire the one-shot only on a clean
        // `prompt_complete` `Event::Stopped`, so it never races the live worker
        // for the same provider API. Fast-path on the event variant BEFORE
        // touching the two sync mutexes so high-volume streaming-delta frames
        // (`AgentMessageChunk`, `ToolCallStarted`, `ThinkingStarted`, ...) skip
        // the locks entirely; the pure predicate then applies the reason
        // allowlist + the two per-session `contains()` checks. See #2348 and
        // the post-merge review nit on #2651.
        let should_rename = matches!(
            frame.event.as_ref(),
            crate::acp::state::Event::Stopped { .. }
        ) && {
            let attempted = state
                .smart_rename_attempted
                .lock()
                .expect("smart_rename_attempted poisoned");
            let inflight = state
                .smart_rename_inflight
                .lock()
                .expect("smart_rename_inflight poisoned");
            crate::session::smart_rename::should_trigger_smart_rename(
                frame.event.as_ref(),
                &frame.session_id,
                &attempted,
                &inflight,
            )
        };
        if should_rename {
            if let Some(first_message) = state.acp_event_store.first_user_prompt(&frame.session_id)
            {
                let state_for_rename = state.clone();
                let session_id = frame.session_id.clone();
                tokio::spawn(async move {
                    crate::session::smart_rename::try_smart_rename(
                        state_for_rename,
                        session_id,
                        first_message,
                    )
                    .await;
                });
            } else {
                // A `prompt_complete` Stopped without any persisted UserPromptSent
                // is unexpected: `publish_user_prompt_with_attachments` runs
                // strictly before `send_prompt` in the ACP handler, so by the
                // time the turn ends the first prompt should be durable in the
                // event store. A silent skip would hide a plumbing bug (attachment
                // rollback, pruning of an old session, race with SessionCleared);
                // surface it at debug so operators can trace it.
                tracing::debug!(
                    target: "smart_rename",
                    session = %frame.session_id,
                    "trigger fired but event store has no first_user_prompt; skipping"
                );
            }
        }

        let status_intent = derive_acp_status(frame.event.as_ref());
        let acp_change = derive_acp_session_change(frame.event.as_ref());
        if status_intent.is_none() && acp_change.is_none() {
            continue;
        }

        // Acquire `instances` once for both branches. Releases before
        // the (potentially blocking) sessions.json save.
        let profile_to_save = {
            let mut instances = state.instances.write().await;
            let Some(inst) = instances.iter_mut().find(|i| i.id == frame.session_id) else {
                continue;
            };
            if !inst.is_structured() {
                continue;
            }

            apply_status_intent(inst, status_intent, &state.status_tx);
            apply_acp_session_change(inst, &frame.session_id, acp_change.as_ref())
        };

        // Persist `acp_session_id` to disk if the field changed.
        // Sync FS (file copy + JSON write) goes through spawn_blocking
        // so the runtime stays responsive under large session lists.
        if let Some(profile) = profile_to_save {
            let session_id_for_log = frame.session_id.clone();
            let session_id_for_save = frame.session_id.clone();
            let profile_for_save = profile.clone();
            let acp_change_for_save = acp_change.clone();
            let file_watch = state.file_watch.clone();
            let save_result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                let storage = crate::session::Storage::new(&profile_for_save, file_watch)?;
                storage.update(|all, _groups| {
                    if let Some(inst) = all.iter_mut().find(|i| i.id == session_id_for_save) {
                        apply_acp_session_change(
                            inst,
                            &session_id_for_save,
                            acp_change_for_save.as_ref(),
                        );
                    }
                    Ok(())
                })?;
                Ok(())
            })
            .await;
            match save_result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!(
                        target: "acp.event_listener",
                        session = %session_id_for_log,
                        "save after acp_session_id update: {e}"
                    );
                }
                Err(join_err) => {
                    tracing::warn!(
                        target: "acp.event_listener",
                        session = %session_id_for_log,
                        "spawn_blocking join error during acp_session_id save: {join_err}"
                    );
                }
            }
        }
    }
}

/// Seed each acp-enabled session's `Instance.status` from the most
/// recent lifecycle event in the on-disk event log. Runs once at
/// daemon startup, before the status poll loop and the acp event
/// listener start, so a session that was mid-turn when the previous
/// daemon died doesn't render Idle until the next live event arrives.
/// Acts via the same `apply_status_intent` path as the live listener
/// so push subscribers and the broadcast channel see the seeded
/// transitions as ordinary StatusChange events. See #1103 (B).
#[cfg(feature = "serve")]
pub(crate) async fn seed_acp_statuses(state: Arc<AppState>) {
    let acp_ids: Vec<String> = state
        .instances
        .read()
        .await
        .iter()
        .filter(|i| i.is_structured())
        .map(|i| i.id.clone())
        .collect();
    if acp_ids.is_empty() {
        return;
    }
    for id in acp_ids {
        let Some(event) = state.acp_event_store.latest_seed_status_event(&id) else {
            continue;
        };
        let Some(intent) = derive_acp_status(&event) else {
            continue;
        };
        let mut instances = state.instances.write().await;
        if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
            // At startup the on-disk event log is the whole truth: nothing
            // is racing the apply, unlike the live stream. If the latest
            // lifecycle event shows a turn was in flight when the previous
            // daemon died, a stale persisted Stopped must not trap the dot
            // grey, so clear it before the Running/Waiting intent applies.
            // A latest Idle/Error (clean or deliberate stop) is left as
            // Stopped by apply_status_intent's guard. See #2248.
            if inst.status == Status::Stopped
                && matches!(intent, StatusIntent::Set(Status::Running | Status::Waiting))
            {
                inst.status = Status::Idle;
            }
            apply_status_intent(inst, Some(intent), &state.status_tx);
        }
    }
}

/// Fold a derived `StatusIntent` into an `Instance`. Pure mutation;
/// callers hold the write lock. Sends a `StatusChange` on
/// `status_tx` so push notifications and the dashboard see the
/// transition like any tmux-driven one.
#[cfg(feature = "serve")]
pub(crate) fn apply_status_intent(
    inst: &mut Instance,
    intent: Option<StatusIntent>,
    status_tx: &broadcast::Sender<StatusChange>,
) {
    let Some(intent) = intent else { return };
    // Genuine in-flight terminal states: never fight them.
    if matches!(inst.status, Status::Deleting | Status::Creating) {
        return;
    }
    let target = match intent {
        StatusIntent::Set(s) => {
            // A Stopped session must not be woken by a trailing worker
            // event: acp events keep arriving for a few ticks after a
            // Stop, and a deliberate Stop must keep showing Stopped. Only
            // a fresh worker-epoch signal (HealError below) lifts Stopped;
            // the live UserPromptSent that follows the respawn then drives
            // Running. Without this, the chain Stopped -> (trailing prompt)
            // Running -> (trailing stop) Idle would strand a deliberate
            // Stop on Idle.
            if inst.status == Status::Stopped {
                return;
            }
            s
        }
        // HealError comes only from AcpSessionAssigned / RateLimitAuto
        // Resumed, both emitted when a fresh worker attaches and never as
        // trailing post-stop events. So heal a sticky Error AND wake a
        // session out of a stale Stopped (idle-reap or manual stop, then
        // re-prompt): the live worker is provably back. See #2248.
        StatusIntent::HealError => {
            if !matches!(inst.status, Status::Error | Status::Stopped) {
                return;
            }
            Status::Idle
        }
    };
    if inst.status == target {
        return;
    }
    let prev = inst.status;
    inst.status = target;
    let now = chrono::Utc::now();
    inst.last_accessed_at = Some(now);
    inst.idle_entered_at = if target == Status::Idle {
        Some(now)
    } else {
        None
    };
    let _ = status_tx.send(StatusChange {
        instance_id: inst.id.clone(),
        instance_title: inst.title.clone(),
        old: prev,
        new: target,
        at: now,
    });
}

/// Fold a derived `AcpSessionChange` into an `Instance`. Returns the
/// owning profile when sessions.json needs to be re-saved (so the new
/// `acp_session_id` survives daemon restart), or `None` if the
/// change was a no-op or no change was emitted.
#[cfg(feature = "serve")]
fn apply_acp_session_change(
    inst: &mut Instance,
    session_id: &str,
    change: Option<&AcpSessionChange>,
) -> Option<String> {
    match change? {
        AcpSessionChange::Assigned(new_id) => {
            // A worker just initialized (session/new or session/load), so the
            // session is by definition no longer idle-dormant. Clear any
            // marker now: a stale one left by a non-user respawn (e.g. the
            // build-stale respawn #1754, which brings the worker back without
            // a user wake) otherwise makes the reconciler's
            // `!is_idle_dormant()` resume filter refuse to bring the session
            // back after this worker later dies, deadlocking a queued prompt
            // that the client parked waiting for a worker that never returns.
            // See #2237.
            let cleared_stale_dormant = inst.idle_dormant_since.take().is_some();
            let same_acp_session = inst.acp_session_id.as_deref() == Some(new_id.as_str());
            // #2276: clear import_pending only when the assigned id matches the
            // imported one, i.e. the import's session/load actually landed and
            // its replay is now in the event store. A fallback session/new (or
            // a stale worker) reports a different id; consuming the marker then
            // would block a later retry from re-seeding the transcript.
            let cleared_import_pending = if same_acp_session {
                inst.import_pending.take().unwrap_or(false)
            } else {
                false
            };
            if same_acp_session {
                // Same id (a reattach / session/load reuses it). Only persist
                // if we actually cleared a stale dormant marker or the import
                // flag; otherwise the id is already on disk and there is
                // nothing to rewrite.
                if cleared_stale_dormant || cleared_import_pending {
                    tracing::info!(
                        target: "acp.event_listener",
                        session = %session_id,
                        cleared_import_pending,
                        "cleared stale idle-dormant / import marker on worker (re)assign"
                    );
                    return Some(inst.source_profile.clone());
                }
                return None;
            }
            tracing::info!(
                target: "acp.event_listener",
                session = %session_id,
                acp_session_id = %new_id,
                "persisting agent-assigned ACP session id"
            );
            inst.acp_session_id = Some(new_id.clone());
            // A structured fork sets fork_pending + import_pending together at
            // creation and does not pre-pin acp_session_id, so the adapter's
            // new forked id arrives on THIS different-id path. Consume both
            // one-shot markers together: a restart resumes the child via
            // session/load instead of re-forking the parent, and leaving
            // import_pending set would make that resume re-seed the transcript
            // into an already-populated store (duplicate-key corruption, the
            // #2276 class). Gate the import clear on fork_pending having been
            // set, so a non-fork different-id assignment leaves import_pending
            // alone for its own retry.
            if inst.fork_pending.take().is_some() {
                inst.import_pending = None;
            }
        }
        AcpSessionChange::Reset(reason) => {
            tracing::info!(
                target: "acp.event_listener",
                session = %session_id,
                %reason,
                "clearing stored ACP session id after a context reset (session/load or session/fork failure)"
            );
            inst.acp_session_id = None;
            // A structured fork that failed (or was refused by a resume-only
            // agent) reaches here via SessionContextReset. Clear the one-shot
            // fork marker so the reconciler stops re-issuing the same failing
            // `session/fork` on every reattach, and drop the paired
            // import_pending the same way the success path does so the fallback
            // spawn is a clean session/new. A session/load-failure reset has no
            // fork pending, so this is a no-op there.
            if inst.fork_pending.take().is_some() {
                inst.import_pending = None;
            }
        }
    }
    Some(inst.source_profile.clone())
}

/// What an event tells the ACP-session-id listener to do. `None` means
/// the event is irrelevant. Extracted so the JSON-shape parsing has a
/// pure-function test surface.
#[cfg(feature = "serve")]
#[derive(Debug, PartialEq, Eq, Clone)]
enum AcpSessionChange {
    Assigned(String),
    Reset(String),
}

#[cfg(feature = "serve")]
fn derive_acp_session_change(event: &crate::acp::Event) -> Option<AcpSessionChange> {
    use crate::acp::Event;
    match event {
        Event::AcpSessionAssigned { acp_session_id } => {
            Some(AcpSessionChange::Assigned(acp_session_id.clone()))
        }
        Event::SessionContextReset { reason } => Some(AcpSessionChange::Reset(reason.clone())),
        _ => None,
    }
}

/// What an acp event implies for the sidebar status. `Set` is an
/// unconditional transition; `HealError` only takes effect if the
/// current status is `Error` (used to recover the sidebar from a
/// sticky `AgentStartupError` banner after a successful respawn
/// without clobbering an in-progress Running/Waiting turn).
#[cfg(feature = "serve")]
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StatusIntent {
    Set(Status),
    HealError,
}

#[cfg(feature = "serve")]
pub(crate) fn derive_acp_status(event: &crate::acp::Event) -> Option<StatusIntent> {
    use crate::acp::Event;
    match event {
        Event::UserPromptSent { .. }
        | Event::ApprovalResolved { .. }
        | Event::ElicitationResolved { .. } => Some(StatusIntent::Set(Status::Running)),
        // Agent transcript output means a turn is live even when no
        // UserPromptSent preceded it. A fired ScheduleWakeup or a background
        // TaskOutput notification resumes the turn agent-side, streaming only
        // these events; aoe never publishes a prompt for them, so without this
        // the sidebar dot stayed grey through real work. apply_status_intent's
        // guards keep a deliberate Stopped grey and no-op once already Running.
        Event::ThinkingStarted
        | Event::AgentMessageChunk { .. }
        | Event::ToolCallStarted { .. } => Some(StatusIntent::Set(Status::Running)),
        // A pending approval or elicitation both block the turn on the
        // user, so the sidebar dot goes yellow either way.
        Event::ApprovalRequested { .. } | Event::ElicitationRequested { .. } => {
            Some(StatusIntent::Set(Status::Waiting))
        }
        // All Stopped reasons surface as Idle, including the
        // rate-limit park: the worker is not crashed, the user just
        // hit a provider quota and the session is waiting for reset
        // (or for the user to switch to another ACP backend). The
        // dedicated RateLimit banner carries the reset time, so the
        // sidebar pill staying grey is the right signal. See #1281.
        Event::Stopped { .. } => Some(StatusIntent::Set(Status::Idle)),
        Event::AgentStartupError { .. } => Some(StatusIntent::Set(Status::Error)),
        // A successful session/new or session/load means the agent
        // is alive. Heal a sticky Error banner so the sidebar dot
        // reverts from red to grey; do NOT clobber an in-progress
        // Running/Waiting turn (a respawn during an active turn
        // would otherwise stop the spinner mid-stream).
        Event::AcpSessionAssigned { .. } => Some(StatusIntent::HealError),
        // Auto-resume after a rate-limit park: the worker is coming back.
        // Heal any sticky error so the sidebar dot recovers; the imminent
        // fresh spawn emits AcpSessionAssigned and live events right after.
        Event::RateLimitAutoResumed { .. } => Some(StatusIntent::HealError),
        _ => None,
    }
}

/// Test-only constructors that integration tests in `tests/` need to drive
/// `reload_state_instances_from_disk` and the dynamic-profile-rewire helpers
/// without going through the full daemon. Mirrors the pattern at
/// `src/tmux/mod.rs`'s `test_support` module: gated on
/// `#[cfg(any(test, feature = "test-support"))]` so the surface stays out of
/// production builds, and `#[doc(hidden)]` so it's invisible in rustdoc.
#[cfg(all(feature = "serve", any(test, feature = "test-support")))]
#[doc(hidden)]
pub mod test_support {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicI64;

    /// Build a minimal `Arc<AppState>` for helper-equivalence tests. Most
    /// fields are seeded with empty / default values; only `instances`,
    /// `recently_restarted`, and the file-watch trio are real. Acp
    /// fields are stubbed because the helper's acp overlay reads them.
    pub fn build_test_app_state(prior: Vec<Instance>) -> Arc<AppState> {
        let app_dir = tempfile::tempdir().expect("tempdir");
        let acp_db = app_dir.path().join("acp_events.db");
        let event_store =
            Arc::new(crate::acp::event_store::EventStore::open(&acp_db, 100).expect("event store"));
        let acp_events_tx = broadcast::channel::<AcpBroadcastFrame>(8).0;
        let sink = std::sync::Arc::new(crate::acp::supervisor::ChannelSink {
            tx: acp_events_tx.clone(),
            event_store: event_store.clone(),
        });
        let supervisor =
            std::sync::Arc::new(crate::acp::supervisor::Supervisor::with_capacity(sink, 1));
        Arc::new(AppState {
            profile: "test".to_string(),
            read_only: false,
            instances: RwLock::new(prior),
            token_manager: Arc::new(TokenManager::new(None, Duration::from_secs(3600))),
            login_manager: Arc::new(login::LoginManager::new(None)),
            rate_limiter: Arc::new(RateLimiter::new()),
            behind_tunnel: false,
            auth_mode: "none",
            serve_mode: "local",
            instance_locks: RwLock::new(HashMap::new()),
            smart_rename_inflight: std::sync::Mutex::new(std::collections::HashSet::new()),
            smart_rename_attempted: std::sync::Mutex::new(std::collections::HashSet::new()),
            smart_rename_semaphore: tokio::sync::Semaphore::new(
                crate::session::smart_rename::MAX_CONCURRENT,
            ),
            recently_restarted: crate::session::recovery::new_recently_restarted(),
            recovery_pending: crate::session::recovery::new_recovery_pending(),
            cleanup_defaults_cache: RwLock::new(CleanupDefaultsCache {
                refreshed_at: std::time::Instant::now(),
                entries: HashMap::new(),
            }),
            changed_files_cache: std::sync::RwLock::new(std::collections::HashMap::new()),
            remote_owner_cache: RwLock::new(HashMap::new()),
            status_tx: broadcast::channel(STATUS_CHANNEL_CAPACITY).0,
            acp_events_tx,
            acp_event_store: event_store,
            acp_supervisor: supervisor,
            plugin_host: None,
            plugin_jobs: Arc::new(api::plugins::PluginJobRegistry::new()),
            push: None,
            push_enabled: false,
            web_config: crate::session::config::WebConfig::default(),
            last_web_activity: AtomicI64::new(0),
            telemetry_usage_seen: crate::telemetry::usage_signals::UsageSeenCounters::new(),
            telemetry_web_clients: FormFactorCounters::default(),
            telemetry_structured_clients: FormFactorCounters::default(),
            telemetry_session_creates: std::sync::atomic::AtomicU32::new(0),
            telemetry_structured: StructuredTelemetryCounters::default(),
            telemetry_last_reported: std::sync::Mutex::new(None),
            shutdown: CancellationToken::new(),
            file_watch: FileWatchService::noop(),
            disk_changed: Arc::new(tokio::sync::Notify::new()),
            disk_watch_handles: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        })
    }

    pub async fn has_disk_watch_handle(state: &Arc<AppState>, profile: &str) -> bool {
        state.disk_watch_handles.lock().await.contains_key(profile)
    }

    pub async fn disk_watch_handle_count(state: &Arc<AppState>) -> usize {
        state.disk_watch_handles.lock().await.len()
    }

    pub use super::api::system::{
        create_profile, delete_profile, rename_profile, CreateProfileBody, RenameProfileBody,
    };

    pub async fn add_profile_disk_watch(state: &Arc<AppState>, profile: &str) {
        super::add_profile_disk_watch(state, profile).await
    }

    pub async fn remove_profile_disk_watch(state: &Arc<AppState>, profile: &str) {
        super::remove_profile_disk_watch(state, profile).await
    }

    pub async fn rename_profile_disk_watch(state: &Arc<AppState>, old: &str, new: &str) {
        super::rename_profile_disk_watch(state, old, new).await
    }

    /// Replace the `Arc<FileWatchService>` on a unique-Arc'd `AppState`.
    /// Tests build state with a `noop` service, then swap to live before
    /// exercising propagation paths. Crate-internal field access is
    /// hidden behind this helper so the field can stay `pub(crate)`.
    pub fn replace_file_watch(state: &mut AppState, fw: Arc<crate::file_watch::FileWatchService>) {
        state.file_watch = fw;
    }

    /// Read the current `Arc<FileWatchService>` for tests asserting on
    /// `subscriber_count`. The Arc clone is cheap.
    pub fn file_watch(state: &AppState) -> Arc<crate::file_watch::FileWatchService> {
        state.file_watch.clone()
    }

    pub async fn reload_disk_only_for_test(state: &Arc<AppState>, fresh: Vec<Instance>) {
        super::reload_state_instances_from_disk(state, fresh, super::StatusSource::DiskOnly).await
    }

    pub async fn reload_tmux_applied_for_test(state: &Arc<AppState>, fresh: Vec<Instance>) {
        super::reload_state_instances_from_disk(state, fresh, super::StatusSource::TmuxApplied)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_web_build_id_finds_entry_bundle() {
        let html = r#"<head><script type="module" crossorigin src="/assets/index-DKenwdW0.js"></script>
<link rel="modulepreload" crossorigin href="/assets/vendor-Bx91yz.js"></head>"#;
        assert_eq!(
            extract_web_build_id(html).as_deref(),
            Some("index-DKenwdW0.js")
        );
    }

    #[test]
    fn extract_web_build_id_none_without_entry() {
        assert_eq!(extract_web_build_id("<html><body>hi</body></html>"), None);
    }

    #[test]
    fn cache_control_immutable_only_for_hashed_assets() {
        assert_eq!(
            cache_control_for("assets/index-DKenwdW0.js"),
            "public, max-age=31536000, immutable"
        );
        // Rollup hashes draw from the base64url alphabet (`_` and `-`
        // included), and chunk base names can themselves contain `-`.
        assert_eq!(
            cache_control_for("assets/StructuredView-DM_xphSL.js"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(
            cache_control_for("assets/theme-bootstrap-Ab12Cd34.css"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(cache_control_for("index.html"), "no-cache");
        assert_eq!(cache_control_for("sw.js"), "no-cache");
        assert_eq!(
            cache_control_for("fonts/GeistMono-Regular.woff2"),
            "no-cache"
        );
        // Un-hashed files under assets/ must NOT be pinned for a year.
        assert_eq!(cache_control_for("assets/logo.svg"), "no-cache");
        assert_eq!(cache_control_for("assets/readme"), "no-cache");
        assert_eq!(cache_control_for("assets/short-a1.js"), "no-cache");
    }

    #[test]
    fn merge_runtime_fields_preserves_last_error_while_still_in_error() {
        // Cascade-Err preservation: prior held the error string, fresh re-derived
        // Error from a still-dead pane without re-attaching the message. Carry it.
        let mut prior = Instance::new("seed", "/tmp/seed");
        prior.status = Status::Error;
        prior.last_error = Some("recovery cascade: foo".to_string());

        let mut fresh = Instance::new("seed", "/tmp/seed");
        fresh.status = Status::Error;
        fresh.last_error = None;

        let merged = merge_runtime_fields(prior, fresh);
        assert_eq!(merged.last_error.as_deref(), Some("recovery cascade: foo"));
    }

    // #2237: a worker coming live (AcpSessionAssigned) must clear a stale
    // idle-dormant marker, even when the acp_session_id is unchanged (a
    // session/load reattach reuses it). Without this, a stale marker left by a
    // non-user respawn keeps the reconciler's resume filter skipping the
    // session forever once the worker dies, deadlocking a queued prompt.
    #[cfg(feature = "serve")]
    #[test]
    fn acp_session_assigned_clears_stale_dormant_marker_on_same_id() {
        let mut inst = Instance::new("seed", "/tmp/seed");
        inst.acp_session_id = Some("sid-1".to_string());
        inst.idle_dormant_since = Some(chrono::Utc::now());

        // Same id as already stored: the only reason to persist is the
        // stale-dormant clear, so the function must return Some(profile).
        let persist = apply_acp_session_change(
            &mut inst,
            "seed",
            Some(&AcpSessionChange::Assigned("sid-1".to_string())),
        );
        assert!(
            inst.idle_dormant_since.is_none(),
            "dormant marker must be cleared when a worker (re)assigns"
        );
        assert!(
            persist.is_some(),
            "clearing a stale marker must trigger a persist even on an unchanged id"
        );
    }

    // A structured fork mints a brand-new child id on its first session/fork,
    // so the assigned id differs from the (None) acp_session_id and we take the
    // new-assignment path. That path must consume the one-shot fork_pending seed
    // and persist, so a restart resumes the child via session/load rather than
    // re-forking the parent.
    #[cfg(feature = "serve")]
    #[test]
    fn assigning_forked_id_clears_fork_pending_and_persists() {
        let mut inst = Instance::new("seed", "/tmp/seed");
        inst.view = crate::session::View::Structured;
        inst.acp_session_id = None;
        inst.fork_pending = Some("parent-acp-id".into());
        inst.import_pending = Some(true);

        let profile = apply_acp_session_change(
            &mut inst,
            "sess-1",
            Some(&AcpSessionChange::Assigned("forked-child-id".into())),
        );

        assert_eq!(inst.acp_session_id.as_deref(), Some("forked-child-id"));
        assert_eq!(
            inst.fork_pending, None,
            "fork_pending cleared once the forked id is assigned"
        );
        assert_eq!(
            inst.import_pending, None,
            "import_pending consumed alongside fork_pending so a restart does not re-seed the transcript into the forked store"
        );
        assert!(
            profile.is_some(),
            "must persist so the forked id survives restart"
        );
    }

    // A different-id assignment that is NOT consuming a fork (fork_pending is
    // None) must leave import_pending alone: that marker belongs to the import
    // flow, which lands on the same-id path, and clearing it here would block a
    // legitimate import retry from re-seeding the transcript.
    #[cfg(feature = "serve")]
    #[test]
    fn non_fork_assignment_preserves_import_pending() {
        let mut inst = Instance::new("seed", "/tmp/seed");
        inst.acp_session_id = None;
        inst.fork_pending = None;
        inst.import_pending = Some(true);

        let profile = apply_acp_session_change(
            &mut inst,
            "sess-1",
            Some(&AcpSessionChange::Assigned("some-new-id".into())),
        );

        assert_eq!(inst.acp_session_id.as_deref(), Some("some-new-id"));
        assert_eq!(
            inst.import_pending,
            Some(true),
            "a non-fork different-id assignment must not consume import_pending"
        );
        assert!(
            profile.is_some(),
            "a new id assignment must persist regardless of markers"
        );
    }

    // A SessionContextReset from a FAILED structured fork must clear the
    // one-shot fork marker (and its paired import marker) so neither the
    // reconciler nor the supervisor re-issues the same failing session/fork on
    // the next reattach. This is the reducer side of the fork-failure retry-loop
    // fix; the reset carries no new id, so acp_session_id is cleared too.
    #[cfg(feature = "serve")]
    #[test]
    fn reset_clears_fork_pending_and_import_pending() {
        let mut inst = Instance::new("seed", "/tmp/seed");
        inst.view = crate::session::View::Structured;
        inst.acp_session_id = Some("stale-parent-id".into());
        inst.fork_pending = Some("parent-acp-id".into());
        inst.import_pending = Some(true);

        let profile = apply_acp_session_change(
            &mut inst,
            "sess-1",
            Some(&AcpSessionChange::Reset("fork_failed: boom".into())),
        );

        assert_eq!(inst.acp_session_id, None, "reset clears the stored id");
        assert_eq!(
            inst.fork_pending, None,
            "a failed fork's one-shot marker must clear so it is not retried"
        );
        assert_eq!(
            inst.import_pending, None,
            "import_pending is consumed alongside fork_pending on reset"
        );
        assert!(profile.is_some(), "the reset must persist");
    }

    // A SessionContextReset from a plain session/load failure (no fork pending)
    // must clear the dead id but leave import_pending untouched: that marker
    // belongs to the import flow, and clearing it here would block a legitimate
    // import retry. Mirrors the non-fork assignment guard.
    #[cfg(feature = "serve")]
    #[test]
    fn reset_without_fork_pending_preserves_import_pending() {
        let mut inst = Instance::new("seed", "/tmp/seed");
        inst.acp_session_id = Some("dead-id".into());
        inst.fork_pending = None;
        inst.import_pending = Some(true);

        let profile = apply_acp_session_change(
            &mut inst,
            "sess-1",
            Some(&AcpSessionChange::Reset("session/load failed: gone".into())),
        );

        assert_eq!(inst.acp_session_id, None, "reset clears the dead id");
        assert_eq!(
            inst.import_pending,
            Some(true),
            "a non-fork reset must not consume import_pending"
        );
        assert!(profile.is_some(), "the reset must persist");
    }

    #[cfg(feature = "serve")]
    #[test]
    fn acp_session_assigned_same_id_no_marker_is_noop() {
        let mut inst = Instance::new("seed", "/tmp/seed");
        inst.acp_session_id = Some("sid-1".to_string());
        inst.idle_dormant_since = None;
        // Same id, nothing stale to clear: must stay a no-op (no rewrite).
        let persist = apply_acp_session_change(
            &mut inst,
            "seed",
            Some(&AcpSessionChange::Assigned("sid-1".to_string())),
        );
        assert!(
            persist.is_none(),
            "unchanged id with no stale marker is a no-op"
        );
    }

    #[test]
    fn merge_runtime_fields_drops_stale_last_error_on_healthy_transition() {
        // Issue #1271: prior errored in-memory, the session recovered to Idle
        // through a path that never nulled `last_error`. The fresh poll must not
        // re-stick the stale string on a now-green session.
        let mut prior = Instance::new("seed", "/tmp/seed");
        prior.status = Status::Error;
        prior.last_error = Some("recovery cascade: foo".to_string());

        let mut fresh = Instance::new("seed", "/tmp/seed");
        fresh.status = Status::Idle;
        fresh.last_error = None;

        let merged = merge_runtime_fields(prior, fresh);
        assert_eq!(merged.last_error, None);
    }

    #[test]
    fn merge_runtime_fields_drops_stale_last_error_idle_to_idle() {
        // Both ends healthy but prior still carried a stale string: don't propagate.
        let mut prior = Instance::new("seed", "/tmp/seed");
        prior.status = Status::Idle;
        prior.last_error = Some("stale".to_string());

        let mut fresh = Instance::new("seed", "/tmp/seed");
        fresh.status = Status::Idle;
        fresh.last_error = None;

        let merged = merge_runtime_fields(prior, fresh);
        assert_eq!(merged.last_error, None);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn init_disk_watch_subscriptions_bootstraps_one_reload_after_wiring() {
        let temp = tempfile::tempdir().expect("tempdir");
        // SAFETY: serialized test; no other test mutates HOME concurrently.
        unsafe { std::env::set_var("HOME", temp.path()) };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
        }

        let storage = crate::session::Storage::new_unwatched("startup-gap").expect("storage");
        storage
            .update(|instances, _groups| {
                *instances = vec![Instance::new("seed", "/tmp/seed")];
                Ok(())
            })
            .expect("seed write");

        let state = test_support::build_test_app_state(Vec::new());
        let live = FileWatchService::new().expect("live svc");
        let mut state_mut = Arc::try_unwrap(state)
            .map_err(|_| ())
            .expect("unique state");
        state_mut.file_watch = live;
        let state = Arc::new(state_mut);

        let wake = {
            let signal = state.disk_changed.clone();
            tokio::spawn(async move {
                tokio::time::timeout(std::time::Duration::from_secs(2), signal.notified()).await
            })
        };

        init_disk_watch_subscriptions(state.clone()).await;

        let woke = wake.await.expect("join");
        assert!(
            woke.is_ok(),
            "startup wiring must bootstrap one disk_changed wake after subscriptions are installed"
        );
        assert_eq!(
            state.file_watch.subscriber_count(),
            1,
            "startup wiring must leave exactly one live subscription for the single profile"
        );
    }

    // Concurrent same-profile rewires must converge to a single
    // consistent map entry and matching live subscription count. The
    // unified helper holds `disk_watch_handles` through the full
    // teardown-then-install transition, so the lock-acquisition order
    // alone decides which call wins; an interleaved unsubscribe and
    // subscribe across two callers cannot leave a half-state where the
    // map and the dispatcher disagree.
    #[tokio::test]
    #[serial_test::serial]
    async fn add_remove_profile_disk_watch_serializes_concurrent_add_and_remove() {
        let temp = tempfile::tempdir().expect("tempdir");
        // SAFETY: serialized test; no other test mutates HOME concurrently.
        unsafe { std::env::set_var("HOME", temp.path()) };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
        }
        let _ = crate::session::get_profile_dir("rewire-race").expect("profile dir");

        let state = test_support::build_test_app_state(Vec::new());
        let live = FileWatchService::new().expect("live svc");
        let mut state_mut = Arc::try_unwrap(state)
            .map_err(|_| ())
            .expect("unique state");
        state_mut.file_watch = live;
        let state = Arc::new(state_mut);

        let mut joins = Vec::new();
        for i in 0..50 {
            let s = state.clone();
            joins.push(tokio::spawn(async move {
                if i % 2 == 0 {
                    add_profile_disk_watch(&s, "rewire-race").await;
                } else {
                    remove_profile_disk_watch(&s, "rewire-race").await;
                }
            }));
        }
        for j in joins {
            j.await.expect("join");
        }

        let count = test_support::disk_watch_handle_count(&state).await;
        assert!(
            count <= 1,
            "concurrent rewires must not leak duplicate entries (got {count})"
        );
        let live_subs = state.file_watch.subscriber_count();
        assert_eq!(
            live_subs, count,
            "live subscriptions must equal map entries; mismatch indicates a leaked or orphaned entry"
        );

        add_profile_disk_watch(&state, "rewire-race").await;
        assert_eq!(
            test_support::disk_watch_handle_count(&state).await,
            1,
            "deterministic add must produce exactly one entry"
        );
        assert_eq!(state.file_watch.subscriber_count(), 1);

        remove_profile_disk_watch(&state, "rewire-race").await;
        assert_eq!(test_support::disk_watch_handle_count(&state).await, 0);
        assert_eq!(state.file_watch.subscriber_count(), 0);
    }

    // Concurrent same-profile add and remove must converge to the
    // last-completed call's intent. The barrier inside
    // `build_disk_watch_entry` lets the test pin task A mid-build
    // while A still holds `disk_watch_handles`, so B's remove blocks
    // until A finishes installing. Once A releases the lock, B's
    // remove wins because it ran strictly after A's install: the
    // final map is empty. If `disk_watch_handles` were not held
    // across the build, B could acquire the lock during A's parked
    // window, observe an empty map, and let A install a stale entry
    // on resume.
    #[tokio::test]
    #[serial_test::serial]
    async fn add_profile_disk_watch_resists_resurrection_under_concurrent_remove() {
        let temp = tempfile::tempdir().expect("tempdir");
        // SAFETY: serialized test; no other test mutates HOME concurrently.
        unsafe { std::env::set_var("HOME", temp.path()) };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
        }
        let _ = crate::session::get_profile_dir("race-fix").expect("profile dir");

        let state = test_support::build_test_app_state(Vec::new());
        let live = FileWatchService::new().expect("live svc");
        let mut state_mut = Arc::try_unwrap(state)
            .map_err(|_| ())
            .expect("unique state");
        state_mut.file_watch = live;
        let state = Arc::new(state_mut);

        let barrier = Arc::new(DiskWatchBuildBarrier {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            armed: tokio::sync::Notify::new(),
        });
        let _barrier_guard = DiskWatchBuildBarrierGuard::install(barrier.clone());

        let s_a = state.clone();
        let task_a = tokio::spawn(async move {
            add_profile_disk_watch(&s_a, "race-fix").await;
        });

        // Wait deterministically until A is parked inside the
        // barrier. No fixed sleep: the `entered` notification is
        // sent strictly after `subscribe_channel` returns and the
        // forwarder is spawned, which is the build-vs-install
        // boundary the test wants to exercise.
        barrier.entered.notified().await;

        let s_b = state.clone();
        let barrier_b = barrier.clone();
        let task_b = tokio::spawn(async move {
            // Signal "B is about to call remove" so the test can
            // proceed to release A without a fixed-time sleep. The
            // notify lands one executor tick before B's `lock().await`
            // registers as a waiter; A still holds the lock so B
            // parks behind A regardless of relative scheduling.
            barrier_b.armed.notify_one();
            remove_profile_disk_watch(&s_b, "race-fix").await;
        });

        // Deterministic happens-before for B's lock attempt: replaces
        // the prior bounded sleep that flaked on heavily-loaded CI.
        barrier.armed.notified().await;
        tokio::task::yield_now().await;

        // Release A; it finishes building, installs the entry, and
        // releases the lock. B then acquires and removes.
        barrier.release.notify_one();

        task_a.await.expect("join A");
        task_b.await.expect("join B");

        let count = test_support::disk_watch_handle_count(&state).await;
        let live_subs = state.file_watch.subscriber_count();
        assert_eq!(
            count, 0,
            "B's remove must observe A's installed entry and tear it down. \
             A non-zero count here means a removed profile was resurrected by \
             an interleaved subscribe."
        );
        assert_eq!(
            live_subs, 0,
            "live subscription count must match the empty handle map; mismatch \
             indicates a leaked subscriber from a resurrected entry."
        );
    }

    // Writes that land during init's per-profile iteration, before
    // their profile has been subscribed, must still be reconciled
    // once init returns. The hook fires after each install; the
    // test uses it to seed a write to a profile not yet reached by
    // the loop. The bootstrap notify at init's end wakes the
    // consumer, which then loads from disk and surfaces both the
    // pre-init seed and the mid-iteration seed.
    #[tokio::test]
    #[serial_test::serial]
    async fn init_disk_watch_subscriptions_reconciles_writes_landing_during_iteration() {
        let temp = tempfile::tempdir().expect("tempdir");
        // SAFETY: serialized test; no other test mutates HOME concurrently.
        unsafe { std::env::set_var("HOME", temp.path()) };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
        }

        let storage_p1 = crate::session::Storage::new_unwatched("init-gap-p1").expect("p1");
        storage_p1
            .update(|i, _| {
                *i = vec![Instance::new("p1-pre-init", "/tmp/p1-pre")];
                Ok(())
            })
            .expect("seed p1");
        let _ = crate::session::get_profile_dir("init-gap-p2").expect("p2 dir");

        let state = test_support::build_test_app_state(Vec::new());
        let live = FileWatchService::new().expect("live svc");
        let mut state_mut = Arc::try_unwrap(state)
            .map_err(|_| ())
            .expect("unique state");
        state_mut.file_watch = live;
        let state = Arc::new(state_mut);

        init_disk_watch_subscriptions_with_hook(state.clone(), |profile| {
            if profile == "init-gap-p1" {
                // Write to P2 at the precise moment when P1 has just
                // been subscribed but P2 has not. The watcher path
                // cannot deliver this event for P2 (no subscription
                // exists yet); only the bootstrap wake plus a reload
                // can reconcile it.
                let storage = crate::session::Storage::new_unwatched("init-gap-p2").expect("p2");
                storage
                    .update(|i, _| {
                        *i = vec![Instance::new("p2-mid-init", "/tmp/p2-mid")];
                        Ok(())
                    })
                    .expect("seed p2");
            }
        })
        .await;

        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            state.disk_changed.notified(),
        )
        .await
        .expect("bootstrap wake must fire after init returns");

        let file_watch = state.file_watch.clone();
        let fresh = tokio::task::spawn_blocking(move || load_all_instances(&file_watch))
            .await
            .expect("join")
            .expect("load");
        reload_state_instances_from_disk(&state, fresh, StatusSource::DiskOnly).await;

        let instances = state.instances.read().await;
        let titles: Vec<&str> = instances.iter().map(|i| i.title.as_str()).collect();
        assert!(
            titles.contains(&"p1-pre-init"),
            "writes BEFORE init started must be reconciled; titles: {:?}",
            titles
        );
        assert!(
            titles.contains(&"p2-mid-init"),
            "writes DURING init's iteration (the gap window) must be reconciled by the bootstrap wake; titles: {:?}",
            titles
        );
    }

    // Bootstrap correctness here has two requirements: subscriptions must be
    // installed before the first wake, and writes that land before init
    // returns must still be visible after the consumer reloads disk state.
    #[tokio::test]
    #[serial_test::serial]
    async fn bootstrap_wake_makes_pre_init_writes_reachable_via_reload() {
        let temp = tempfile::tempdir().expect("tempdir");
        // SAFETY: serialized test; no other test mutates HOME concurrently.
        unsafe { std::env::set_var("HOME", temp.path()) };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
        }

        let storage = crate::session::Storage::new_unwatched("startup-reload").expect("storage");
        storage
            .update(|instances, _groups| {
                *instances = vec![Instance::new("pre-init", "/tmp/pre-init")];
                Ok(())
            })
            .expect("seed write");

        let state = test_support::build_test_app_state(Vec::new());
        let live = FileWatchService::new().expect("live svc");
        let mut state_mut = Arc::try_unwrap(state)
            .map_err(|_| ())
            .expect("unique state");
        state_mut.file_watch = live;
        let state = Arc::new(state_mut);

        init_disk_watch_subscriptions(state.clone()).await;

        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            state.disk_changed.notified(),
        )
        .await
        .expect("bootstrap wake must fire after init returns");

        let file_watch = state.file_watch.clone();
        let fresh = tokio::task::spawn_blocking(move || load_all_instances(&file_watch))
            .await
            .expect("join")
            .expect("load");
        reload_state_instances_from_disk(&state, fresh, StatusSource::DiskOnly).await;

        let instances = state.instances.read().await;
        assert!(
            instances.iter().any(|i| i.title == "pre-init"),
            "bootstrap wake plus reload must surface writes that landed before init returned"
        );
    }

    // #1874 / #1875: a confirmed snapshot clears a reported telemetry counter
    // (the create counter and the web/acp open counts all share this path)
    // by exactly the value it reported, so an increment that arrives during the
    // in-flight send survives into the next snapshot instead of being reset away.
    #[test]
    fn reported_count_decrement_preserves_concurrent_increments() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let counter = AtomicU32::new(5);
        // The snapshot reported the 5 increments seen at build time.
        let reported = counter.load(Ordering::Relaxed);
        // One more lands while the snapshot is in flight.
        counter.fetch_add(1, Ordering::Relaxed);
        // The confirmed send clears only what it reported.
        decrement_reported_count(&counter, reported);
        assert_eq!(
            counter.load(Ordering::Relaxed),
            1,
            "the increment that arrived during the send must be retained"
        );
    }

    #[test]
    fn reported_count_decrement_is_noop_for_zero() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let counter = AtomicU32::new(3);
        decrement_reported_count(&counter, 0);
        assert_eq!(counter.load(Ordering::Relaxed), 3);
    }

    // #1883: the per-form-factor counters dedup repeated same-class opens to a
    // single was-seen entry, and the confirmed-send decrement subtracts exactly
    // what was reported so a class opened during an in-flight send survives.
    #[test]
    fn form_factor_counters_dedup_and_preserve_in_flight_opens() {
        use crate::telemetry::WebClientFormFactor::{Desktop, MobilePwa};

        let counters = FormFactorCounters::default();
        // Two desktop opens and one mobile-PWA open before the snapshot builds.
        counters.increment(Desktop);
        counters.increment(Desktop);
        counters.increment(MobilePwa);

        let reported = counters.read();
        // Repeated same-class pings collapse to one was-seen entry on the wire.
        let map = reported.seen_map();
        assert_eq!(map.get("desktop"), Some(&true));
        assert_eq!(map.get("mobile_pwa"), Some(&true));
        assert_eq!(map.get("mobile"), None, "unseen classes are absent");
        assert_eq!(map.len(), 2);

        // A mobile-PWA open lands while the snapshot is in flight.
        counters.increment(MobilePwa);
        // The confirmed send clears only the reported counts.
        counters.decrement(&reported);

        let after = counters.read();
        assert_eq!(after.get(Desktop), 0, "reported desktop opens cleared");
        assert_eq!(
            after.get(MobilePwa),
            1,
            "the open that arrived during the send must be retained"
        );
    }

    // #1888: the same decrement path carries the structured-interaction counters,
    // so an interaction that lands mid-send must survive the clear (the plan
    // mode counter shown here, which the snapshot reports as the bool count>0).
    #[test]
    fn reported_count_decrement_preserves_concurrent_structured_interaction() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let plan_mode = AtomicU32::new(2);
        let reported = plan_mode.load(Ordering::Relaxed);
        plan_mode.fetch_add(1, Ordering::Relaxed);
        decrement_reported_count(&plan_mode, reported);
        assert_eq!(plan_mode.load(Ordering::Relaxed), 1);
    }

    // The decrement saturates rather than underflow-wrapping the AtomicU32, so a
    // hypothetical future refactor that detaches sends (double-clearing a
    // counter) degrades to zero instead of jumping to u32::MAX.
    #[test]
    fn reported_count_decrement_saturates_below_zero() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let counter = AtomicU32::new(2);
        decrement_reported_count(&counter, 5);
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[cfg(feature = "serve")]
    #[test]
    fn derive_acp_status_maps_terminal_events() {
        use crate::acp::approvals::{ApprovalDecision, Nonce};
        use crate::acp::permissions::build_approval;
        use crate::acp::state::ToolCall;
        use crate::acp::Event;
        let tool_call = ToolCall {
            id: "t".into(),
            name: "shell".into(),
            kind: "execute".into(),
            args_preview: "{}".into(),
            started_at: chrono::Utc::now(),
            parent_tool_call_id: None,
            memory_recall: None,
            diffs: Vec::new(),
        };
        assert_eq!(
            derive_acp_status(&Event::UserPromptSent {
                text: "hi".into(),
                attachments: Vec::new(),
            }),
            Some(StatusIntent::Set(Status::Running))
        );
        assert_eq!(
            derive_acp_status(&Event::ApprovalRequested {
                approval: build_approval(tool_call.clone()),
            }),
            Some(StatusIntent::Set(Status::Waiting))
        );
        assert_eq!(
            derive_acp_status(&Event::ApprovalResolved {
                nonce: Nonce("x".into()),
                decision: ApprovalDecision::Allow,
            }),
            Some(StatusIntent::Set(Status::Running))
        );
        // A pending elicitation blocks the turn on the user just like an
        // approval, so the sidebar dot must go yellow (Waiting) and recover
        // to Running on resolution.
        let elicitation = crate::acp::elicitations::Elicitation {
            nonce: Nonce("e-1".into()),
            message: "Pick".into(),
            title: None,
            description: None,
            tool_call_id: None,
            questions: Vec::new(),
            requested_at: chrono::Utc::now(),
            resolved: None,
        };
        assert_eq!(
            derive_acp_status(&Event::ElicitationRequested { elicitation }),
            Some(StatusIntent::Set(Status::Waiting))
        );
        assert_eq!(
            derive_acp_status(&Event::ElicitationResolved {
                nonce: Nonce("e-1".into()),
                outcome: crate::acp::elicitations::ElicitationOutcome::Accepted,
                answers: Vec::new(),
            }),
            Some(StatusIntent::Set(Status::Running))
        );
        assert_eq!(
            derive_acp_status(&Event::Stopped {
                reason: "prompt_complete".into()
            }),
            Some(StatusIntent::Set(Status::Idle))
        );
        // Rate-limit park: NOT an error; sidebar stays grey, the
        // dedicated RateLimit banner carries the reset time. See #1281.
        assert_eq!(
            derive_acp_status(&Event::Stopped {
                reason: "rate_limited".into()
            }),
            Some(StatusIntent::Set(Status::Idle))
        );
        assert_eq!(
            derive_acp_status(&Event::AgentStartupError {
                message: "boom".into()
            }),
            Some(StatusIntent::Set(Status::Error))
        );
        // AcpSessionAssigned heals an Error banner only — never
        // clobbers an in-progress Running/Waiting turn.
        assert_eq!(
            derive_acp_status(&Event::AcpSessionAssigned {
                acp_session_id: "uuid".into()
            }),
            Some(StatusIntent::HealError)
        );
        // Rate-limit auto-resume breadcrumb heals like AcpSessionAssigned:
        // the worker is coming back, so clear a sticky error without
        // clobbering an in-progress turn. See #1722.
        assert_eq!(
            derive_acp_status(&Event::RateLimitAutoResumed {
                resets_at: chrono::Utc::now()
            }),
            Some(StatusIntent::HealError)
        );
    }

    #[cfg(feature = "serve")]
    #[test]
    fn derive_acp_session_change_extracts_assigned_id() {
        use crate::acp::Event;
        let ev = Event::AcpSessionAssigned {
            acp_session_id: "uuid-1234".into(),
        };
        assert_eq!(
            derive_acp_session_change(&ev),
            Some(AcpSessionChange::Assigned("uuid-1234".into()))
        );
    }

    #[cfg(feature = "serve")]
    #[test]
    fn derive_acp_session_change_extracts_reset_reason() {
        use crate::acp::Event;
        let ev = Event::SessionContextReset {
            reason: "session/load failed: bad id".into(),
        };
        assert_eq!(
            derive_acp_session_change(&ev),
            Some(AcpSessionChange::Reset(
                "session/load failed: bad id".into()
            ))
        );
    }

    #[cfg(feature = "serve")]
    #[test]
    fn derive_acp_session_change_ignores_unrelated_events() {
        use crate::acp::Event;
        assert_eq!(
            derive_acp_session_change(&Event::AgentMessageChunk { text: "x".into() }),
            None
        );
        assert_eq!(
            derive_acp_session_change(&Event::Stopped {
                reason: "prompt_complete".into()
            }),
            None
        );
        assert_eq!(derive_acp_session_change(&Event::ThinkingStarted), None);
    }

    #[cfg(feature = "serve")]
    #[test]
    fn derive_acp_status_running_on_agent_activity() {
        use crate::acp::state::ToolCall;
        use crate::acp::Event;
        // A turn that resumes agent-side (fired ScheduleWakeup, background
        // TaskOutput notification) streams only these events, never a
        // UserPromptSent. They must drive Running so the sidebar dot recovers.
        assert_eq!(
            derive_acp_status(&Event::AgentMessageChunk { text: "x".into() }),
            Some(StatusIntent::Set(Status::Running))
        );
        assert_eq!(
            derive_acp_status(&Event::ThinkingStarted),
            Some(StatusIntent::Set(Status::Running))
        );
        assert_eq!(
            derive_acp_status(&Event::ToolCallStarted {
                tool_call: ToolCall {
                    id: "t".into(),
                    name: "shell".into(),
                    kind: "execute".into(),
                    args_preview: "{}".into(),
                    started_at: chrono::Utc::now(),
                    parent_tool_call_id: None,
                    memory_recall: None,
                    diffs: Vec::new(),
                },
            }),
            Some(StatusIntent::Set(Status::Running))
        );
        // ThinkingEnded is a sub-phase terminator, not a work signal; leaving
        // it None avoids needless intents (ThinkingStarted already set Running).
        assert_eq!(derive_acp_status(&Event::ThinkingEnded), None);
    }

    // --- #2248: a structured session must heal out of a stale Stopped ---

    #[cfg(feature = "serve")]
    fn stopped_structured_instance() -> Instance {
        let mut inst = Instance::new("s", "/tmp/s");
        inst.view = crate::session::View::Structured;
        inst.status = Status::Stopped;
        inst
    }

    #[cfg(feature = "serve")]
    fn apply(inst: &mut Instance, intent: StatusIntent) {
        let tx = broadcast::channel(8).0;
        apply_status_intent(inst, Some(intent), &tx);
    }

    #[cfg(feature = "serve")]
    #[test]
    fn heal_error_wakes_a_stopped_session() {
        // AcpSessionAssigned / RateLimitAutoResumed -> HealError: a fresh
        // worker attached, so a stale Stopped from idle-reap or a prior
        // manual stop must heal. This is the #2248 trap: pre-fix the guard
        // froze Stopped and the dot stayed grey through a live turn.
        let mut inst = stopped_structured_instance();
        apply(&mut inst, StatusIntent::HealError);
        assert_eq!(inst.status, Status::Idle);
        // The UserPromptSent that follows the respawn then drives Running.
        apply(&mut inst, StatusIntent::Set(Status::Running));
        assert_eq!(inst.status, Status::Running);
    }

    #[cfg(feature = "serve")]
    #[test]
    fn agent_activity_wakes_an_idle_session_after_a_fired_wakeup() {
        // A session that paused on ScheduleWakeup sits Idle. When the wake
        // fires the turn resumes agent-side with activity events (no
        // UserPromptSent), so the activity-derived Set(Running) must flip the
        // dot green instead of leaving it grey.
        let mut inst = stopped_structured_instance();
        inst.status = Status::Idle;
        apply(&mut inst, StatusIntent::Set(Status::Running));
        assert_eq!(inst.status, Status::Running);
        assert_eq!(inst.idle_entered_at, None);
    }

    #[cfg(feature = "serve")]
    #[test]
    fn heal_error_still_heals_a_sticky_error() {
        let mut inst = stopped_structured_instance();
        inst.status = Status::Error;
        apply(&mut inst, StatusIntent::HealError);
        assert_eq!(inst.status, Status::Idle);
    }

    #[cfg(feature = "serve")]
    #[test]
    fn trailing_set_intents_do_not_wake_a_stopped_session() {
        // A deliberate Stop, or a session mid-stop, keeps emitting acp
        // events for a few ticks. None of those Set intents may revive it,
        // or the chain Stopped -> Running -> Idle would strand a deliberate
        // Stop on Idle.
        for target in [Status::Running, Status::Waiting, Status::Idle] {
            let mut inst = stopped_structured_instance();
            apply(&mut inst, StatusIntent::Set(target));
            assert_eq!(
                inst.status,
                Status::Stopped,
                "target {target:?} woke Stopped"
            );
        }
    }

    #[cfg(feature = "serve")]
    #[test]
    fn deleting_and_creating_block_every_intent() {
        for terminal in [Status::Deleting, Status::Creating] {
            let mut inst = stopped_structured_instance();
            inst.status = terminal;
            apply(&mut inst, StatusIntent::Set(Status::Running));
            assert_eq!(inst.status, terminal);
            apply(&mut inst, StatusIntent::HealError);
            assert_eq!(inst.status, terminal);
        }
    }

    #[cfg(feature = "serve")]
    #[tokio::test]
    async fn seed_unblocks_a_stopped_session_with_an_in_flight_turn() {
        use crate::acp::Event;
        // Daemon restart: session persisted Stopped, but the last lifecycle
        // event was a UserPromptSent (a turn was in flight when the prior
        // daemon died). Seed must reflect the live turn, not the stale dot.
        let inst = stopped_structured_instance();
        let id = inst.id.clone();
        let state = test_support::build_test_app_state(vec![inst]);
        state
            .acp_event_store
            .record(
                &id,
                1,
                &Event::UserPromptSent {
                    text: "go".into(),
                    attachments: Vec::new(),
                },
            )
            .expect("record");
        seed_acp_statuses(state.clone()).await;
        assert_eq!(state.instances.read().await[0].status, Status::Running);
    }

    #[cfg(feature = "serve")]
    #[tokio::test]
    async fn seed_preserves_a_deliberate_stop_across_restart() {
        use crate::acp::Event;
        // Latest event is a Stopped (clean / deliberate stop), so the seed
        // leaves the persisted Stopped intact rather than downgrading it.
        let inst = stopped_structured_instance();
        let id = inst.id.clone();
        let state = test_support::build_test_app_state(vec![inst]);
        state
            .acp_event_store
            .record(
                &id,
                1,
                &Event::Stopped {
                    reason: "prompt_complete".into(),
                },
            )
            .expect("record");
        seed_acp_statuses(state.clone()).await;
        assert_eq!(state.instances.read().await[0].status, Status::Stopped);
    }

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

    #[test]
    fn classify_ip_recognizes_tailscale_cgnat() {
        use std::net::Ipv4Addr;
        // CGNAT range 100.64.0.0/10 = second octet 64..=127.
        assert_eq!(classify_ip(Ipv4Addr::new(100, 64, 0, 1)), IpKind::Tailscale);
        assert_eq!(
            classify_ip(Ipv4Addr::new(100, 100, 50, 50)),
            IpKind::Tailscale
        );
        assert_eq!(
            classify_ip(Ipv4Addr::new(100, 127, 255, 254)),
            IpKind::Tailscale
        );
        // Boundary: 100.63.x.x is NOT CGNAT, it's just regular public
        // space — classify as LAN so we still surface it (rare but
        // possible on a weird home network).
        assert_eq!(classify_ip(Ipv4Addr::new(100, 63, 0, 1)), IpKind::Lan);
        // Boundary: 100.128.x.x is also not CGNAT.
        assert_eq!(classify_ip(Ipv4Addr::new(100, 128, 0, 1)), IpKind::Lan);
    }

    #[test]
    fn classify_ip_recognizes_rfc1918_lan() {
        use std::net::Ipv4Addr;
        assert_eq!(classify_ip(Ipv4Addr::new(192, 168, 1, 42)), IpKind::Lan);
        assert_eq!(classify_ip(Ipv4Addr::new(10, 0, 0, 1)), IpKind::Lan);
        assert_eq!(classify_ip(Ipv4Addr::new(172, 16, 5, 10)), IpKind::Lan);
    }

    #[test]
    fn classify_ip_recognizes_loopback() {
        use std::net::Ipv4Addr;
        assert_eq!(classify_ip(Ipv4Addr::new(127, 0, 0, 1)), IpKind::Loopback);
        assert_eq!(classify_ip(Ipv4Addr::new(127, 1, 2, 3)), IpKind::Loopback);
    }

    #[test]
    fn ip_kind_ordering_prefers_tailscale() {
        // This is the "Tailscale first in QR" contract. If the sort order
        // ever flips, the user's phone would scan a LAN IP from cellular
        // and hit a timeout — regression test locks it in.
        let mut v = [IpKind::Loopback, IpKind::Lan, IpKind::Tailscale];
        v.sort();
        assert_eq!(v, [IpKind::Tailscale, IpKind::Lan, IpKind::Loopback]);
    }

    #[test]
    fn csp_parses_as_valid_header_value() {
        // Catches typos that would make the header unparseable.
        // security_headers() calls `.parse().unwrap()` at request time;
        // this test surfaces any regression at `cargo test` time instead.
        let parsed: axum::http::HeaderValue = CSP.parse().expect("CSP must parse");
        let rendered = parsed.to_str().expect("CSP must be ASCII");
        // Spot-check load-bearing directives so a future edit that
        // accidentally drops one fails loudly.
        for needle in [
            "default-src 'self'",
            "script-src 'self' 'wasm-unsafe-eval'",
            "img-src 'self' data: https://github.com https://avatars.githubusercontent.com https://raw.githubusercontent.com",
            "connect-src 'self' ws: wss:",
            "frame-ancestors 'none'",
        ] {
            assert!(
                rendered.contains(needle),
                "CSP is missing required directive fragment `{needle}`"
            );
        }
    }

    #[test]
    fn cleanup_defaults_cache_stale_within_ttl_is_false() {
        let cache = CleanupDefaultsCache {
            refreshed_at: std::time::Instant::now(),
            entries: std::collections::HashMap::new(),
        };
        assert!(!cache.stale());
    }

    #[test]
    fn cleanup_defaults_cache_stale_past_ttl_is_true() {
        let cache = CleanupDefaultsCache {
            refreshed_at: std::time::Instant::now()
                - CLEANUP_DEFAULTS_TTL
                - std::time::Duration::from_millis(1),
            entries: std::collections::HashMap::new(),
        };
        assert!(cache.stale());
    }

    #[tokio::test]
    async fn resolve_auth_mode_matches_about_precedence() {
        let token = TokenManager::new(Some("abc123".to_string()), Duration::from_secs(3600));
        let no_token = TokenManager::new(None, Duration::from_secs(3600));
        let passphrase = login::LoginManager::new(Some("hunter2"));
        let no_passphrase = login::LoginManager::new(None);

        // A token wins over a passphrase second factor when both are set.
        assert_eq!(resolve_auth_mode(&token, &passphrase).await, "token");
        assert_eq!(resolve_auth_mode(&token, &no_passphrase).await, "token");
        // No token but a passphrase reports passphrase auth.
        assert_eq!(
            resolve_auth_mode(&no_token, &passphrase).await,
            "passphrase"
        );
        // Neither configured is the security-relevant fully-open mode.
        assert_eq!(resolve_auth_mode(&no_token, &no_passphrase).await, "none");
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
    async fn token_manager_no_auth_mode() {
        let mgr = TokenManager::new(None, Duration::from_secs(3600));
        assert!(mgr.is_no_auth().await);
    }

    // Pins the `MissedTickBehavior::Delay` contract on `tokio::time::interval`;
    // the prod callsite (`status_poll_loop`) is not exercised by this test.
    #[tokio::test]
    async fn status_poll_loop_interval_delays_after_stall() {
        let period = Duration::from_millis(100);
        let mut interval = tokio::time::interval(period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        interval.tick().await;
        tokio::time::sleep(period * 4).await;
        interval.tick().await;

        let before = std::time::Instant::now();
        interval.tick().await;
        let gap = before.elapsed();

        assert!(
            gap >= Duration::from_millis(80),
            "second post-stall tick must wait ~period (Delay), got {gap:?}"
        );
    }
}
