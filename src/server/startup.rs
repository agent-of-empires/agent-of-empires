//! Bringing the server up: auth mode, fd limits, the listener, and the
//! background loops it owns.

use crate::file_watch::FileWatchService;
use crate::server::push::{PushState, STATUS_CHANNEL_CAPACITY};
use crate::server::rate_limit::RateLimiter;
use anyhow::Context;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::info;

use super::access::{host_from_url, resolve_access_policy};
use super::acp_events::{acp_event_listener, seed_acp_statuses};
use super::disk_watch::{disk_watcher_consumer, init_disk_watch_subscriptions};
use super::ip_discovery::{discover_tagged_ips, IpKind};
use super::reload::load_all_instances;
use super::router::build_router;
use super::serve_snapshot::{
    spawn_serve_snapshot_loop, FormFactorCounters, StructuredTelemetryCounters,
};
use super::startup_recovery::{daemon_startup_recovery_cascade, daemon_startup_recovery_mark};
use super::state::{
    AppState, CleanupDefaultsCache, ACP_CHANNEL_CAPACITY, CLEANUP_DEFAULTS_TTL,
    PENDING_ATTACHMENT_TTL,
};
use super::status_poll::status_poll_loop;
use super::token::{
    load_or_generate_token, test_token_grace_override, test_token_lifetime_override,
    write_secret_file, TokenManager, DEFAULT_TOKEN_GRACE,
};
use crate::server::{api, callback, login, push, session_service, tunnel};

/// Build the owner-only `serve.url` contents for a remotely exposed daemon.
/// The public tunnel stays first for backwards-compatible display/QR consumers,
/// while the loopback alternate lets same-host clients (notably the TUI) use the
/// auth middleware's filesystem-trusted loopback bypass instead of round-tripping
/// through the tunnel and getting challenged for a browser passphrase session.
pub(super) fn remote_serve_url_contents(
    remote_base_url: &str,
    local_port: u16,
    token: Option<&str>,
) -> String {
    let with_token = |base_url: &str| {
        let base_url = base_url.trim_end_matches('/');
        match token {
            Some(token) => format!("{base_url}/?token={token}"),
            None => format!("{base_url}/"),
        }
    };

    let remote_url = with_token(remote_base_url);
    let loopback_url = with_token(&format!("http://127.0.0.1:{local_port}"));
    format!("{remote_url}\nlocalhost\t{loopback_url}\n")
}

const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Post-signal shutdown: cancel the shared token, arm the force-exit
/// deadline, then reap plugin workers within four fifths of the window,
/// leaving the rest for the caller's own cleanup.
///
/// A worker that never terminates therefore cannot keep the daemon alive.
/// It is not free: a reap cut short can leave a worker group SIGTERMed
/// without the SIGKILL escalation, and a forced exit skips the
/// post-`axum::serve` cleanup entirely (acp detach, tunnel SIGTERM of
/// cloudflared, removal of serve.passphrase). The PID file is swept by
/// `daemon_pid`'s stale-PID check on the next start.
async fn run_shutdown_sequence<R, F>(
    shutdown: &CancellationToken,
    grace: Duration,
    reap: R,
    force_exit: F,
) where
    R: Future<Output = ()>,
    F: FnOnce() + Send + 'static,
{
    shutdown.cancel();
    // Build the timer here, not inside the task: `sleep` fixes its deadline
    // from the clock at construction, so constructing it in the task would
    // restart the window at the task's first poll, which the reap's own
    // synchronous work can delay.
    let deadline = tokio::time::sleep(grace);
    tokio::spawn(async move {
        deadline.await;
        tracing::warn!(
            target: "shutdown",
            grace_secs = grace.as_secs(),
            "graceful shutdown exceeded grace window, forcing exit"
        );
        force_exit();
    });
    if tokio::time::timeout(grace * 4 / 5, reap).await.is_err() {
        tracing::warn!(
            target: "shutdown",
            "plugin worker reap did not finish in time, continuing shutdown"
        );
    }
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
pub(super) fn raise_fd_limit() {
    use nix::sys::resource::{getrlimit, setrlimit, Resource};
    match getrlimit(Resource::RLIMIT_NOFILE) {
        Ok((soft, hard)) => {
            // `rlim_t` is u64 on Linux/macOS but i64 on the BSDs, so derive the
            // 8192 ceiling in the limits' own type rather than a hardcoded u64;
            // this keeps the arithmetic and the setrlimit call below portable.
            let target = hard.min(8192).max(soft);
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
pub(super) fn raise_fd_limit() {}

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
    /// Operator-supplied `--allowed-host` entries, merged with the derived
    /// loopback/bind/tunnel set by `resolve_access_policy`. See #2735.
    pub extra_allowed_hosts: Vec<String>,
    /// Operator-supplied `--allowed-origin` entries (normalized to the browser
    /// `Origin` form), for reverse proxies on nonstandard ports. See #2735.
    pub extra_allowed_origins: Vec<String>,
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
        extra_allowed_hosts,
        extra_allowed_origins,
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
    let config = crate::session::config::profile_config::resolve_config_or_warn(profile);
    // Feed the unread-feature gate from this daemon's resolved config. Like
    // `push_enabled`, this is read once at startup; a config change needs a
    // restart to take effect. The TUI process maintains its own copy.
    crate::session::set_unread_enabled(config.session.unread_indicator);
    crate::session::set_favorites_first(config.session.favorites_first);

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

    let acp_events_tx = broadcast::channel(ACP_CHANNEL_CAPACITY).0;
    let acp_event_store = {
        let app_dir = crate::session::get_app_dir().context("acp event store: resolve app dir")?;
        let db_path = app_dir.join("acp_events.db");
        Arc::new(
            crate::acp::event_store::EventStore::open(&db_path, config.acp.replay_events as usize)
                .context("acp event store: open")?,
        )
    };
    let acp_control_cache = Arc::new(crate::acp::control_cache::ControlStateCache::new());
    let acp_supervisor = {
        // Approval pushes are dispatched from `acp_event_listener`,
        // which subscribes to the broadcast that ChannelSink::publish
        // feeds and has `Arc<AppState>` in scope without a closure
        // dance through the supervisor. See #1038.
        let sink = std::sync::Arc::new(crate::acp::supervisor::ChannelSink {
            tx: acp_events_tx.clone(),
            event_store: acp_event_store.clone(),
            control_cache: acp_control_cache.clone(),
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
    // The session-domain service is built before the plugin host so the
    // host's session RPCs (#2897) get it by construction, never late-bound.
    let instances = Arc::new(RwLock::new(instances));
    let instance_locks = Arc::new(RwLock::new(std::collections::HashMap::new()));
    let idempotency_locks = Arc::new(RwLock::new(std::collections::HashMap::new()));
    let telemetry_session_creates = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let mutation_epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let session_service = Arc::new(session_service::SessionService::new(
        Arc::clone(&instances),
        Arc::clone(&instance_locks),
        Arc::clone(&file_watch),
        Arc::clone(&telemetry_session_creates),
        Arc::clone(&mutation_epoch),
        session_service::AcpDeps {
            supervisor: acp_supervisor.clone(),
            event_store: acp_event_store.clone(),
            control_cache: acp_control_cache.clone(),
        },
    ));

    // the daemon serves fine without plugin workers.
    // The host API includes mutating session.meta.set/cas, so a read-only
    // daemon must not run plugin workers at all: gate the host on !read_only.
    let plugin_host = if read_only {
        tracing::info!(target: "plugin.host", "plugin host disabled in read-only serve mode");
        None
    } else {
        match crate::session::get_app_dir() {
            Ok(app_dir) => {
                // Session RPCs need the automation-policy ledger; if it cannot
                // open, workers still run but session RPCs answer
                // service_unavailable (fail closed on limits, not open).
                let session_rpc = match crate::plugin::automation_policy::AutomationPolicy::open(
                    &app_dir.join("plugin_events.db"),
                ) {
                    Ok(policy) => Some(Arc::new(crate::plugin::session_api::SessionRpcDeps {
                        session_service: Arc::clone(&session_service),
                        policy: Arc::new(policy),
                        profile: profile.to_string(),
                    })),
                    Err(e) => {
                        tracing::warn!(
                            target: "plugin.host",
                            "plugin session RPCs disabled: automation policy store failed: {e:#}"
                        );
                        None
                    }
                };
                match crate::plugin::host::PluginHost::new(&app_dir, profile, session_rpc) {
                    Ok(host) => Some(host),
                    Err(e) => {
                        tracing::warn!(target: "plugin.host", "plugin host disabled: {e:#}");
                        None
                    }
                }
            }
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

    {
        let instances = instances.read().await;
        crate::acp::version_probe::warn_for_structured_sessions(&instances, !is_daemon).await;
    }

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

        // Keep the public tunnel URL first for backwards-compatible consumers,
        // plus a loopback alternate so same-host clients do not round-trip
        // through the tunnel and hit the passphrase wall.
        if let Ok(app_dir) = crate::session::get_app_dir() {
            let contents =
                remote_serve_url_contents(&handle.url, local_port, auth_token.as_deref());
            write_secret_file(&app_dir.join("serve.url"), &contents).await;
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

        // A build without the dashboard bundle still serves the API, so the
        // banner must not promise a page that is not there.
        if cfg!(feature = "web") {
            println!("aoe web dashboard running at:");
        } else {
            println!("aoe daemon running at (API only, no dashboard bundle):");
        }
        for (_, u) in &labeled_urls {
            println!("  {}", u);
        }
        if auth_token.is_some() && cfg!(feature = "web") {
            println!();
            println!(
                "Open any URL above in a browser. Share it to access from other devices on your network."
            );
        }

        if open_browser && !is_daemon {
            if cfg!(feature = "web") {
                if let Some((_, primary)) = labeled_urls.first() {
                    maybe_open_browser(primary);
                }
            } else {
                // Opening a browser on an API-only daemon lands on a 404. Say
                // so rather than dropping the flag on the floor.
                eprintln!("--open ignored: this build has no dashboard bundle");
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

    // DNS-rebinding gate (#2735). Auto-inject the tunnel/Tailscale public host
    // so remote dashboards and their live-terminal WS upgrade (which carries
    // `Origin: https://<tunnel-host>`) pass without any operator flag; the URL
    // rotates on quick tunnels and the bind is forced to loopback, so
    // `--allowed-host` cannot cover this path.
    let tunnel_host: Option<String> = tunnel_handle.as_ref().and_then(|h| host_from_url(&h.url));
    let (allowed_hosts, allowed_origins) = resolve_access_policy(
        host,
        local_port,
        &extra_allowed_hosts,
        &extra_allowed_origins,
        tunnel_host.as_deref(),
    );
    tracing::info!(
        target: "http.access",
        ?allowed_hosts,
        ?allowed_origins,
        "resolved DNS-rebinding allowlist"
    );

    let state = Arc::new(AppState {
        profile: profile.to_string(),
        read_only,
        cityhall_mode: std::env::var_os("AOE_CITYHALL_MODE").is_some(),
        instances,
        session_service,
        token_manager: Arc::clone(&token_manager),
        login_manager: Arc::clone(&login_manager),
        rate_limiter: Arc::clone(&rate_limiter),
        behind_tunnel: remote || behind_proxy,
        auth_mode,
        serve_mode,
        allowed_hosts,
        allowed_origins,
        instance_locks,
        idempotency_locks,
        smart_rename_inflight: std::sync::Mutex::new(std::collections::HashSet::new()),
        smart_rename_attempted: std::sync::Mutex::new(std::collections::HashSet::new()),
        smart_rename_semaphore: tokio::sync::Semaphore::new(
            crate::session::smart_rename::MAX_CONCURRENT,
        ),
        summary_inflight: std::sync::Mutex::new(std::collections::HashSet::new()),
        summary_semaphore: tokio::sync::Semaphore::new(
            crate::session::conversation_summary::MAX_CONCURRENT,
        ),
        recently_restarted: crate::session::recovery::new_recently_restarted(),
        mutation_epoch: Arc::clone(&mutation_epoch),
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
        acp_events_tx: acp_events_tx.clone(),
        acp_event_store: acp_event_store.clone(),
        acp_control_cache: acp_control_cache.clone(),
        acp_supervisor: acp_supervisor.clone(),
        plugin_host: plugin_host.clone(),
        plugin_jobs: Arc::new(api::plugins::PluginJobRegistry::new()),
        push: push_state,
        push_enabled,
        web_config: config.web.clone(),
        web_presence: std::sync::Mutex::new(std::collections::HashMap::new()),
        sleep_inhibit_snapshot: std::sync::atomic::AtomicU8::new(0),
        telemetry_usage_seen: crate::telemetry::usage_signals::UsageSeenCounters::new(),
        telemetry_web_clients: FormFactorCounters::default(),
        telemetry_structured_clients: FormFactorCounters::default(),
        telemetry_session_creates,
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
                // Same one-shot startup slot: repoint any managed worktree
                // whose directory was moved outside aoe. See #2002.
                crate::server::api::reconcile_worktree_paths(&sweep_state).await;
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(60 * 60));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        _ = interval.tick() => {
                            crate::server::api::purge_expired_trash(&sweep_state).await;
                            // Q5: reclaim attachment bytes buffered for a queued
                            // prompt that never drained (a session that never went
                            // idle again). Removal/clear/drain/session-delete drop
                            // these already, so this only catches the stranded tail.
                            let store = sweep_state.acp_event_store.clone();
                            let pruned = tokio::task::spawn_blocking(move || {
                                store.prune_pending_attachments_older_than(PENDING_ATTACHMENT_TTL)
                            })
                            .await
                            .unwrap_or(0);
                            if pruned > 0 {
                                tracing::info!(target: "acp.queue", pruned, "pruned stale queued-prompt attachments past TTL");
                            }
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

    // Per-session dispatcher callback consumer: subscribes to the same
    // status_tx broadcast and fires an HTTP POST to any instance's
    // callback_url on a fire-worthy transition. See #3156.
    callback::spawn_consumer(state.clone());

    // Launch plugin workers for every active plugin that declares a runtime.
    // Non-blocking: each worker runs in its own supervised task. A daemon with
    // no community plugin workers (the common case) does nothing here.
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
        // The tunnel URL is stable across the daemon's lifetime (Tailscale
        // and named CF tunnels are stable; quick CF rotates only on
        // restart, which is outside this task's scope). Capture once so
        // the rotation task can rebuild `serve.url` with the new token.
        let rot_base_url: Option<String> = tunnel_handle.as_ref().map(|h| h.url.clone());
        tokio::spawn(remote_rotation_loop(
            state.token_manager.clone(),
            state.push.clone(),
            state.shutdown.clone(),
            rot_base_url,
            local_port,
        ));
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
    //   2. Arms a 5s force-exit deadline, then reaps plugin workers
    //      within part of that window: if any handler or worker ignores
    //      the cancel, the process still force-exits, so `Ctrl-C` and
    //      terminal hangups never hang. See #1198.
    //
    // Note: this future is awaited by `with_graceful_shutdown`, which
    // signals axum to stop accepting new connections once the future
    // resolves. Wrapping `axum::serve(...).await` itself in a
    // `tokio::time::timeout` would cap TOTAL server lifetime instead
    // of just the post-signal drain, which is wrong (the server would
    // exit after 5s of normal uptime). The deadline lives inside the
    // signal handler so the clock only starts after the signal fires.
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
        let plugin_host = shutdown_state.plugin_host.clone();
        run_shutdown_sequence(
            &shutdown_state.shutdown,
            SHUTDOWN_GRACE,
            async move {
                if let Some(host) = plugin_host {
                    host.shutdown().await;
                }
            },
            || std::process::exit(0),
        )
        .await;
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

/// Best-effort launch of `url` in the user's default browser. Suppressed
/// in environments where opening a browser is not useful: SSH sessions
/// (the user is on another host) and Linux/BSD without a display server.
/// Failures are logged but never propagate; the server keeps running.
pub(super) fn maybe_open_browser(url: &str) {
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

/// The `--remote` token rotation loop: wait one lifetime, rotate, then wait out
/// the grace window before clearing the previous token and dropping the push
/// subscriptions bound only to its hash.
///
/// Runs here rather than in `TokenManager::spawn_rotation_task` so rotation can
/// also refresh `serve.url` and prune the push store. Both deadlines come from
/// the manager, so the previous token's state and its subscriptions cannot
/// outlive the window `validate` accepts it in.
async fn remote_rotation_loop(
    token_manager: Arc<TokenManager>,
    push: Option<Arc<PushState>>,
    shutdown: CancellationToken,
    base_url: Option<String>,
    local_port: u16,
) {
    loop {
        let lifetime = token_manager.lifetime_secs().await;
        let grace = token_manager.grace().await;
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(lifetime)) => {}
            _ = shutdown.cancelled() => break,
        }

        // Capture the hashes of the current and (about-to-be) previous tokens
        // BEFORE rotating, so we know which owner-hashes are still valid in
        // the store.
        let pre_rotate_current = token_manager.current_token().await;
        token_manager.rotate().await;
        let post_rotate_current = token_manager.current_token().await;

        // Refresh `serve.url` so the TUI display and the QR-code URL stay in
        // sync with the rotated token. Without this the TUI keeps showing
        // `?token=<old>`, which stops working once the grace window closes.
        if let (Some(base_url), Some(token)) = (base_url.as_ref(), post_rotate_current.as_ref()) {
            if let Ok(app_dir) = crate::session::get_app_dir() {
                let contents = remote_serve_url_contents(base_url, local_port, Some(token));
                write_secret_file(&app_dir.join("serve.url"), &contents).await;
            }
        }

        if let Some(push) = push.as_ref() {
            let mut valid_hashes: Vec<[u8; 32]> = Vec::new();
            if let Some(t) = &post_rotate_current {
                valid_hashes.push(push::sha256_token(t));
            }
            if let Some(t) = &pre_rotate_current {
                // The old token is still inside the grace window, so devices
                // that have not picked up the new one keep receiving pushes.
                valid_hashes.push(push::sha256_token(t));
            }
            // In no-auth mode the token is None and we use a zero hash;
            // preserve that so zero-hash subs survive.
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

        tokio::select! {
            _ = tokio::time::sleep(grace) => {}
            _ = shutdown.cancelled() => break,
        }
        token_manager.clear_previous().await;

        if let Some(push) = push.as_ref() {
            let mut valid_hashes: Vec<[u8; 32]> = Vec::new();
            if let Some(t) = token_manager.current_token().await {
                valid_hashes.push(push::sha256_token(&t));
            }
            if valid_hashes.is_empty() {
                valid_hashes.push([0u8; 32]);
            }
            if let Err(e) = push.store.retain_owners(&valid_hashes).await {
                tracing::warn!(target: "http.middleware", error = %e, "push: retain_owners failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_serve_url_contents_keeps_public_primary_and_loopback_alternate() {
        assert_eq!(
            remote_serve_url_contents("https://aoe.example.test", 8080, Some("secret")),
            "https://aoe.example.test/?token=secret\n\
             localhost\thttp://127.0.0.1:8080/?token=secret\n"
        );
    }

    #[test]
    fn remote_serve_url_contents_handles_trailing_slash_and_no_auth() {
        assert_eq!(
            remote_serve_url_contents("https://aoe.example.test/", 8080, None),
            "https://aoe.example.test/\nlocalhost\thttp://127.0.0.1:8080/\n"
        );
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

    /// The post-rotation cleanup must run on the configured grace period, not
    /// a hardcoded default, so a token stops being accepted and stops owning
    /// push subscriptions at the same moment.
    #[tokio::test(start_paused = true)]
    async fn rotation_cleanup_honors_the_configured_grace() {
        let lifetime = Duration::from_secs(60);
        let grace = Duration::from_secs(7);
        let manager = Arc::new(TokenManager::with_grace(
            Some("old_token".to_string()),
            lifetime,
            grace,
        ));

        let dir = tempfile::tempdir().unwrap();
        let push = Arc::new(PushState::init(dir.path()).unwrap());
        push.store
            .upsert(push::Subscription {
                endpoint: "https://push.example.test/old".to_string(),
                p256dh: "pk".into(),
                auth: "auth".into(),
                owner_token_hash: push::sha256_token("old_token"),
                user_agent: "UA".into(),
                created_at: chrono::Utc::now(),
                generation: 0,
                origin: "https://aoe.example.test".into(),
            })
            .await
            .unwrap();

        let shutdown = CancellationToken::new();
        tokio::spawn(remote_rotation_loop(
            manager.clone(),
            Some(push.clone()),
            shutdown.clone(),
            None,
            8080,
        ));

        // Mid-grace: the rotated-out token is still accepted, and its
        // subscription still belongs to a valid owner.
        tokio::time::sleep(lifetime + grace / 2).await;
        assert!(manager.validate("old_token").await.0);
        assert!(manager.holds_previous().await);
        assert_eq!(push.store.snapshot().await.len(), 1);

        // Past the configured grace: validation and cleanup agree. With the
        // hardcoded 300s sleep the token was rejected here while its state and
        // subscription lingered.
        tokio::time::sleep(grace).await;
        assert!(!manager.validate("old_token").await.0);
        assert!(!manager.holds_previous().await);
        assert!(push.store.snapshot().await.is_empty());

        shutdown.cancel();
    }

    #[tokio::test]
    async fn shutdown_sequence_bounds_a_plugin_reap_that_never_finishes() {
        use std::sync::atomic::{AtomicBool, Ordering};

        const GRACE: Duration = Duration::from_millis(100);

        // Arming the deadline first must not skip the plugin teardown.
        let shutdown = CancellationToken::new();
        let reaped = Arc::new(AtomicBool::new(false));
        let flag = reaped.clone();
        run_shutdown_sequence(
            &shutdown,
            GRACE,
            async move { flag.store(true, Ordering::SeqCst) },
            || {},
        )
        .await;
        assert!(shutdown.is_cancelled());
        assert!(reaped.load(Ordering::SeqCst));

        // A reap that never finishes is bounded, so the caller still reaches
        // its own cleanup, and the deadline still forces the exit.
        let shutdown = CancellationToken::new();
        let forced = Arc::new(AtomicBool::new(false));
        let flag = forced.clone();
        tokio::time::timeout(
            GRACE * 10,
            run_shutdown_sequence(&shutdown, GRACE, std::future::pending::<()>(), move || {
                flag.store(true, Ordering::SeqCst)
            }),
        )
        .await
        .expect("a hung reap must not block the shutdown sequence");

        for _ in 0..200 {
            if forced.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(forced.load(Ordering::SeqCst));
    }

    /// The grace window must run from the cancel, not from whenever the
    /// watchdog task first gets polled. On this single-threaded runtime a
    /// reap that works synchronously before yielding holds the only worker,
    /// so a deadline built inside the task would not start until the reap
    /// released it, stretching the window past `GRACE`.
    #[tokio::test]
    async fn shutdown_deadline_runs_from_the_cancel_not_the_watchdog_poll() {
        use std::sync::atomic::{AtomicBool, Ordering};

        const GRACE: Duration = Duration::from_millis(100);

        let shutdown = CancellationToken::new();
        let forced = Arc::new(AtomicBool::new(false));
        let flag = forced.clone();
        run_shutdown_sequence(
            &shutdown,
            GRACE,
            async { std::thread::sleep(GRACE * 2) },
            move || flag.store(true, Ordering::SeqCst),
        )
        .await;
        assert!(
            !forced.load(Ordering::SeqCst),
            "the reap blocked the worker"
        );

        // One short park is all the watchdog needs once its deadline has
        // already passed, and far less than another full window.
        tokio::time::sleep(Duration::from_millis(1)).await;
        assert!(forced.load(Ordering::SeqCst));
    }
}
