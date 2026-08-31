//! Test-only constructors that integration tests in `tests/` need to drive
//! `reload_state_instances_from_disk` and the dynamic-profile-rewire helpers
//! without going through the full daemon. Mirrors the pattern at
//! `src/tmux/mod.rs`'s `test_support` module: gated on
//! `#[cfg(any(test, feature = "test-support"))]` so the surface stays out of
//! production builds, and `#[doc(hidden)]` so it's invisible in rustdoc.

use super::*;
use crate::file_watch::FileWatchService;
use crate::server::push::STATUS_CHANNEL_CAPACITY;
use crate::server::rate_limit::RateLimiter;
use crate::session::Instance;
use crate::session::Storage;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, RwLock};
use tokio_util::sync::CancellationToken;

/// Build a minimal `Arc<AppState>` for helper-equivalence tests. Most
/// fields are seeded with empty / default values; only `instances`,
/// `recently_restarted`, and the file-watch trio are real. Acp
/// fields are stubbed because the helper's acp overlay reads them.
pub fn build_test_app_state(prior: Vec<Instance>) -> Arc<AppState> {
    build_test_app_state_with_policy(prior, Vec::new(), Vec::new(), None)
}

/// Like [`build_test_app_state`] but with CityHall client mode on, so route
/// tests can assert the mode's 403/400 guards fire (#7).
pub fn build_test_app_state_cityhall(prior: Vec<Instance>) -> Arc<AppState> {
    build_test_app_state_impl(prior, Vec::new(), Vec::new(), None, true)
}

/// Like [`build_test_app_state`] but seeds the DNS-rebinding allowlist and,
/// optionally, a real auth token so tests can exercise `access_policy` and
/// the router layering, including the before-auth ordering (#2735).
pub fn build_test_app_state_with_policy(
    prior: Vec<Instance>,
    allowed_hosts: Vec<String>,
    allowed_origins: Vec<String>,
    token: Option<String>,
) -> Arc<AppState> {
    build_test_app_state_impl(prior, allowed_hosts, allowed_origins, token, false)
}

fn build_test_app_state_impl(
    prior: Vec<Instance>,
    allowed_hosts: Vec<String>,
    allowed_origins: Vec<String>,
    token: Option<String>,
    cityhall_mode: bool,
) -> Arc<AppState> {
    let app_dir = tempfile::tempdir().expect("tempdir");
    let acp_db = app_dir.path().join("acp_events.db");
    let event_store =
        Arc::new(crate::acp::event_store::EventStore::open(&acp_db, 100).expect("event store"));
    let acp_events_tx = broadcast::channel::<AcpBroadcastFrame>(8).0;
    let acp_control_cache = Arc::new(crate::acp::control_cache::ControlStateCache::new());
    let sink = std::sync::Arc::new(crate::acp::supervisor::ChannelSink {
        tx: acp_events_tx.clone(),
        event_store: event_store.clone(),
        control_cache: acp_control_cache.clone(),
    });
    let supervisor =
        std::sync::Arc::new(crate::acp::supervisor::Supervisor::with_capacity(sink, 1));
    let instances = Arc::new(RwLock::new(prior));
    let instance_locks = Arc::new(RwLock::new(HashMap::new()));
    let idempotency_locks = Arc::new(RwLock::new(HashMap::new()));
    let telemetry_session_creates = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let mutation_epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let file_watch = FileWatchService::noop();
    let session_service = Arc::new(session_service::SessionService::new(
        Arc::clone(&instances),
        Arc::clone(&instance_locks),
        Arc::clone(&file_watch),
        Arc::clone(&telemetry_session_creates),
        Arc::clone(&mutation_epoch),
        session_service::AcpDeps {
            supervisor: supervisor.clone(),
            event_store: event_store.clone(),
            control_cache: acp_control_cache.clone(),
        },
    ));
    Arc::new(AppState {
        profile: "test".to_string(),
        read_only: false,
        cityhall_mode,
        instances,
        session_service,
        token_manager: Arc::new(TokenManager::new(token, Duration::from_secs(3600))),
        login_manager: Arc::new(login::LoginManager::new(None)),
        rate_limiter: Arc::new(RateLimiter::new()),
        behind_tunnel: false,
        auth_mode: "none",
        serve_mode: "local",
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
            refreshed_at: std::time::Instant::now(),
            entries: HashMap::new(),
        }),
        changed_files_cache: std::sync::RwLock::new(std::collections::HashMap::new()),
        remote_owner_cache: RwLock::new(HashMap::new()),
        status_tx: broadcast::channel(STATUS_CHANNEL_CAPACITY).0,
        acp_events_tx,
        acp_event_store: event_store,
        acp_control_cache,
        acp_supervisor: supervisor,
        plugin_host: None,
        plugin_jobs: Arc::new(api::plugins::PluginJobRegistry::new()),
        push: None,
        push_enabled: false,
        web_config: crate::session::config::WebConfig::default(),
        web_presence: std::sync::Mutex::new(HashMap::new()),
        sleep_inhibit_snapshot: std::sync::atomic::AtomicU8::new(0),
        telemetry_usage_seen: crate::telemetry::usage_signals::UsageSeenCounters::new(),
        telemetry_web_clients: FormFactorCounters::default(),
        telemetry_structured_clients: FormFactorCounters::default(),
        telemetry_session_creates,
        telemetry_structured: StructuredTelemetryCounters::default(),
        telemetry_last_reported: std::sync::Mutex::new(None),
        shutdown: CancellationToken::new(),
        file_watch,
        disk_changed: Arc::new(tokio::sync::Notify::new()),
        disk_watch_handles: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
    })
}

pub async fn drain_session_id_updates_for_test(state: &Arc<AppState>) {
    super::session_identity::drain_session_id_updates_in_state(state).await;
}

pub fn attach_session_id_update_for_test(inst: &mut Instance, sid: &str) {
    let poller = crate::session::poller::SessionPoller::new(format!("test-tmux-{}", inst.id));
    poller.inject_test_update(&inst.id, sid);
    inst.session_id_poller = Some(Arc::new(std::sync::Mutex::new(poller)));
}

pub fn seed_instances_on_disk_for_test(profile: &str, insts: Vec<Instance>) {
    let storage = Storage::new_unwatched(profile).expect("storage");
    storage
        .update(move |instances, _groups| {
            *instances = insts;
            Ok(())
        })
        .expect("seed sessions.json");
}

pub fn load_instances_from_disk_for_test(profile: &str) -> Vec<Instance> {
    Storage::new_unwatched(profile)
        .expect("storage")
        .load()
        .expect("load sessions.json")
}

pub async fn has_disk_watch_handle(state: &Arc<AppState>, profile: &str) -> bool {
    state.disk_watch_handles.lock().await.contains_key(profile)
}

pub fn build_router_for_test(state: Arc<AppState>) -> axum::Router {
    super::router::build_router(state)
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

pub async fn reload_disk_only_for_test(
    state: &Arc<AppState>,
    fresh: Vec<Instance>,
    live_worker_records: Vec<(crate::process::worker_registry::WorkerRecord, String)>,
) {
    let read_epoch = state
        .mutation_epoch
        .load(std::sync::atomic::Ordering::SeqCst);
    super::reload::reload_state_instances_from_disk(
        state,
        fresh,
        live_worker_records,
        super::state::StatusSource::DiskOnly,
        read_epoch,
    )
    .await
}

pub async fn reload_tmux_applied_for_test(
    state: &Arc<AppState>,
    fresh: Vec<Instance>,
    live_worker_records: Vec<(crate::process::worker_registry::WorkerRecord, String)>,
) {
    let read_epoch = state
        .mutation_epoch
        .load(std::sync::atomic::Ordering::SeqCst);
    super::reload::reload_state_instances_from_disk(
        state,
        fresh,
        live_worker_records,
        super::state::StatusSource::TmuxApplied,
        read_epoch,
    )
    .await
}
