//! Test-only constructors that integration tests in `tests/` need to drive
//! `reload_state_instances_from_disk` and the dynamic-profile-rewire helpers without going
//! through the full daemon.

use super::*;
use crate::file_watch::FileWatchService;
use crate::server::push::STATUS_CHANNEL_CAPACITY;
use crate::server::rate_limit::RateLimiter;
use crate::session::Instance;
use crate::session::Storage;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;
use tokio::sync::{broadcast, RwLock};

static RUNTIME_ENV_LOCK: Mutex<()> = Mutex::new(());

struct RuntimeEnvGuard {
    previous: Option<OsString>,
    _lock: MutexGuard<'static, ()>,
}

impl RuntimeEnvGuard {
    fn set(value: &Path) -> Self {
        let lock = RUNTIME_ENV_LOCK
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", value);
        Self {
            previous,
            _lock: lock,
        }
    }
}

impl Drop for RuntimeEnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
    }
}
use tokio_util::sync::CancellationToken;

/// Build a minimal `Arc<AppState>` for helper-equivalence tests.
pub fn build_test_app_state(prior: Vec<Instance>) -> Arc<AppState> {
    build_test_app_state_with_policy(prior, Vec::new(), Vec::new(), None)
}

/// Like [`build_test_app_state`] but with CityHall client mode on, so route
/// tests can assert the mode's 403/400 guards fire (#7).
pub fn build_test_app_state_cityhall(prior: Vec<Instance>) -> Arc<AppState> {
    build_test_app_state_impl(
        prior,
        Vec::new(),
        Vec::new(),
        None,
        true,
        std::convert::identity,
    )
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
    build_test_app_state_impl(
        prior,
        allowed_hosts,
        allowed_origins,
        token,
        false,
        std::convert::identity,
    )
}

/// Like [`build_test_app_state`] but every worker start goes through
/// `launcher`, so a test decides how a start ends.
#[cfg(test)]
pub(crate) fn build_test_app_state_with_launcher(
    prior: Vec<Instance>,
    launcher: crate::acp::supervisor::Launcher,
) -> Arc<AppState> {
    build_test_app_state_impl(prior, Vec::new(), Vec::new(), None, false, |s| {
        s.with_launcher(launcher)
    })
}

fn build_test_app_state_impl(
    prior: Vec<Instance>,
    allowed_hosts: Vec<String>,
    allowed_origins: Vec<String>,
    token: Option<String>,
    cityhall_mode: bool,
    customize: impl FnOnce(
        crate::acp::supervisor::Supervisor<crate::acp::supervisor::ChannelSink>,
    )
        -> crate::acp::supervisor::Supervisor<crate::acp::supervisor::ChannelSink>,
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
    let supervisor = std::sync::Arc::new(customize(
        crate::acp::supervisor::Supervisor::with_capacity(sink, 1),
    ));
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
        list_sessions_resolver_misses: std::sync::atomic::AtomicUsize::new(0),
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
        metrics_sampler: tokio::sync::Mutex::new(Default::default()),
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

pub async fn rename_profile_disk_watch(state: &Arc<AppState>, old: &str, new: &str) {
    super::rename_profile_disk_watch(state, old, new).await
}

/// Replace the `Arc<FileWatchService>` on a unique-Arc'd `AppState`.
pub fn replace_file_watch(state: &mut AppState, fw: Arc<crate::file_watch::FileWatchService>) {
    state.file_watch = fw;
}

/// Read the current `Arc<FileWatchService>` for tests asserting on `subscriber_count`.
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

/// A live local runtime read, in a namespace of the test's own.
///
/// The producer resolves the app dir from the environment, so the harness
/// points `XDG_CONFIG_HOME` at a temporary base first. Bases are tried in
/// order and the real publisher is the oracle: a base whose ancestor chain the
/// trusted walk would reject is reported as `app_dir_untrusted` and skipped.
pub struct RuntimeUdsTestServer {
    /// The temporary base a publication owns, when this harness created it.
    _namespace: Option<tempfile::TempDir>,
    app_dir: std::path::PathBuf,
    _env: RuntimeEnvGuard,
    listener: tokio::task::JoinHandle<()>,
}

impl RuntimeUdsTestServer {
    pub fn start(state: Arc<AppState>) -> Result<Self, String> {
        let mut bases: Vec<std::path::PathBuf> = Vec::new();
        if let Some(base) = std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from) {
            bases.push(base);
        }
        bases.push(std::env::temp_dir());
        if let Some(home) = dirs::home_dir() {
            bases.push(home);
        }
        let mut rejected = Vec::new();
        for base in bases {
            let namespace = match tempfile::tempdir_in(&base) {
                Ok(namespace) => namespace,
                Err(error) => {
                    rejected.push(format!("{}: {error}", base.display()));
                    continue;
                }
            };
            let env = RuntimeEnvGuard::set(namespace.path());
            match super::runtime_uds::publish() {
                Ok(published) => {
                    let listener = crate::task_util::spawn_supervised(
                        "runtime.uds.test",
                        crate::task_util::PanicPolicy::Log,
                        super::runtime_uds::serve(state, published),
                    );
                    return Ok(Self {
                        app_dir: namespace.path().join(crate::session::APP_DIR_NAME_XDG),
                        _namespace: Some(namespace),
                        _env: env,
                        listener,
                    });
                }
                Err(error) => {
                    if error.code() != "app_dir_untrusted" {
                        return Err(error.to_string());
                    }
                    rejected.push(format!("{}: {error}", base.display()));
                }
            }
        }
        Err(format!("no trusted app dir base: {rejected:?}"))
    }

    /// A live local read published into the XDG base the caller chose, so the
    /// store under it is the one the daemon serves and the one a client
    /// pointed at the same base resolves.
    ///
    /// The base is what [`Self::start`] also takes: the app dir is
    /// `base/<APP_DIR_NAME_XDG>`, exactly where the client looks for it.
    pub fn start_in(xdg_base: &std::path::Path, state: Arc<AppState>) -> Result<Self, String> {
        let app_dir = xdg_base.join(crate::session::APP_DIR_NAME_XDG);
        std::fs::create_dir_all(&app_dir).map_err(|error| error.to_string())?;
        let env = RuntimeEnvGuard::set(xdg_base);
        let published = super::runtime_uds::publish().map_err(|error| error.to_string())?;
        let listener = crate::task_util::spawn_supervised(
            "runtime.uds.test",
            crate::task_util::PanicPolicy::Log,
            super::runtime_uds::serve(state, published),
        );
        Ok(Self {
            app_dir,
            _namespace: None,
            _env: env,
            listener,
        })
    }

    /// The app dir this publication owns, for assertions on its artifacts.
    pub fn app_dir(&self) -> std::path::PathBuf {
        self.app_dir.clone()
    }

    /// Join after the daemon's shutdown is cancelled: the accept loop returns
    /// and its artifacts are retracted before this resolves.
    pub async fn join(self) {
        let _ = self.listener.await;
    }
}
