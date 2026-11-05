//! Test-only constructors that integration tests in `tests/` need to drive
//! `reload_state_instances_from_disk` and the dynamic-profile-rewire helpers
//! without going through the full daemon.

use super::*;
use crate::file_watch::FileWatchService;
use crate::server::push::STATUS_CHANNEL_CAPACITY;
use crate::server::rate_limit::RateLimiter;
use crate::session::Instance;
use crate::session::Storage;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::sync::{Arc, MutexGuard};
use std::time::Duration;
use tokio::sync::{broadcast, RwLock};

/// Excludes environment changes; drop owned roots before this guard.
pub struct RuntimeEnvGuard {
    previous: Option<OsString>,
    previous_home: Option<OsString>,
    _lock: Option<MutexGuard<'static, ()>>,
}

impl RuntimeEnvGuard {
    /// Keep macOS legacy discovery inside the owned fixture too.
    pub fn set(value: &Path) -> Self {
        let mut guard = Self::read_lock();
        guard.bind(value);
        guard
    }

    /// Acquire before selecting an environment-derived temporary parent.
    pub fn read_lock() -> Self {
        let lock = crate::test_env_lock::acquire_env_lock(|| {});
        Self {
            previous: std::env::var_os("XDG_CONFIG_HOME"),
            previous_home: std::env::var_os("HOME"),
            _lock: lock,
        }
    }

    /// Change the fixture root without releasing exclusion or its original restore state.
    pub fn bind(&mut self, value: &Path) {
        std::env::set_var("HOME", value);
        std::env::set_var("XDG_CONFIG_HOME", value);
    }
}

impl Drop for RuntimeEnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        match &self.previous_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        crate::test_env_lock::release_env_lock(self._lock.is_some());
    }
}

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
    build_test_app_state_impl(
        prior,
        Vec::new(),
        Vec::new(),
        None,
        true,
        |_| {},
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
        |_| {},
        std::convert::identity,
    )
}

pub fn build_test_app_state_with_policy_configured(
    prior: Vec<Instance>,
    allowed_hosts: Vec<String>,
    allowed_origins: Vec<String>,
    token: Option<String>,
    configure: impl FnOnce(&mut AppState),
) -> Arc<AppState> {
    build_test_app_state_impl(
        prior,
        allowed_hosts,
        allowed_origins,
        token,
        false,
        configure,
        std::convert::identity,
    )
}

pub fn build_test_app_state_configured(
    prior: Vec<Instance>,
    configure: impl FnOnce(&mut AppState),
) -> Arc<AppState> {
    build_test_app_state_impl(
        prior,
        Vec::new(),
        Vec::new(),
        None,
        false,
        configure,
        std::convert::identity,
    )
}

/// A state for ingress tests: optional bearer token and passphrase login, with the
/// plaintext-remote gate and proxy mode set explicitly.
pub fn build_test_app_state_ingress(
    token: Option<String>,
    passphrase: Option<&str>,
    proxy_mode: bool,
    allow_plaintext_remote: bool,
) -> Arc<AppState> {
    build_test_app_state_with_policy_configured(
        Vec::new(),
        vec!["localhost".to_string()],
        vec!["http://localhost:8080".to_string()],
        token,
        |state| {
            state.login_manager = Arc::new(login::LoginManager::new(passphrase));
            state.behind_tunnel = proxy_mode;
            state.trust_cf_header = proxy_mode;
            state.allow_plaintext_remote = allow_plaintext_remote;
        },
    )
}

/// Like [`build_test_app_state`] but every worker start goes through
/// `launcher`, so a test decides how a start ends.
#[cfg(test)]
pub(crate) fn build_test_app_state_with_launcher(
    prior: Vec<Instance>,
    launcher: crate::acp::supervisor::Launcher,
) -> Arc<AppState> {
    build_test_app_state_impl(
        prior,
        Vec::new(),
        Vec::new(),
        None,
        false,
        |_| {},
        |s| s.with_launcher(launcher),
    )
}

pub(crate) fn build_test_app_state_impl(
    prior: Vec<Instance>,
    allowed_hosts: Vec<String>,
    allowed_origins: Vec<String>,
    token: Option<String>,
    cityhall_mode: bool,
    configure: impl FnOnce(&mut AppState),
    customize: impl FnOnce(
        crate::acp::supervisor::Supervisor<crate::acp::supervisor::ChannelSink>,
    )
        -> crate::acp::supervisor::Supervisor<crate::acp::supervisor::ChannelSink>,
) -> Arc<AppState> {
    build_test_app_state_impl_with_event_store(
        prior,
        allowed_hosts,
        allowed_origins,
        token,
        None,
        |state| {
            state.cityhall_mode = cityhall_mode;
            configure(state);
        },
        customize,
    )
}

pub(crate) fn build_test_app_state_impl_with_event_store(
    prior: Vec<Instance>,
    allowed_hosts: Vec<String>,
    allowed_origins: Vec<String>,
    token: Option<String>,
    event_store: Option<Arc<crate::acp::event_store::EventStore>>,
    configure: impl FnOnce(&mut AppState),
    customize: impl FnOnce(
        crate::acp::supervisor::Supervisor<crate::acp::supervisor::ChannelSink>,
    )
        -> crate::acp::supervisor::Supervisor<crate::acp::supervisor::ChannelSink>,
) -> Arc<AppState> {
    build_test_app_state_with_event_capacity(
        prior,
        allowed_hosts,
        allowed_origins,
        token,
        (event_store, 8),
        configure,
        customize,
    )
}

/// The managed SDK host observes complete bounded workloads without changing retention.
pub(crate) fn build_test_app_state_with_event_capacity(
    prior: Vec<Instance>,
    allowed_hosts: Vec<String>,
    allowed_origins: Vec<String>,
    token: Option<String>,
    events: (Option<Arc<crate::acp::event_store::EventStore>>, usize),
    configure: impl FnOnce(&mut AppState),
    customize: impl FnOnce(
        crate::acp::supervisor::Supervisor<crate::acp::supervisor::ChannelSink>,
    )
        -> crate::acp::supervisor::Supervisor<crate::acp::supervisor::ChannelSink>,
) -> Arc<AppState> {
    let (event_store, event_capacity) = events;
    let app_dir = event_store
        .is_none()
        .then(|| tempfile::tempdir().expect("tempdir"));
    let event_store = event_store.unwrap_or_else(|| {
        let acp_db = app_dir
            .as_ref()
            .expect("default store directory")
            .path()
            .join("acp_events.db");
        Arc::new(crate::acp::event_store::EventStore::open(&acp_db, 100).expect("event store"))
    });
    let acp_events_tx = broadcast::channel::<AcpBroadcastFrame>(event_capacity).0;
    let acp_control_cache = Arc::clone(event_store.control_cache());
    let sink = std::sync::Arc::new(crate::acp::supervisor::ChannelSink {
        tx: acp_events_tx.clone(),
        event_store: event_store.clone(),
    });
    let supervisor = std::sync::Arc::new(
        customize(crate::acp::supervisor::Supervisor::with_capacity(sink, 1))
            .with_native_admission(),
    );
    supervisor.hydrate_seqs(
        event_store
            .all_session_seqs()
            .expect("event publication frontiers"),
    );
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
    let shutdown = session_service.work.shutdown.clone();
    let mut state = AppState {
        profile: "test".to_owned().into(),
        core_only: false,
        read_only: false,
        cityhall_mode: false,
        instances,
        profile_namespace: Arc::new(RwLock::new(())),
        publication: Arc::new(RwLock::new(())),
        reload_lane: tokio::sync::Mutex::new(()),
        canonical_metadata: RwLock::new(super::reload::CanonicalMetadata {
            default_profile: "test".into(),
            ..Default::default()
        }),
        canonical_health: RwLock::new(crate::daemon::RuntimeHealth::Healthy),
        canonical_diagnostics: Default::default(),
        runtime: super::runtime::NativeRuntime::new(
            "test".into(),
            None,
            session_service.work.clone(),
        ),
        runtime_read_cache: Default::default(),
        session_service,
        token_manager: Arc::new(TokenManager::new(token, Duration::from_secs(3600))),
        login_manager: Arc::new(login::LoginManager::new(None)),
        passkey_manager: Arc::new(super::passkeys::PasskeyManager::in_memory()),
        rate_limiter: Arc::new(RateLimiter::new()),
        behind_tunnel: false,
        trust_cf_header: false,
        // Handler tests drive requests from arbitrary peers; the 426 gate has its own tests.
        allow_plaintext_remote: true,
        auth_mode: "none",
        serve_mode: "local",
        allowed_hosts,
        allowed_origins,
        instance_locks,
        idempotency_locks,
        create_progress: Default::default(),
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
        runtime_read_semaphore: Arc::new(tokio::sync::Semaphore::new(
            crate::server::runtime_ws::RUNTIME_READ_CONCURRENCY,
        )),
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
        lifecycle: crate::server::lifecycle_feed::LifecycleFeed::new(),
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
        acp_orphan_cleanups: std::sync::Mutex::new(std::collections::HashSet::new()),
        shutdown,
        file_watch,
        disk_changed: Arc::new(tokio::sync::Notify::new()),
        disk_watch_handles: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
    };
    configure(&mut state);
    let state = Arc::new(state);
    state.session_service.bind_native_state(&state);
    state
}

pub async fn refresh_canonical_metadata_for_test(state: &Arc<AppState>) {
    let loaded = super::reload::load_all_profiles(&state.file_watch).expect("load profiles");
    *state.canonical_metadata.write().await = loaded.metadata;
}

/// Accept fixture disk rows and component health through the normal reload commit.
pub async fn accept_runtime_read_cache_for_test(state: &Arc<AppState>) {
    let read_epoch = state
        .mutation_epoch
        .load(std::sync::atomic::Ordering::SeqCst);
    let (loaded, metadata) = super::reload::load_profiles_for_reload(&state.file_watch);
    super::reload::reload_state_instances_from_disk(
        state,
        loaded,
        Vec::new(),
        super::state::StatusSource::DiskOnly,
        read_epoch,
        metadata,
        Default::default(),
    )
    .await;
}

#[cfg(test)]
pub(crate) fn select_retained_for_test(row: &mut Instance) {
    assert!(row.acp_establishment.is_none());
    assert!(row.fork_pending.is_none() && row.import_pending != Some(true));
    let id = row
        .acp_session_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    row.resume_intent = crate::session::ResumeIntent::Use(id);
}

pub async fn drain_session_id_updates_for_test(state: &Arc<AppState>) {
    super::session_identity::drain_session_id_updates_in_state(state).await;
}

pub fn attach_session_id_update_for_test(inst: &mut Instance, sid: &str) {
    let poller = crate::session::poller::SessionPoller::new(
        format!("test-tmux-{}", inst.id),
        inst.tool.clone(),
        inst.active_execution.clone(),
    );
    poller.inject_test_update(&inst.id, sid);
    inst.capture_poller
        .set_handle(Some(Arc::new(std::sync::Mutex::new(poller))));
}

/// The Contract Pack's recording entry point, re-exported because
/// `runtime_ws` itself is crate-private and the recorder is an out-of-process
/// tool: it rewrites committed transcripts, so it lives in `tests/`.
pub use super::runtime_ws::{record_exchange, RecordedExchange, RecordedOwner, RecordingPins};

pub fn seed_instances_on_disk_for_test(profile: &str, insts: Vec<Instance>) {
    let storage = Storage::new_unwatched(profile).expect("storage");
    storage
        .update(move |instances, _groups| {
            *instances = insts;
            Ok(())
        })
        .expect("seed sessions.json");
}

/// Make storage for `profile` fail on every write: a plain file where its directory belongs.
pub fn block_profile_storage_for_test(profile: &str) {
    let dir = crate::session::get_app_dir()
        .expect("app dir")
        .join("profiles");
    std::fs::create_dir_all(&dir).expect("profiles dir");
    std::fs::write(dir.join(profile), b"").expect("block profile");
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

pub use super::api::system::{create_profile, delete_profile, rename_profile};

pub async fn add_profile_disk_watch(state: &Arc<AppState>, profile: &str) {
    super::add_profile_disk_watch(state, profile).await
}

pub async fn rename_profile_disk_watch(state: &Arc<AppState>, old: &str, new: &str) {
    super::rename_profile_disk_watch(state, old, new).await
}

/// Configure both collaborators before binding the native runtime.
pub fn replace_file_watch(state: &mut AppState, fw: Arc<crate::file_watch::FileWatchService>) {
    Arc::get_mut(&mut state.session_service)
        .expect("configure session service before sharing it")
        .file_watch = fw.clone();
    state.file_watch = fw;
}

/// Read the current `Arc<FileWatchService>` for tests asserting on
/// `subscriber_count`. The Arc clone is cheap.
pub fn file_watch(state: &AppState) -> Arc<crate::file_watch::FileWatchService> {
    state.file_watch.clone()
}

pub async fn reload_disk_snapshot_at_epoch_for_test(
    state: &Arc<AppState>,
    fresh: Vec<Instance>,
    read_epoch: u64,
) {
    let metadata = state.canonical_metadata.read().await.clone();
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        super::reload::reload_state_instances_from_disk(
            state,
            fresh.into(),
            Vec::new(),
            super::state::StatusSource::DiskOnly,
            read_epoch,
            Ok(metadata),
            Default::default(),
        ),
    )
    .await
    .expect("disk snapshot reload did not finish within 30 seconds");
}

pub async fn reload_disk_only_for_test(
    state: &Arc<AppState>,
    fresh: Vec<Instance>,
    live_worker_records: Vec<(crate::process::worker_registry::WorkerRecord, String)>,
) {
    let read_epoch = state
        .mutation_epoch
        .load(std::sync::atomic::Ordering::SeqCst);
    let metadata = state.canonical_metadata.read().await.clone();
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        super::reload::reload_state_instances_from_disk(
            state,
            fresh.into(),
            live_worker_records,
            super::state::StatusSource::DiskOnly,
            read_epoch,
            Ok(metadata),
            Default::default(),
        ),
    )
    .await
    .expect("disk-only reload did not finish within 30 seconds");
}

pub async fn reload_tmux_applied_for_test(
    state: &Arc<AppState>,
    fresh: Vec<Instance>,
    live_worker_records: Vec<(crate::process::worker_registry::WorkerRecord, String)>,
) {
    let read_epoch = state
        .mutation_epoch
        .load(std::sync::atomic::Ordering::SeqCst);
    let metadata = state.canonical_metadata.read().await.clone();
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        super::reload::reload_state_instances_from_disk(
            state,
            fresh.into(),
            live_worker_records,
            super::state::StatusSource::TmuxApplied,
            read_epoch,
            Ok(metadata),
            Default::default(),
        ),
    )
    .await
    .expect("Tmux-applied reload did not finish within 30 seconds");
}

/// Install a global `before_session` hook that creates `<label>-ready` in `dir` and blocks
/// until `<label>-release` exists, so a test can commit a peer write inside a launch's hook
/// window. Returns `(ready, release)`.
#[cfg(test)]
pub(crate) fn install_blocking_before_session_hook(
    dir: &std::path::Path,
    label: &str,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let ready = dir.join(format!("{label}-ready"));
    let release = dir.join(format!("{label}-release"));
    let hook = format!(
        ": > '{}'; while [ ! -e '{}' ]; do sleep 0.01; done",
        ready.display(),
        release.display()
    );
    crate::session::config::update_config(|global| {
        global.host_hooks.before_session = vec![hook];
    })
    .expect("install before_session hook");
    (ready, release)
}

/// A launcher that counts its calls and fails every launch.
#[cfg(test)]
pub(crate) fn counting_failing_launcher() -> (
    crate::acp::supervisor::Launcher,
    Arc<std::sync::atomic::AtomicUsize>,
) {
    let launches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&launches);
    let launcher: crate::acp::supervisor::Launcher = Arc::new(move |_config, _id| {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async {
            Err::<crate::acp::acp_client::AcpClient, _>(crate::acp::acp_client::AcpError::Spawn(
                "test launcher".into(),
            ))
        })
    });
    (launcher, launches)
}

/// Fails every `sessions.json` write while the guard lives, leaving rows loadable.
/// The failpoint is process-wide, so the test must be serial.
#[cfg(test)]
pub(crate) fn fail_session_writes_for_test(profile: &str) -> impl Drop {
    Storage::new_unwatched(profile)
        .expect("storage")
        .fail_writes_for_test()
}

/// Once the blocking hook signals `ready`, archive the stored rows matching `is_target` under
/// their lifecycle lock, as `aoe session archive --no-kill` does, then release the hook.
/// Returns whether the hook ran and a row was archived; the hook is released either way.
#[cfg(test)]
pub(crate) async fn archive_while_hook_waits(
    (ready, release): &(std::path::PathBuf, std::path::PathBuf),
    profile: &str,
    is_target: impl Fn(&Instance) -> bool,
) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !ready.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let storage = Storage::new_unwatched(profile).expect("storage");
    let target = ready
        .exists()
        .then(|| storage.load().expect("load rows"))
        .and_then(|rows| rows.into_iter().find(|row| is_target(row)));
    if let Some(target) = &target {
        let _lock = storage
            .acquire_instance_lifecycle_lock(&target.id)
            .expect("lifecycle lock");
        storage
            .update(|rows, _| {
                if let Some(row) = rows.iter_mut().find(|row| row.id == target.id) {
                    row.archive();
                }
                Ok(())
            })
            .expect("archive stored row");
    }
    std::fs::write(release, b"release").expect("release hook");
    target.is_some()
}

/// Serve a fixture router through the real owner-authenticated private listener.
pub async fn serve_owner_router_for_test(
    socket: &std::path::Path,
    router: axum::Router,
) -> tokio::task::JoinHandle<()> {
    let (listener, lease) = super::peer::bind_private(socket)
        .await
        .expect("fixture private listener");
    tokio::spawn(async move {
        let _lease = lease;
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<super::peer::ConnectionPeer>(),
        )
        .await
        .expect("fixture owner server");
    })
}

/// Everything `f` logs through a plain fmt layer, so tests can assert on the rendered line.
#[cfg(test)]
pub fn capture_log(f: impl FnOnce()) -> String {
    use std::io::Write;
    use tracing_subscriber::layer::SubscriberExt as _;
    struct Buf(Arc<std::sync::Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, src: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(src);
            Ok(src.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let buf = Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    let writer = buf.clone();
    let layer = tracing_subscriber::fmt::layer()
        .with_writer(move || Buf(writer.clone()))
        .with_ansi(false);
    tracing::subscriber::with_default(
        tracing_subscriber::registry::Registry::default().with(layer),
        f,
    );
    let out = buf.lock().unwrap().clone();
    String::from_utf8(out).unwrap()
}
#[cfg(test)]
pub(crate) struct ManagedControlSession {
    pub(crate) client: crate::acp::supervisor::test_support::managed::ManagedClient,
    pub(crate) native: crate::acp::supervisor::test_support::managed::NativePeer,
    pub(crate) state: Arc<super::AppState>,
    pub(crate) sid: String,
    prompts: tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
}

#[cfg(test)]
impl ManagedControlSession {
    pub(crate) fn id(&self) -> &str {
        &self.client.id
    }

    pub(crate) async fn next_prompt(&mut self) -> serde_json::Value {
        tokio::time::timeout(Duration::from_secs(10), self.prompts.recv())
            .await
            .expect("actual native prompt arrives")
            .expect("native request lane remains open")
    }

    pub(crate) async fn start_prompt(&mut self, text: &str) -> serde_json::Value {
        use super::session_service::{SendTurnRequest, SessionCaller};
        let id = self.client.id.clone();
        let submission = self.state.session_service.prompt_submission(&id).await;
        self.state
            .session_service
            .send_turn(
                &SessionCaller::User,
                &id,
                SendTurnRequest {
                    resume_authority: crate::process::worker_registry::ResumeAuthority::Explicit,
                    resume_reservation: None,
                    text,
                    attachments: &[],
                    woke_idle_dormant: false,
                    prompt_id: None,
                    synthesized: false,
                    no_revive: false,
                },
            )
            .await
            .unwrap_or_else(|error| {
                panic!("explicit input rejected by real retained actor: {error}")
            });
        drop(submission);
        let request = self.next_prompt().await;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if self
                    .state
                    .session_service
                    .fold_control_state(&id)
                    .await
                    .unwrap()
                    .turn_active
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the actual Native input establishes durable active state");
        request
    }

    pub(crate) async fn finish_prompt(
        &mut self,
        request: &serde_json::Value,
        error_kind: Option<&str>,
    ) {
        let response = match error_kind {
            Some(kind) => serde_json::json!({"jsonrpc":"2.0","id":request["id"],
                "error":{"code":-32603,"message":"original request failed","data":{"errorKind":kind}}}),
            None => {
                serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":{"stopReason":"end_turn"}})
            }
        };
        self.native.send(response).await;
        let reason = if error_kind == Some("rate_limit") {
            "rate_limited"
        } else if error_kind.is_some() {
            "prompt_failed"
        } else {
            "prompt_complete"
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let event = self
                    .client
                    .next_event()
                    .await
                    .expect("managed drain stays alive");
                if matches!(&event, crate::acp::Event::Stopped { reason: got } if got == reason) {
                    break;
                }
                assert!(!matches!(
                    event,
                    crate::acp::Event::AgentStartupError { .. }
                        | crate::acp::Event::AcpConnectionUnavailable { .. }
                ));
            }
        })
        .await
        .expect("the original result reaches its durable terminal");
    }

    pub(crate) async fn park(&mut self, text: &str) {
        let request = self.start_prompt(text).await;
        self.finish_prompt(&request, Some("rate_limit")).await;
        assert!(matches!(
            self.state
                .acp_event_store
                .rate_limit_park(self.id())
                .unwrap(),
            crate::acp::event_store::ControlKnowledge::Known(Some(_))
        ));
    }

    /// Change clock data only, preserving actual Native provenance and durable budget.
    pub(crate) fn make_quota_due(&self) {
        self.state
            .acp_event_store
            .test_conn()
            .execute(
                "UPDATE acp_events SET created_at=?2 WHERE session_id=?1",
                rusqlite::params![
                    self.id(),
                    (chrono::Utc::now() - chrono::Duration::days(3)).timestamp_millis()
                ],
            )
            .unwrap();
        self.state.acp_event_store.control_cache().forget(self.id());
    }
}

#[cfg(test)]
pub(crate) async fn managed_control_session(
    scope: &mut crate::acp::supervisor::test_support::managed::ManagedScope,
    label: &str,
) -> ManagedControlSession {
    let project = scope.tempdir();
    let native_store = crate::session::get_app_dir()
        .unwrap()
        .join("managed-control-native-store");
    std::fs::create_dir_all(&native_store).unwrap();
    scope.save_environment(&["CLAUDE_CONFIG_DIR"]);
    std::env::set_var("CLAUDE_CONFIG_DIR", &native_store);
    let sid = uuid::Uuid::new_v4().to_string();
    let response_sid = sid.clone();
    let (sent, prompts) = tokio::sync::mpsc::unbounded_channel();
    let mut config_options = serde_json::json!([{
        "id": "model", "name": "Model", "category": "model", "type": "select",
        "currentValue": "old-model", "options": [
            {"value": "old-model", "name": "Old model"},
            {"value": "new-model", "name": "New model"},
            {"value": "plan", "name": "Plan"}
        ]
    }]);
    let mut modes = serde_json::json!({
        "currentModeId": "old-mode", "availableModes": [
            {"id": "old-mode", "name": "Old mode"},
            {"id": "new-mode", "name": "New mode"},
            {"id": "plan", "name": "Plan"}
        ]
    });
    let (config, native) = scope.native_peer(
        project.path().into(),
        "claude",
        "claude",
        move |request| match request["method"].as_str().unwrap() {
            "initialize" => Some(serde_json::json!({"jsonrpc":"2.0","id":request["id"],
                "result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}})),
            "session/new" | "session/load" => {
                let sid = if request["method"] == "session/new" {
                    serde_json::Value::String(response_sid.clone())
                } else {
                    request["params"]["sessionId"].clone()
                };
                Some(serde_json::json!({"jsonrpc":"2.0","id":request["id"],
                    "result":{"sessionId":sid,"configOptions":config_options,"modes":modes}}))
            }
            "session/set_mode" => {
                modes["currentModeId"] = request["params"]["modeId"].clone();
                Some(serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":{}}))
            }
            "session/set_config_option" => {
                assert_eq!(request["params"]["configId"], "model");
                config_options[0]["currentValue"] = request["params"]["value"].clone();
                Some(serde_json::json!({"jsonrpc":"2.0","id":request["id"],
                    "result":{"configOptions":config_options}}))
            }
            "session/prompt" => {
                sent.send(request.clone()).unwrap();
                None
            }
            "session/cancel" => None,
            other => panic!("unexpected Native method: {other}"),
        },
    );
    let mut client = scope.spawn(config, label).await.unwrap();
    client.ready().await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = client
                .next_event()
                .await
                .expect("actual original assignment");
            if matches!(&event, crate::acp::Event::AcpSessionAssigned {
                acp_session_id, connection_ready: true, native_subject: Some(_),
            } if acp_session_id == &sid)
            {
                break;
            }
            assert!(!matches!(
                event,
                crate::acp::Event::AgentStartupError { .. }
            ));
        }
    })
    .await
    .unwrap();
    let state = scope.state.clone();
    accept_runtime_read_cache_for_test(&state).await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let finalized = state
                .instances
                .read()
                .await
                .iter()
                .find(|row| row.id == client.id)
                .is_some_and(|row| row.launch_is_finalized());
            if finalized {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the actual creation finishes before control fixture use");
    let creation = tokio::time::timeout(
        Duration::from_secs(10),
        state.session_service.prompt_submission(&client.id),
    )
    .await
    .expect("the actual creation releases its submission custody");
    drop(creation);
    let owner = state
        .acp_supervisor
        .current_native_availability(&client.id)
        .await
        .unwrap()
        .owner
        .expect("actual runner birth");
    assert_eq!(Some(owner), client.lease.spawned_runner());
    assert_ne!(owner.pid, std::process::id());
    ManagedControlSession {
        client,
        native,
        state,
        sid,
        prompts,
    }
}

/// DATA-only fixture. Never pass this metadata to a sink, Native gate, resource, or runtime admission.
#[cfg(test)]
pub(crate) fn sql_projection_metadata() -> crate::acp::event_sender::EventMetadata {
    use crate::acp::event_sender::{EventMetadata, EventScope};
    EventMetadata {
        scope: EventScope::OperationCurrent,
        native_origin: Some(crate::acp::state::NativeEventOrigin {
            context: serde_json::from_value(serde_json::to_value([0_u8; 32]).unwrap()).unwrap(),
            generation: 1,
            attempt_id: serde_json::from_value(serde_json::json!(uuid::Uuid::new_v4())).unwrap(),
            producer: uuid::Uuid::new_v4(),
            owner: None,
        }),
        operation: Some(crate::acp::protocol::AcpOperationMetadata {
            correlation: crate::acp::establishment::NativeOperationCorrelation::new(None),
            outcome: None,
        }),
        ..EventMetadata::history()
    }
}
