//! `AppState`, the shared handle every request and background loop reads,
//! plus the caches hanging off it.

use crate::acp::protocol::AcpBroadcastFrame;
use crate::file_watch::{FileWatchService, SubscriptionHandle};
use crate::server::push::{PushState, StatusChange};
use crate::server::rate_limit::RateLimiter;
use crate::session::Instance;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};
use tokio_util::sync::CancellationToken;

use super::serve_snapshot::{
    FormFactorCounters, ReportedServeSignals, StructuredTelemetryCounters,
};
use super::token::TokenManager;
use crate::server::{api, login, session_service};

pub(super) const ACP_CHANNEL_CAPACITY: usize = 256;

/// Per-profile cleanup defaults with a refresh timestamp. Re-resolved from
/// disk after `CLEANUP_DEFAULTS_TTL`.
pub struct CleanupDefaultsCache {
    pub refreshed_at: std::time::Instant,
    pub entries: std::collections::HashMap<String, api::CleanupDefaults>,
}

pub const CLEANUP_DEFAULTS_TTL: std::time::Duration = std::time::Duration::from_secs(30);

/// How long attachment bytes buffered for a queued prompt live before the
/// hourly sweep reclaims them. A
/// queued prompt normally drains within seconds; this only catches bytes
/// stranded by a session that never becomes idle again.
pub(super) const PENDING_ATTACHMENT_TTL: std::time::Duration =
    std::time::Duration::from_secs(24 * 60 * 60);

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
pub(super) struct ChangedFilesEntry {
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
    pub(super) handle: SubscriptionHandle,
    /// Abort handle for the forwarder task that drains the per-profile
    /// receiver into `disk_changed`.
    pub(super) forwarder: tokio::task::AbortHandle,
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
    /// CityHall client mode, resolved once at launch from `AOE_CITYHALL_MODE`.
    /// When set, the web dashboard is locked down to an end-user client
    /// (composer + structured view only) and the server rejects terminal,
    /// diff, project-management, and advanced-settings endpoints. Enforced
    /// server-side, not only in the UI, mirroring `read_only`. See #7.
    pub cityhall_mode: bool,
    pub instances: Arc<RwLock<Vec<Instance>>>,
    /// Session-domain service handle sharing `instances`, `instance_locks`,
    /// `file_watch`, the telemetry create counter, and the ACP supervisor
    /// with the fields on this struct, so a non-HTTP caller (the plugin
    /// host, #2897) can drive session create/turn without holding
    /// `AppState`.
    pub session_service: Arc<session_service::SessionService>,
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
    /// DNS-rebinding gate: accepted `Host` values, port-stripped,
    /// ASCII-lowercased, IPv6 unbracketed. Resolved once at launch by
    /// `resolve_access_policy` from the bind host, `--allowed-host`, and any
    /// auto-injected tunnel host. `access_policy` rejects an unlisted `Host`
    /// with 403, before auth. See #2735.
    pub allowed_hosts: Vec<String>,
    /// DNS-rebinding gate: accepted `Origin` values (scheme + host [+ port],
    /// ASCII-lowercased). A request whose `Origin` is unlisted is rejected
    /// with 403; a request with no `Origin` (curl, native TUI, non-browser
    /// WS) is exempt. Resolved alongside `allowed_hosts`. See #2735.
    pub allowed_origins: Vec<String>,
    /// Per-instance mutex guarding mutations that must not interleave
    /// (e.g. `ensure_session` decide-and-restart). Entries are created on
    /// first use and live for the lifetime of the process — there are only
    /// as many as the user has sessions.
    pub instance_locks: Arc<RwLock<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// Per-`idempotency_key` mutex serializing `POST /api/sessions` create
    /// requests that share a key, so two concurrent retries with the same
    /// key can't both scan-miss the existing-instance check and both create
    /// a session. Unlike `instance_locks` above, entries here are NOT bounded
    /// by the number of sessions: keys are caller-supplied, one per request.
    /// `idempotency_lock` therefore prunes unreferenced entries on its miss
    /// path, so the map tracks in-flight keyed creates rather than every key
    /// the daemon has ever seen. See #3156.
    pub idempotency_locks:
        Arc<RwLock<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
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
    /// Session ids with an in-flight conversation-summary one-shot, so the
    /// automatic trigger and the on-demand endpoint cannot spawn concurrent
    /// summaries for the same session (which would also race on the
    /// last-summary seq). Synchronous mutex; tiny critical sections. See
    /// `session::conversation_summary` and #2808.
    pub summary_inflight: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Global cap on concurrent conversation-summary one-shots. Separate from
    /// `smart_rename_semaphore` (permits=2) and sized to 1: a summary runs the
    /// agent over the whole transcript, so it is slower and costlier than a
    /// title call; a dedicated single slot keeps heavy background summaries
    /// from starving the snappy first-prompt rename. See #2808.
    pub summary_semaphore: tokio::sync::Semaphore,
    /// Suppression set for the startup-recovery cascade. While an entry is
    /// present and younger than `recovery::RECENTLY_RESTARTED_TTL`, the
    /// `status_poll_loop` skips `update_status_with_metadata` for that
    /// instance and surfaces `Status::Starting` instead. Without this,
    /// `last_start_time` (which is `#[serde(skip)]`) is lost on the loop's
    /// `load_all_instances` reload, and a freshly-recovered session
    /// transitions to `Status::Error` for up to 8 seconds while the agent
    /// is still settling. Periodically GC'd by a background task.
    pub recently_restarted: crate::session::recovery::RecentlyRestarted,
    /// Bumped once per committed membership change of the session set: a
    /// removal, after the row is gone from both `sessions.json` and
    /// `instances`, and a creation, after the row is in both. A reloader
    /// reads it before its disk read and hands the value back to
    /// `reload_state_instances_from_disk`, which drops the reload when the
    /// value moved: the disk snapshot it is carrying predates the mutation,
    /// so folding it in would resurrect a removed row or drop a created one.
    /// See invariant 8 on that function.
    ///
    /// Membership only. A field edit on an existing row does not bump, because
    /// the per-id merge already reconciles those; the epoch exists for the
    /// two cases the merge cannot see, where the id itself is absent from one
    /// side.
    pub mutation_epoch: Arc<std::sync::atomic::AtomicU64>,
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
    /// Cached (owner, host-scoped key) per repo path. Remote owners don't
    /// change, so entries live for the lifetime of the process. The key
    /// ("owner@host") lets the web org axis bucket by host-scoped identity
    /// without merging same-named owners on different hosts; `remote_owner`
    /// stays the plain owner for display.
    pub remote_owner_cache: RwLock<std::collections::HashMap<String, Option<(String, String)>>>,
    /// Short-TTL cache of `compute_changed_files` keyed by `(repo_path,
    /// base_branch)`, shared by the file-list and per-file diff endpoints so a
    /// burst of file switches reuses one branch scan. See `ChangedFilesEntry`.
    pub(super) changed_files_cache:
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
    pub acp_events_tx: broadcast::Sender<AcpBroadcastFrame>,
    /// Disk-backed acp event log. The single source of truth for
    /// replay: `ChannelSink::publish` writes here on every event, the
    /// WS-on-connect drain reads from here, the `/acp/replay` REST
    /// endpoint reads from here, and `Supervisor::next_seqs` is seeded
    /// from here at startup so a fresh publish gets `max_seq + 1`
    /// rather than 1.
    pub acp_event_store: Arc<crate::acp::event_store::EventStore>,
    /// Live control-state projection per session, folded at the publish choke
    /// point and shared with `ChannelSink`. Prompt dispatch reads it instead
    /// of replaying the log on every POST; see `crate::acp::control_cache`.
    pub acp_control_cache: Arc<crate::acp::control_cache::ControlStateCache>,
    /// Owns the per-session ACP agent subprocesses.
    pub acp_supervisor:
        Arc<crate::acp::supervisor::Supervisor<crate::acp::supervisor::ChannelSink>>,
    /// The Tier 1 plugin worker host. `None` in test harnesses that do not
    /// stand up a host; `Some` in a live daemon.
    pub plugin_host: Option<Arc<crate::plugin::host::PluginHost>>,
    /// Tracks in-flight web plugin install / update / uninstall jobs so the
    /// dashboard can tail their host-side log. In-memory; see
    /// `api::plugins::PluginJobRegistry`.
    pub plugin_jobs: Arc<crate::server::api::plugins::PluginJobRegistry>,
    /// Per-browser foreground dashboard presence. Entries are keyed by a hash
    /// of the device-binding secret and expire when the browser stops sending
    /// its visibility heartbeat. Push suppression must not treat ordinary
    /// polling or a backgrounded PWA as evidence that somebody is looking at
    /// the dashboard.
    pub web_presence: std::sync::Mutex<std::collections::HashMap<[u8; 32], i64>>,
    /// Packed sleep-inhibit reconciler snapshot for read-only status reporting:
    /// bit `SLEEP_INHIBIT_SNAPSHOT_ENABLED` is the
    /// `prevent_sleep_when_active` toggle as the reconciler last read it, bit
    /// `SLEEP_INHIBIT_SNAPSHOT_SLOT_PRESENT` is whether an inhibitor slot is
    /// retained (slot presence, not the gated held state the endpoint reports).
    /// Sole writer is `update_sleep_inhibit`; `/api/about` reads it. Packed into
    /// one byte so the two correlated bits are read torn-free. The slot itself
    /// stays loop-local; only this derived scalar reaches `AppState`.
    /// `backend_available` is read live from the latch, not stored here.
    pub sleep_inhibit_snapshot: std::sync::atomic::AtomicU8,
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
    pub telemetry_session_creates: Arc<std::sync::atomic::AtomicU32>,
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
    pub(super) telemetry_last_reported: std::sync::Mutex<Option<ReportedServeSignals>>,
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
        instance_lock_in(&self.instance_locks, id).await
    }

    /// Get or create the per-idempotency-key serialization mutex. Same
    /// get-or-create shape as `instance_lock`, but prunes on the miss path:
    /// unlike session ids, idempotency keys are per-request and unbounded, so
    /// without eviction the map would grow for the daemon's lifetime.
    pub async fn idempotency_lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        {
            let guard = self.idempotency_locks.read().await;
            if let Some(lock) = guard.get(key) {
                return lock.clone();
            }
        }
        let mut guard = self.idempotency_locks.write().await;
        // Drop keys nobody is using. A strong count of 1 means the map holds
        // the only reference, so no request is mid-flight on that key and the
        // created session's persisted `idempotency_key` is now the durable
        // dedup record; a later retry re-creates a fresh mutex and re-reads
        // that record, which is equivalent. A waiter can only clone the `Arc`
        // while holding this same write lock, so pruning cannot race one away.
        // Mirrors the prune-under-write-lock shape in `changed_files_cached`.
        guard.retain(|_, lock| Arc::strong_count(lock) > 1);
        guard
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Record whether one browser is currently foregrounded. Browser identity
    /// is derived from its device-binding secret, never retained in plaintext.
    pub fn set_web_presence(&self, client: [u8; 32], active: bool) {
        let mut presence = self.web_presence.lock().expect("web_presence poisoned");
        if active {
            presence.insert(client, crate::util::now_ms() as i64);
        } else {
            presence.remove(&client);
        }
    }

    /// Returns true if any dashboard recently reported itself visible and
    /// focused. Stale entries are swept here, on the only read path.
    pub fn web_active_within(&self, threshold: std::time::Duration) -> bool {
        let now = crate::util::now_ms() as i64;
        let max_age = threshold.as_millis() as i64;
        let mut presence = self.web_presence.lock().expect("web_presence poisoned");
        presence.retain(|_, last| now.saturating_sub(*last) < max_age);
        !presence.is_empty()
    }
}

/// Get or create the per-instance serialization mutex in `locks`. Free function
/// rather than only an [`AppState`] method so the ACP turn-end unread writers
/// can take the *same* lock a REST handler takes without needing an `AppState`
/// (which has no test constructor). See [`AppState::instance_lock`].
pub(super) async fn instance_lock_in(
    locks: &RwLock<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    id: &str,
) -> Arc<tokio::sync::Mutex<()>> {
    {
        let guard = locks.read().await;
        if let Some(lock) = guard.get(id) {
            return lock.clone();
        }
    }
    let mut guard = locks.write().await;
    guard
        .entry(id.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::test_support;

    /// `idempotency_locks` must not grow for the daemon's lifetime: keys are
    /// caller-supplied and unbounded, so an entry nobody holds is pruned on
    /// the next miss. A key whose lock is still held must survive. See #3156.
    #[tokio::test]
    async fn idempotency_lock_prunes_unreferenced_keys() {
        let state = test_support::build_test_app_state(vec![]);

        // A key acquired and released leaves nothing behind: the next miss on
        // a different key prunes it.
        drop(state.idempotency_lock("released-key").await);
        let _other = state.idempotency_lock("other-key").await;
        assert!(
            !state
                .idempotency_locks
                .read()
                .await
                .contains_key("released-key"),
            "an unreferenced key must be pruned rather than retained forever"
        );

        // A key still held by a live caller must NOT be pruned, or two
        // concurrent same-key creates would stop serializing.
        let held = state.idempotency_lock("held-key").await;
        let _guard = held.lock_owned().await;
        let _third = state.idempotency_lock("third-key").await;
        assert!(
            state
                .idempotency_locks
                .read()
                .await
                .contains_key("held-key"),
            "a key with a live holder must survive pruning"
        );
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
}
