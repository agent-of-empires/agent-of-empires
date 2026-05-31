//! Web dashboard for remote agent session access
//!
//! Provides an embedded axum web server that serves a responsive dashboard
//! for monitoring and interacting with agent sessions from any browser.

pub mod api;
pub mod auth;
#[cfg(feature = "serve")]
pub mod cockpit_reconciler;
#[cfg(feature = "serve")]
pub mod cockpit_ws;
pub mod login;
pub mod push;
pub mod push_send;
pub mod rate_limit;
pub mod tunnel;
pub mod ws;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use rust_embed::Embed;
use serde::Serialize;
use tokio::sync::{broadcast, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{info, Instrument};

use self::push::{PushState, StatusChange, STATUS_CHANNEL_CAPACITY};

#[cfg(feature = "serve")]
const COCKPIT_CHANNEL_CAPACITY: usize = 256;

/// Re-export of the broadcast frame defined in `crate::cockpit::protocol`,
/// kept under `crate::server::` so existing supervisor/WS call sites keep
/// resolving without churn. The canonical definition lives in protocol.rs
/// so the daemon and any client share a single source of truth.
#[cfg(feature = "serve")]
pub use crate::cockpit::protocol::CockpitBroadcastFrame;

use crate::session::Instance;
use crate::session::Status;
use crate::session::Storage;

use self::rate_limit::RateLimiter;

#[derive(Embed)]
#[folder = "web/dist/"]
struct StaticAssets;

// ── DeviceInfo ──────────────────────────────────────────────────────────────

/// A device that has connected to the dashboard.
#[derive(Clone, Serialize)]
pub struct DeviceInfo {
    pub ip: String,
    pub user_agent: String,
    pub first_seen: chrono::DateTime<chrono::Utc>,
    pub last_seen: chrono::DateTime<chrono::Utc>,
    pub request_count: u64,
}

// ── TokenManager ────────────────────────────────────────────────────────────

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

// ── AppState ────────────────────────────────────────────────────────────────

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

/// Shared application state accessible by all request handlers.
pub struct AppState {
    pub profile: String,
    pub read_only: bool,
    pub instances: RwLock<Vec<Instance>>,
    pub token_manager: Arc<TokenManager>,
    pub login_manager: Arc<login::LoginManager>,
    pub rate_limiter: Arc<RateLimiter>,
    pub devices: RwLock<Vec<DeviceInfo>>,
    pub behind_tunnel: bool,
    /// Per-instance mutex guarding mutations that must not interleave
    /// (e.g. `ensure_session` decide-and-restart). Entries are created on
    /// first use and live for the lifetime of the process — there are only
    /// as many as the user has sessions.
    pub instance_locks: RwLock<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Suppression set for the startup-recovery cascade. While an entry is
    /// present and younger than `recovery::RECENTLY_RESTARTED_TTL`, the
    /// `status_poll_loop` skips `update_status_with_metadata` for that
    /// instance and surfaces `Status::Starting` instead. Without this,
    /// `last_start_time` (which is `#[serde(skip)]`) is lost on the loop's
    /// `load_all_instances` reload, and a freshly-recovered session
    /// transitions to `Status::Error` for up to 8 seconds while the agent
    /// is still settling. Periodically GC'd by a background task.
    pub recently_restarted: crate::session::recovery::RecentlyRestarted,
    /// Cached per-profile cleanup defaults for the delete dialog, with a
    /// timestamp so we re-resolve after config changes (see
    /// `CLEANUP_DEFAULTS_TTL`).
    pub cleanup_defaults_cache: RwLock<CleanupDefaultsCache>,
    /// Cached remote owner per repo path. Remote owners don't change, so
    /// entries live for the lifetime of the process.
    pub remote_owner_cache: RwLock<std::collections::HashMap<String, Option<String>>>,
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
    /// Broadcasts cockpit events to subscribed WebSocket clients. The
    /// channel carries `(session_id, serialized event JSON)` frames so
    /// clients can filter by session. Empty when no clients are
    /// connected; senders never need to check before emitting.
    #[cfg(feature = "serve")]
    pub cockpit_events_tx: broadcast::Sender<CockpitBroadcastFrame>,
    /// Disk-backed cockpit event log. The single source of truth for
    /// replay: `ChannelSink::publish` writes here on every event, the
    /// WS-on-connect drain reads from here, the `/cockpit/replay` REST
    /// endpoint reads from here, and `Supervisor::next_seqs` is seeded
    /// from here at startup so a fresh publish gets `max_seq + 1`
    /// rather than 1.
    #[cfg(feature = "serve")]
    pub cockpit_event_store: Arc<crate::cockpit::event_store::EventStore>,
    /// Mirror of `config.cockpit.enabled`. Initialized at startup from
    /// `config.toml`; the `PATCH /api/cockpit/master` endpoint persists
    /// to disk and updates this atomic so the reconciler and REST gates
    /// pick up the new value without an `aoe serve` restart. When false,
    /// the reconciler skips auto-spawn and every cockpit-spawning REST
    /// path refuses with 503.
    #[cfg(feature = "serve")]
    pub cockpit_master_enabled: std::sync::atomic::AtomicBool,
    /// Owns the per-session ACP agent subprocesses.
    #[cfg(feature = "serve")]
    pub cockpit_supervisor:
        Arc<crate::cockpit::supervisor::Supervisor<crate::cockpit::supervisor::ChannelSink>>,
    /// Per-tmux-session primary WebSocket client. Maps tmux session name
    /// to the client ID that most recently sent keyboard input. Only the
    /// primary client's resize messages are applied to its PTY, preventing
    /// multiple browser viewports from fighting over the tmux window size.
    pub session_primaries: Arc<RwLock<std::collections::HashMap<String, String>>>,
    /// Per-tmux-session refcount of clients currently asking the pane's
    /// process tree to be paused (SIGSTOP). Incremented by `pause_output`,
    /// decremented by `resume_output` and on WebSocket disconnect. The
    /// pane's process is SIGSTOP-ed on 0→N transitions and SIGCONT-ed on
    /// N→0, so two mobile clients scrolling concurrently don't have one's
    /// `resume_output` un-pause the other's scrollback read.
    pub session_pause_counts: Arc<tokio::sync::Mutex<std::collections::HashMap<String, u32>>>,
    /// Epoch-millis timestamp of the most recent authenticated API request.
    /// Updated by auth middleware on every successful auth. The push consumer
    /// checks this to suppress notifications when someone is actively using
    /// the web dashboard (on any device).
    pub last_web_activity: std::sync::atomic::AtomicI64,
    /// Resolved when the daemon receives SIGINT/SIGTERM/SIGHUP. Long-lived
    /// handlers (cockpit WS, terminal WS) clone this and `select!` on
    /// `cancelled()` so they exit promptly instead of holding axum's
    /// graceful drain open until the browser tab decides to disconnect.
    /// See #1198.
    pub shutdown: CancellationToken,
}

impl AppState {
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

// ── Server ──────────────────────────────────────────────────────────────────

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

    let instances = load_all_instances()?;

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
    let login_manager = Arc::new(login::LoginManager::new(passphrase));
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
    let config = crate::session::profile_config::resolve_config_or_warn(profile);
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
    let cockpit_events_tx = broadcast::channel(COCKPIT_CHANNEL_CAPACITY).0;
    #[cfg(feature = "serve")]
    let cockpit_master_enabled = std::sync::atomic::AtomicBool::new(config.cockpit.enabled);
    #[cfg(feature = "serve")]
    let cockpit_event_store = {
        let app_dir =
            crate::session::get_app_dir().context("cockpit event store: resolve app dir")?;
        let db_path = app_dir.join("cockpit_events.db");
        Arc::new(
            crate::cockpit::event_store::EventStore::open(
                &db_path,
                config.cockpit.replay_events as usize,
            )
            .context("cockpit event store: open")?,
        )
    };
    #[cfg(feature = "serve")]
    let cockpit_supervisor = {
        // Approval pushes are dispatched from `cockpit_event_listener`,
        // which subscribes to the broadcast that ChannelSink::publish
        // feeds and has `Arc<AppState>` in scope without a closure
        // dance through the supervisor. See #1038.
        let sink = std::sync::Arc::new(crate::cockpit::supervisor::ChannelSink {
            tx: cockpit_events_tx.clone(),
            event_store: cockpit_event_store.clone(),
        });
        let supervisor =
            std::sync::Arc::new(crate::cockpit::supervisor::Supervisor::with_capacity(
                sink,
                config.cockpit.max_concurrent_workers,
            ));
        // Seed the seq counter from disk so fresh publishes don't
        // collide with restored history. Without this, after a
        // restart the first publish would be seq=1 — duplicate of
        // the row already on disk — and INSERT OR IGNORE would
        // silently drop it.
        supervisor.hydrate_seqs(cockpit_event_store.all_session_seqs());
        supervisor
    };

    let state = Arc::new(AppState {
        profile: profile.to_string(),
        read_only,
        instances: RwLock::new(instances),
        token_manager: Arc::clone(&token_manager),
        login_manager: Arc::clone(&login_manager),
        rate_limiter: Arc::clone(&rate_limiter),
        devices: RwLock::new(Vec::new()),
        behind_tunnel: remote || behind_proxy,
        instance_locks: RwLock::new(std::collections::HashMap::new()),
        recently_restarted: crate::session::recovery::new_recently_restarted(),
        cleanup_defaults_cache: RwLock::new(CleanupDefaultsCache {
            // Seed with an already-stale timestamp so the first request
            // forces a fresh resolve instead of handing out an empty map.
            refreshed_at: std::time::Instant::now() - CLEANUP_DEFAULTS_TTL,
            entries: std::collections::HashMap::new(),
        }),
        remote_owner_cache: RwLock::new(std::collections::HashMap::new()),
        session_primaries: Arc::new(RwLock::new(std::collections::HashMap::new())),
        session_pause_counts: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
        status_tx: broadcast::channel(STATUS_CHANNEL_CAPACITY).0,
        #[cfg(feature = "serve")]
        cockpit_events_tx: cockpit_events_tx.clone(),
        #[cfg(feature = "serve")]
        cockpit_event_store: cockpit_event_store.clone(),
        #[cfg(feature = "serve")]
        cockpit_master_enabled,
        #[cfg(feature = "serve")]
        cockpit_supervisor: cockpit_supervisor.clone(),
        push: push_state,
        push_enabled,
        web_config: config.web.clone(),
        last_web_activity: std::sync::atomic::AtomicI64::new(0),
        shutdown: CancellationToken::new(),
    });

    let app = build_router(state.clone());

    // Cockpit workers for persisted sessions get auto-spawned by the
    // reconciler in `status_poll_loop`. The poll interval's first tick
    // fires immediately, so on cold startup this is equivalent to the
    // old in-place loop here, while also covering sessions added via
    // `aoe add --cockpit` while serve is already running. The
    // reconciler short-circuits when `cockpit.enabled = false`.

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

    // Seed cockpit sessions' status from the on-disk event log before
    // any background task runs. The status_poll_loop overlay reads
    // `state.instances` and the cockpit_event_listener only sees
    // live transitions, so a session that was mid-turn when the
    // previous daemon died otherwise renders Idle until the next
    // lifecycle event arrives. See #1103.
    seed_cockpit_statuses(state.clone()).await;

    // Two-phase startup recovery. Phase A runs synchronously (acquire
    // lock, snapshot candidates, mark them in `recently_restarted`) so
    // that the marks are in place before `status_poll_loop` is spawned
    // and its first tick fires; otherwise the first poll could observe
    // missing tmux state and broadcast a phantom Idle->Error transition.
    // Phase B (the cascade workers) runs in a spawned task and holds
    // the lock until done.
    let recovery_inputs = daemon_startup_recovery_mark(state.clone()).await;

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

    if let Some((lock, candidates)) = recovery_inputs {
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

    // Cockpit broadcast listener: a single subscriber that handles
    // every in-process consumer of cockpit events. Status mirroring
    // (sidebar dot, push-notification source) and ACP-session-id
    // persistence (so `session/load` works across restart) used to be
    // two separate subscribers, which doubled the broadcast clone
    // count and locked `state.instances` twice for the events that
    // matter to both (e.g. AcpSessionAssigned).
    {
        let listener_state = state.clone();
        crate::task_util::spawn_supervised(
            "server.cockpit_event_listener",
            crate::task_util::PanicPolicy::Log,
            async move {
                cockpit_event_listener(listener_state).await;
            },
        );
    }

    // Push-notification consumer: subscribes to status_tx, applies
    // dwell + cooldown, sends pushes. No-op when push_state is None
    // (feature disabled via web.notifications_enabled=false).
    push::spawn_consumer(state.clone());

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
    //   1. Cancels `state.shutdown` so long-lived WS handlers (cockpit +
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
        tokio::spawn(async {
            tokio::time::sleep(SHUTDOWN_GRACE).await;
            tracing::warn!(
                target: "shutdown",
                grace_secs = SHUTDOWN_GRACE.as_secs(),
                "graceful shutdown exceeded grace window, forcing exit"
            );
            // Force-exit skips the post-`axum::serve` cleanup block below
            // (cockpit detach, tunnel SIGTERM of cloudflared, removal of
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

    // Detach (but do NOT kill) every cockpit ACP worker. The per-session
    // `aoe __cockpit-runner` shims outlive this daemon: a fresh
    // `aoe serve` reattaches via the reconciler on startup, so in-flight
    // turns survive `aoe serve --stop`. To actually terminate workers,
    // use `aoe cockpit stop [--all]`.
    cockpit_supervisor.detach_all().await;

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
        .route(
            "/api/workspace-ordering",
            put(api::update_workspace_ordering),
        )
        .route(
            "/api/sessions/{id}",
            patch(api::rename_session).delete(api::delete_session),
        )
        .route(
            "/api/sessions/{id}/diff/files",
            get(api::session_diff_files),
        )
        .route("/api/sessions/{id}/diff/file", get(api::session_diff_file))
        .route("/api/sessions/{id}/ensure", post(api::ensure_session))
        .route("/api/sessions/{id}/send", post(api::send_message))
        .route("/api/sessions/{id}/output", get(api::read_output))
        .route(
            "/api/sessions/{id}/notifications",
            patch(api::update_session_notifications),
        )
        .route(
            "/api/sessions/{id}/diff-base",
            patch(api::update_session_diff_base),
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
        .route("/api/sessions/{id}/terminal", post(api::ensure_terminal))
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
        .route("/api/projects/{name}", delete(api::delete_project))
        .route("/api/docker/status", get(api::docker_status))
        // Settings + themes
        .route(
            "/api/settings",
            get(api::get_settings).patch(api::update_settings),
        )
        .route("/api/themes", get(api::list_themes))
        .route("/api/themes/{name}", get(api::get_resolved_theme))
        .route("/api/theme/current", get(api::get_current_theme))
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
        // Devices
        .route("/api/devices", get(api::list_devices))
        // About (version, auth status, read-only state)
        .route("/api/about", get(api::get_about))
        // Update status (latest release, available flag)
        .route("/api/system/update-status", get(api::get_update_status))
        .route(
            "/api/log-level",
            get(api::get_log_level).patch(api::patch_log_level),
        )
        .route("/api/client-log", post(api::post_client_log))
        // Terminal WebSockets
        .route("/sessions/{id}/ws", get(ws::terminal_ws))
        .route("/sessions/{id}/terminal/ws", get(ws::paired_terminal_ws))
        .route(
            "/sessions/{id}/container-terminal/ws",
            get(ws::container_terminal_ws),
        );

    #[cfg(feature = "serve")]
    let app = app
        .route("/sessions/{id}/cockpit/ws", get(cockpit_ws::cockpit_ws))
        .route("/api/sessions/{id}/cockpit/spawn", post(api::spawn_cockpit))
        .route("/api/sessions/{id}/cockpit", delete(api::shutdown_cockpit))
        .route(
            "/api/sessions/{id}/cockpit/switch-agent",
            post(api::switch_cockpit_agent),
        )
        .route(
            "/api/sessions/{id}/cockpit/prompt",
            post(api::cockpit_prompt),
        )
        .route(
            "/api/sessions/{id}/cockpit/prompt/diff-comments",
            post(api::cockpit_prompt_diff_comments),
        )
        .route(
            "/api/sessions/{id}/cockpit/cancel",
            post(api::cockpit_cancel),
        )
        .route(
            "/api/sessions/{id}/cockpit/force_end_turn",
            post(api::cockpit_force_end_turn),
        )
        .route("/api/sessions/{id}/cockpit/files", get(api::cockpit_files))
        .route(
            "/api/sessions/{id}/cockpit/worker-log",
            get(api::cockpit_worker_log),
        )
        .route(
            "/api/sessions/{id}/cockpit/replay",
            get(api::cockpit_replay),
        )
        .route(
            "/api/sessions/{id}/cockpit/context-primer",
            get(api::cockpit_context_primer),
        )
        .route(
            "/api/sessions/{id}/cockpit/mode",
            post(api::cockpit_set_mode),
        )
        .route(
            "/api/sessions/{id}/cockpit/config-option",
            post(api::cockpit_set_config_option),
        )
        .route(
            "/api/sessions/{id}/cockpit/enable",
            post(api::cockpit_enable),
        )
        .route(
            "/api/sessions/{id}/cockpit/disable",
            post(api::cockpit_disable),
        )
        .route(
            "/api/sessions/{id}/cockpit/approvals/{nonce}",
            post(api::resolve_approval),
        )
        .route("/api/cockpit/master", patch(api::set_cockpit_master))
        .route("/api/cockpit/agents", get(api::list_cockpit_agents));

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
/// - `img-src 'self' data: https://github.com https://avatars.githubusercontent.com`:
///   repo-owner avatars are loaded from `github.com/{user}.png` which 302s
///   to `avatars.githubusercontent.com`; CSP checks both URLs across the
///   redirect, so both hosts must be allowed. `data:` covers inline icons.
/// - `font-src 'self'`: Geist fonts are bundled under /fonts/.
/// - `connect-src 'self' ws: wss:`: REST + PTY WebSocket to same origin.
/// - `frame-ancestors 'none'`: CSP-native equivalent of X-Frame-Options.
/// - `base-uri 'self'`, `form-action 'self'`, `object-src 'none'`: tighten
///   the usual attack surfaces on injection bugs.
const CSP: &str = "default-src 'self'; \
    script-src 'self' 'wasm-unsafe-eval'; \
    style-src 'self' 'unsafe-inline'; \
    img-src 'self' data: https://github.com https://avatars.githubusercontent.com; \
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

async fn serve_index(uri: axum::http::Uri) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;

    let path = uri.path().trim_start_matches('/');
    if !path.is_empty() && path != "index.html" && path.contains('.') {
        if let Some(file) = StaticAssets::get(path) {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            return (
                axum::http::StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, mime.as_ref().to_string())],
                file.data.to_vec(),
            )
                .into_response();
        }
    }
    serve_embedded_file("index.html")
}

async fn serve_asset(
    axum::extract::Path(path): axum::extract::Path<String>,
) -> impl axum::response::IntoResponse {
    serve_embedded_file(&format!("assets/{}", path))
}

async fn serve_public_file(uri: axum::http::Uri) -> impl axum::response::IntoResponse {
    // Strip leading slash to match rust-embed paths
    let path = uri.path().trim_start_matches('/');
    serve_embedded_file(path)
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

fn serve_embedded_file(path: &str) -> axum::response::Response {
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;

    match StaticAssets::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, mime.as_ref().to_string())],
                file.data.to_vec(),
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "Not found").into_response(),
    }
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
fn load_all_instances() -> anyhow::Result<Vec<Instance>> {
    let profiles = crate::session::list_profiles().unwrap_or_default();
    let mut all = Vec::new();
    for profile in &profiles {
        if let Ok(storage) = Storage::new(profile) {
            if let Ok(mut instances) = storage.load() {
                for inst in &mut instances {
                    inst.source_profile = profile.clone();
                }
                all.extend(instances);
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
    fresh.last_error = prior.last_error;
    fresh.session_id_poller = prior.session_id_poller;
    fresh.retroactive_capture_excludes = prior.retroactive_capture_excludes;
    fresh.last_acquired_existing_sid = prior.last_acquired_existing_sid;
    fresh
}

/// Background task that periodically refreshes session statuses. On each
/// tick, diffs pre- and post-refresh statuses and emits a `StatusChange`
/// on `state.status_tx` for every transition. Keeping the diff here,
/// rather than pushing it into `Instance::update_status_with_metadata`,
/// leaves the session module free of any broadcast-channel dependency
/// and keeps TUI/CLI callers unchanged.
async fn status_poll_loop(state: Arc<AppState>) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
    #[cfg(feature = "serve")]
    let mut attempted_cockpit_spawns: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    #[cfg(feature = "serve")]
    let mut last_idle_reap: Option<std::time::Instant> = None;
    loop {
        interval.tick().await;

        // Snapshot prior statuses so we can detect transitions without
        // holding the lock across the blocking tmux work.
        let prev: std::collections::HashMap<String, crate::session::Status> = {
            let instances = state.instances.read().await;
            instances.iter().map(|i| (i.id.clone(), i.status)).collect()
        };

        // Run blocking tmux subprocess calls in a dedicated thread.
        // Snapshot the suppression set BEFORE `batch_pane_metadata()` so
        // a worker that unmarks between the scrape and the per-instance
        // decision cannot combine "pane missing" metadata with a cleared
        // mark and re-emit the phantom Error transition the suppression
        // exists to prevent.
        let suppressed_ids =
            crate::session::recovery::snapshot_recently_restarted(&state.recently_restarted);
        let updated = tokio::task::spawn_blocking(move || {
            let mut instances = load_all_instances().unwrap_or_default();

            crate::tmux::refresh_session_cache();
            let pane_metadata = crate::tmux::batch_pane_metadata().unwrap_or_default();

            for inst in &mut instances {
                if suppressed_ids.contains(&inst.id) {
                    // Suppress the status update: a recovery cascade just
                    // ran for this id, and `last_start_time` was lost on
                    // the disk reload above. Surfacing `Status::Error`
                    // ("tmux session is gone") here would broadcast a
                    // phantom transition before the agent has finished
                    // settling. The TTL window is sized to cover the
                    // worst-case cascade + cold-start latency.
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
            // The poll loop refreshes from disk every tick, but the
            // cockpit_status_listener's status writes are in-memory only
            // (status is derived from live ACP events, not persisted
            // per-event). Without re-applying them here the disk reload
            // would silently revert cockpit sessions to whatever Status
            // was last persisted (typically Idle from spawn) and the
            // sidebar dot would never turn green after a prompt.
            #[cfg(feature = "serve")]
            {
                type CockpitStatusOverlay = std::collections::HashMap<
                    String,
                    (
                        Status,
                        Option<chrono::DateTime<chrono::Utc>>,
                        Option<chrono::DateTime<chrono::Utc>>,
                    ),
                >;
                let overlay: CockpitStatusOverlay = {
                    let state_instances = state.instances.read().await;
                    state_instances
                        .iter()
                        .filter(|i| i.cockpit_mode)
                        .map(|i| {
                            (
                                i.id.clone(),
                                (i.status, i.last_accessed_at, i.idle_entered_at),
                            )
                        })
                        .collect()
                };
                for inst in &mut instances {
                    if !inst.cockpit_mode {
                        continue;
                    }
                    if let Some((status, last_accessed, idle_entered)) = overlay.get(&inst.id) {
                        inst.status = *status;
                        inst.last_accessed_at = *last_accessed;
                        inst.idle_entered_at = *idle_entered;
                    }
                }
            }

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
            // Merge by id rather than blind-replace: preserves the
            // in-memory-only `#[serde(skip)]` runtime state on Instance
            // (last_error, last_start_time, last_error_check,
            // session_id_poller, retroactive_capture_excludes) that the
            // disk reload otherwise resets to default every 2 s.
            // Additions on disk surface here; ids absent from disk are
            // dropped, matching the prior wholesale-replace semantics
            // for create/delete propagation.
            {
                let mut current = state.instances.write().await;
                let mut by_id: std::collections::HashMap<String, Instance> = current
                    .drain(..)
                    .map(|inst| (inst.id.clone(), inst))
                    .collect();
                let mut merged = Vec::with_capacity(instances.len());
                for fresh in instances {
                    if let Some(prior) = by_id.remove(&fresh.id) {
                        merged.push(merge_runtime_fields(prior, fresh));
                    } else {
                        merged.push(fresh);
                    }
                }
                *current = merged;
            }

            #[cfg(feature = "serve")]
            cockpit_reconciler::reconcile_cockpit_workers(
                &state,
                &mut attempted_cockpit_spawns,
                &mut last_idle_reap,
            )
            .await;
        }
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
            // `cockpit_mode` OR brought the tmux pane back. Without the
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
                    crate::session::recovery::unmark_recently_restarted(
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
                // Phase A pre-marked this id; without unmarking, the
                // status_poll_loop would suppress the real status for
                // the full TTL even though we are not running a cascade.
                crate::session::recovery::unmark_recently_restarted(
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
                    crate::session::recovery::unmark_recently_restarted(
                        &inst_state.recently_restarted,
                        &id,
                    );
                }
                Ok((mut updated, Err(e))) => {
                    tracing::warn!(
                        target: "session.startup_recovery",
                        instance_id = %id,
                        title = %title,
                        error = %e,
                        "recovery cascade failed",
                    );
                    // The cascade leaves last_error=None on every Err exit
                    // (no failure path sets it) and self.status as either
                    // `Status::Starting` (the common case: probe_settle
                    // returned Dead, or Tier-2 failed after finalize_launch
                    // ran at instance.rs:1403) or `Status::Idle` (rare:
                    // kill_clean failed, or Tier-1 start_with_size_opts
                    // failed before finalize_launch). In either case,
                    // without an explicit Error transition the next
                    // status_poll_loop tick falls through to
                    // update_status_with_metadata and generates a generic
                    // "tmux session is gone" message, hiding the
                    // cascade-specific error.
                    updated.status = crate::session::Status::Error;
                    updated.last_error = Some(format!("recovery cascade: {}", e));
                    // Stamp last_error_check so the in-memory error overlay
                    // in status_poll_loop arms the 30s stickiness in
                    // update_status_with_metadata_inner. Without this
                    // (#[serde(skip)] would otherwise leave it None on the
                    // next disk reload), the cascade-specific message is
                    // overwritten by the generic "tmux session is gone" on
                    // the very next poll tick.
                    updated.last_error_check = Some(std::time::Instant::now());
                    let mut instances = inst_state.instances.write().await;
                    if let Some(slot) = instances.iter_mut().find(|i| i.id == id) {
                        *slot = updated;
                    }
                    drop(instances);
                    // Release the suppression so the next poll respects the
                    // Error state instead of forcing Status::Starting for
                    // the rest of the TTL window.
                    crate::session::recovery::unmark_recently_restarted(
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
                    crate::session::recovery::unmark_recently_restarted(
                        &inst_state.recently_restarted,
                        &id,
                    );
                }
            }
        });
    }

    while tasks.join_next().await.is_some() {}
    drop(lock);
}

/// One task instead of two halves the broadcast clone count and locks
/// `state.instances` once per event instead of twice for the events
/// (e.g. `AcpSessionAssigned`) that both consumers care about.
#[cfg(feature = "serve")]
async fn cockpit_event_listener(state: Arc<AppState>) {
    let mut rx = state.cockpit_events_tx.subscribe();
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
                    target: "cockpit.event_listener",
                    skipped,
                    "broadcast lagged; status and acp_session_id may briefly desync"
                );
                continue;
            }
            // Closed: AppState dropped (shutdown). Exit cleanly.
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                tracing::debug!(
                    target: "cockpit.event_listener",
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
            crate::cockpit::state::Event::UserPromptSent { .. }
        ) {
            match state
                .cockpit_event_store
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
                        target: "cockpit.wakeup",
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
                        target: "cockpit.wakeup",
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
        if let crate::cockpit::state::Event::ApprovalRequested { approval } = frame.event.as_ref() {
            let state_for_push = state.clone();
            let session_id = frame.session_id.clone();
            let approval_title = approval.tool_call.name.clone();
            let destructive = approval.destructive;
            tokio::spawn(async move {
                cockpit_ws::trigger_approval_push(
                    &state_for_push,
                    &session_id,
                    &approval_title,
                    destructive,
                )
                .await;
            });
        }

        let status_intent = derive_cockpit_status(frame.event.as_ref());
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
            if !inst.cockpit_mode {
                continue;
            }

            apply_status_intent(inst, status_intent, &state.status_tx);
            apply_acp_session_change(inst, &frame.session_id, acp_change.as_ref())
        };

        // Persist `cockpit_acp_session_id` to disk if the field changed.
        // Sync FS (file copy + JSON write) goes through spawn_blocking
        // so the runtime stays responsive under large session lists.
        if let Some(profile) = profile_to_save {
            let session_id_for_log = frame.session_id.clone();
            let session_id_for_save = frame.session_id.clone();
            let profile_for_save = profile.clone();
            let acp_change_for_save = acp_change.clone();
            let save_result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                let storage = crate::session::Storage::new(&profile_for_save)?;
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
                        target: "cockpit.event_listener",
                        session = %session_id_for_log,
                        "save after acp_session_id update: {e}"
                    );
                }
                Err(join_err) => {
                    tracing::warn!(
                        target: "cockpit.event_listener",
                        session = %session_id_for_log,
                        "spawn_blocking join error during acp_session_id save: {join_err}"
                    );
                }
            }
        }
    }
}

/// Seed each cockpit-enabled session's `Instance.status` from the most
/// recent lifecycle event in the on-disk event log. Runs once at
/// daemon startup, before the status poll loop and the cockpit event
/// listener start, so a session that was mid-turn when the previous
/// daemon died doesn't render Idle until the next live event arrives.
/// Acts via the same `apply_status_intent` path as the live listener
/// so push subscribers and the broadcast channel see the seeded
/// transitions as ordinary StatusChange events. See #1103 (B).
#[cfg(feature = "serve")]
pub(crate) async fn seed_cockpit_statuses(state: Arc<AppState>) {
    let cockpit_ids: Vec<String> = state
        .instances
        .read()
        .await
        .iter()
        .filter(|i| i.cockpit_mode)
        .map(|i| i.id.clone())
        .collect();
    if cockpit_ids.is_empty() {
        return;
    }
    for id in cockpit_ids {
        let Some(event) = state.cockpit_event_store.latest_status_event(&id) else {
            continue;
        };
        let intent = derive_cockpit_status(&event);
        if intent.is_none() {
            continue;
        }
        let mut instances = state.instances.write().await;
        if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
            apply_status_intent(inst, intent, &state.status_tx);
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
    // Don't fight terminal lifecycle states. Cockpit events keep
    // arriving for a few ticks after a Stop/Delete, and we don't
    // want the spinner to flicker back to Running.
    if matches!(
        inst.status,
        Status::Stopped | Status::Deleting | Status::Creating
    ) {
        return;
    }
    let target = match intent {
        StatusIntent::Set(s) => s,
        // HealError: only move from Error → Idle. Skip when the
        // session is in a normal state so a respawn during an active
        // Running turn doesn't stop the spinner.
        StatusIntent::HealError => {
            if inst.status != Status::Error {
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
/// `cockpit_acp_session_id` survives daemon restart), or `None` if the
/// change was a no-op or no change was emitted.
#[cfg(feature = "serve")]
fn apply_acp_session_change(
    inst: &mut Instance,
    session_id: &str,
    change: Option<&AcpSessionChange>,
) -> Option<String> {
    match change? {
        AcpSessionChange::Assigned(new_id) => {
            if inst.cockpit_acp_session_id.as_deref() == Some(new_id.as_str()) {
                // Same id — already on disk, no need to rewrite.
                return None;
            }
            tracing::info!(
                target: "cockpit.event_listener",
                session = %session_id,
                acp_session_id = %new_id,
                "persisting agent-assigned ACP session id"
            );
            inst.cockpit_acp_session_id = Some(new_id.clone());
        }
        AcpSessionChange::Reset(reason) => {
            tracing::info!(
                target: "cockpit.event_listener",
                session = %session_id,
                %reason,
                "clearing stored ACP session id after session/load failure"
            );
            inst.cockpit_acp_session_id = None;
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
fn derive_acp_session_change(event: &crate::cockpit::Event) -> Option<AcpSessionChange> {
    use crate::cockpit::Event;
    match event {
        Event::AcpSessionAssigned { acp_session_id } => {
            Some(AcpSessionChange::Assigned(acp_session_id.clone()))
        }
        Event::SessionContextReset { reason } => Some(AcpSessionChange::Reset(reason.clone())),
        _ => None,
    }
}

/// What a cockpit event implies for the sidebar status. `Set` is an
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
pub(crate) fn derive_cockpit_status(event: &crate::cockpit::Event) -> Option<StatusIntent> {
    use crate::cockpit::Event;
    match event {
        Event::UserPromptSent { .. } | Event::ApprovalResolved { .. } => {
            Some(StatusIntent::Set(Status::Running))
        }
        Event::ApprovalRequested { .. } => Some(StatusIntent::Set(Status::Waiting)),
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
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_runtime_fields_carries_last_acquired_existing_sid() {
        let mut prior = crate::session::Instance::new("title", "/tmp/x");
        prior.last_acquired_existing_sid = true;
        let fresh = crate::session::Instance::new("title", "/tmp/x");
        assert!(!fresh.last_acquired_existing_sid);

        let merged = merge_runtime_fields(prior, fresh);
        assert!(
            merged.last_acquired_existing_sid,
            "merge_runtime_fields must carry last_acquired_existing_sid across status_poll reloads",
        );
    }
    #[cfg(feature = "serve")]
    #[test]
    fn derive_cockpit_status_maps_terminal_events() {
        use crate::cockpit::approvals::{ApprovalDecision, Nonce};
        use crate::cockpit::permissions::build_approval;
        use crate::cockpit::state::ToolCall;
        use crate::cockpit::Event;
        let tool_call = ToolCall {
            id: "t".into(),
            name: "shell".into(),
            kind: "execute".into(),
            args_preview: "{}".into(),
            started_at: chrono::Utc::now(),
            parent_tool_call_id: None,
            memory_recall: None,
        };
        assert_eq!(
            derive_cockpit_status(&Event::UserPromptSent { text: "hi".into() }),
            Some(StatusIntent::Set(Status::Running))
        );
        assert_eq!(
            derive_cockpit_status(&Event::ApprovalRequested {
                approval: build_approval(tool_call.clone()),
            }),
            Some(StatusIntent::Set(Status::Waiting))
        );
        assert_eq!(
            derive_cockpit_status(&Event::ApprovalResolved {
                nonce: Nonce("x".into()),
                decision: ApprovalDecision::Allow,
            }),
            Some(StatusIntent::Set(Status::Running))
        );
        assert_eq!(
            derive_cockpit_status(&Event::Stopped {
                reason: "prompt_complete".into()
            }),
            Some(StatusIntent::Set(Status::Idle))
        );
        // Rate-limit park: NOT an error; sidebar stays grey, the
        // dedicated RateLimit banner carries the reset time. See #1281.
        assert_eq!(
            derive_cockpit_status(&Event::Stopped {
                reason: "rate_limited".into()
            }),
            Some(StatusIntent::Set(Status::Idle))
        );
        assert_eq!(
            derive_cockpit_status(&Event::AgentStartupError {
                message: "boom".into()
            }),
            Some(StatusIntent::Set(Status::Error))
        );
        // AcpSessionAssigned heals an Error banner only — never
        // clobbers an in-progress Running/Waiting turn.
        assert_eq!(
            derive_cockpit_status(&Event::AcpSessionAssigned {
                acp_session_id: "uuid".into()
            }),
            Some(StatusIntent::HealError)
        );
    }

    #[cfg(feature = "serve")]
    #[test]
    fn derive_acp_session_change_extracts_assigned_id() {
        use crate::cockpit::Event;
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
        use crate::cockpit::Event;
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
        use crate::cockpit::Event;
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
    fn derive_cockpit_status_ignores_streaming_and_string_events() {
        use crate::cockpit::Event;
        // Mid-turn events that shouldn't move the session out of Running.
        assert_eq!(
            derive_cockpit_status(&Event::AgentMessageChunk { text: "x".into() }),
            None
        );
        assert_eq!(derive_cockpit_status(&Event::ThinkingStarted), None);
        assert_eq!(derive_cockpit_status(&Event::ThinkingEnded), None);
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
            "img-src 'self' data: https://github.com https://avatars.githubusercontent.com",
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
}
