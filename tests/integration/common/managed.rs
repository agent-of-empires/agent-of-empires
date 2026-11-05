//! Private real-runner composition for the managed SDK consumer suites.

#[path = "node.rs"]
mod node;

use std::collections::HashMap;
use std::future::Future;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::Json;
use futures_util::FutureExt;
use tokio::sync::broadcast;

use super::spawn_request;
use crate::acp::acp_client::{AcpClient, SdkTestOptions, SpawnConfig};
use crate::acp::event_store::EventStore;
use crate::acp::runner_lifecycle::{Lease, ProcessControl, SystemProcessControl};
use crate::acp::state::Event;
use crate::acp::supervisor::{ChannelSink, Launcher, Supervisor, SupervisorError};
use crate::process::runner::test_support::managed_runner::{
    child_if_requested, NativeSpawnProof, RunnerInspection,
};
use crate::server::{AcpBroadcastFrame, AppState};

/// Node shim used by the managed consumer suites.
pub fn shim_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("acp-worker")
        .join("test-shim")
        .join("shim.mjs")
}

/// Reports missing Node/shim dependencies before isolated setup.
pub fn shim_ready() -> Result<(), String> {
    shim_node()?;
    let shim = shim_path();
    if !shim.exists() {
        return Err(format!("shim missing at {}", shim.display()));
    }
    let node_modules = shim.parent().unwrap().join("node_modules");
    if !node_modules.exists() {
        return Err(
            "shim deps not installed; run `cd acp-worker/test-shim && npm ci` first".into(),
        );
    }
    Ok(())
}

/// Resolve the runtime behind version-manager launchers before tests isolate
/// HOME: probe and spawn must use the same executable.
pub fn shim_node() -> Result<&'static Path, String> {
    static NODE: std::sync::OnceLock<Result<PathBuf, String>> = std::sync::OnceLock::new();
    NODE.get_or_init(|| {
        let output = std::process::Command::new("node")
            .args(["--print", "process.execPath"])
            .output()
            .map_err(|error| format!("cannot resolve Node runtime: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "cannot resolve Node runtime: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        let path = String::from_utf8(output.stdout)
            .map_err(|error| format!("invalid Node runtime path: {error}"))?;
        std::fs::canonicalize(path.trim())
            .map_err(|error| format!("cannot resolve Node executable: {error}"))
    })
    .as_ref()
    .map(PathBuf::as_path)
    .map_err(Clone::clone)
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct IssuedOriginal {
    pub(crate) request_id: i64,
    pub(crate) scope: crate::acp::establishment::NativeRequestScope,
    pub(crate) receipt: Arc<crate::acp::establishment::NativeEstablishmentReceipt>,
}

pub struct ManagedClient {
    client: Arc<AcpClient>,
    supervisor: Arc<Supervisor<ChannelSink>>,
    pub(crate) id: String,
    pub(crate) lease: Lease,
    events: broadcast::Receiver<AcpBroadcastFrame>,
    last_seq: Option<u64>,
    retirement: Arc<ManagedRetirement>,
    state: Arc<AppState>,
    drain: tokio::task::AbortHandle,
}

impl Deref for ManagedClient {
    type Target = AcpClient;
    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

impl ManagedClient {
    pub(crate) fn actual(&self) -> Arc<AcpClient> {
        self.client.clone()
    }

    pub(crate) fn subscribe_publications(&self) -> broadcast::Receiver<AcpBroadcastFrame> {
        self.state.acp_events_tx.subscribe()
    }
    pub(crate) async fn ready(&self) {
        tokio::time::timeout(
            Duration::from_secs(30),
            self.supervisor.wait_until_ready(&self.id),
        )
        .await
        .expect("actual SDK readiness deadline")
        .expect("real SDK readiness and Applied");
    }

    pub(crate) async fn recv(&mut self) -> Option<Event> {
        self.next_event().await
    }

    pub(crate) fn try_recv(&mut self) -> Result<Event, broadcast::error::TryRecvError> {
        loop {
            let frame = match self.events.try_recv() {
                Err(broadcast::error::TryRecvError::Lagged(count)) => {
                    panic!(
                        "managed observer lost {count} publications for {} after {:?}",
                        self.id, self.last_seq
                    );
                }
                result => result?,
            };
            if frame.session_id != self.id {
                continue;
            }
            if let Some(previous) = self.last_seq {
                assert!(frame.seq > previous);
            }
            self.last_seq = Some(frame.seq);
            return Ok((*frame.event).clone());
        }
    }

    pub(crate) async fn pause_drain(&self) -> crate::acp::supervisor::teardown::TestDrainPause {
        self.supervisor
            .test_pause_drain(&self.lease, &self.client)
            .await
            .expect("exact original drain acknowledges its actual receive boundary")
    }

    pub(crate) async fn prepare_reset(
        &self,
        deadline: tokio::time::Instant,
    ) -> Arc<crate::acp::supervisor::LiveFreshProposal> {
        tokio::time::timeout_at(deadline, self.ready())
            .await
            .expect("original readiness before proposal deadline");
        self.supervisor
            .prepare_live_fresh(&self.id, &self.client, deadline)
            .await
            .expect("genuine LiveFresh proposal")
    }

    pub async fn next_event(&mut self) -> Option<Event> {
        loop {
            let frame = match self.events.try_recv() {
                Ok(frame) => frame,
                Err(broadcast::error::TryRecvError::Lagged(count)) => {
                    panic!(
                        "managed observer lost {count} publications for {} after {:?}",
                        self.id, self.last_seq
                    );
                }
                Err(broadcast::error::TryRecvError::Closed) => return None,
                Err(broadcast::error::TryRecvError::Empty) => {
                    if self.drain.is_finished() {
                        return None;
                    }
                    match tokio::time::timeout(Duration::from_millis(10), self.events.recv()).await
                    {
                        Ok(Ok(frame)) => frame,
                        Ok(Err(broadcast::error::RecvError::Lagged(count))) => {
                            panic!(
                                "managed observer lost {count} publications for {} after {:?}",
                                self.id, self.last_seq
                            );
                        }
                        Ok(Err(broadcast::error::RecvError::Closed)) => return None,
                        Err(_) => continue,
                    }
                }
            };
            if frame.session_id != self.id {
                continue;
            }
            if let Some(previous) = self.last_seq {
                assert!(
                    frame.seq > previous,
                    "managed publications must remain ordered"
                );
            }
            self.last_seq = Some(frame.seq);
            return Some((*frame.event).clone());
        }
    }

    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.retirement.retire().await
    }

    pub(crate) async fn stop_session(&self) {
        let stopped = crate::server::api::sessions::stop_session(
            State(self.state.clone()),
            axum::extract::Path(self.id.clone()),
        )
        .await
        .into_response();
        assert_eq!(
            stopped.status(),
            axum::http::StatusCode::OK,
            "real Stop must settle the original projection before retained Load/Fork"
        );
        self.retirement
            .completed
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Release the stopped case's real SID ownership before reusing it.
    pub(crate) async fn stop_and_forget(&self) {
        self.stop_session().await;
        let response = crate::server::api::sessions::delete_session(
            State(self.state.clone()),
            axum::extract::Path(self.id.clone()),
            None,
        )
        .await
        .into_response();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            status,
            axum::http::StatusCode::OK,
            "permanent deletion of the stopped fixture: {}",
            String::from_utf8_lossy(&body)
        );
        assert!(!self
            .state
            .instances
            .read()
            .await
            .iter()
            .any(|row| row.id == self.id));
    }

    /// Preserve the real checkpoint before a deliberate SDK-only shutdown stimulus.
    pub(crate) async fn prepare_for_sdk_shutdown(&self) -> anyhow::Result<()> {
        let _operation = self.retirement.operation.lock().await;
        anyhow::ensure!(
            self.retirement.prepare().await?.is_some(),
            "SDK-only shutdown requires its still-owned checkpoint"
        );
        self.lease.cancellation().cancel();
        Ok(())
    }

    pub async fn reset_session(
        &self,
        text: &str,
    ) -> Result<crate::acp::acp_client::ResetSessionOutcome, crate::acp::acp_client::AcpError> {
        let now = tokio::time::Instant::now();
        let deadline = now + crate::acp::acp_client::SESSION_RESET_IN_TASK_TIMEOUT;
        let outer = now + crate::acp::acp_client::SESSION_RESET_TIMEOUT;
        let proposal = self.prepare_reset(deadline).await;
        self.client
            .reset_session(text, proposal, deadline, outer)
            .await
    }
    pub(crate) fn app_state(&self) -> Arc<AppState> {
        self.state.clone()
    }
}

/// Helper task custody survives test-body unwinding, unlike an abandoned JoinHandle.
struct ManagedTaskState {
    task: Option<tokio::task::JoinHandle<()>>,
    abort_requested: bool,
    failure: Option<String>,
}

#[derive(Clone)]
pub(crate) struct ManagedTask(Arc<Mutex<ManagedTaskState>>);

impl ManagedTask {
    pub(crate) fn abort(&self) {
        let mut state = self.0.lock().unwrap();
        if state.task.as_ref().is_some_and(|task| !task.is_finished()) {
            state.abort_requested = true;
            state.task.as_ref().unwrap().abort();
        }
    }
}

impl Future for ManagedTask {
    type Output = Result<(), tokio::task::JoinError>;
    fn poll(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let mut state = self.0.lock().unwrap();
        let Some(task) = state.task.as_mut() else {
            return std::task::Poll::Ready(Ok(()));
        };
        let result = Pin::new(task).poll(cx);
        if let std::task::Poll::Ready(Err(error)) = &result {
            if error.is_panic() || !error.is_cancelled() || !state.abort_requested {
                state.failure = Some(error.to_string());
            }
        }
        if result.is_ready() {
            state.task.take();
        }
        result
    }
}

#[derive(Clone)]
pub(crate) struct ScopedRelease(Arc<Mutex<Option<tokio::sync::oneshot::Sender<()>>>>);

impl ScopedRelease {
    pub(crate) fn release(&self) -> bool {
        self.0
            .lock()
            .unwrap()
            .take()
            .is_some_and(|sender| sender.send(()).is_ok())
    }
}

enum NativeBridgeControl {
    Raw,
    Stdout,
    Stdin,
}

pub(crate) struct NativePeer {
    writer: tokio::sync::mpsc::Sender<(serde_json::Value, tokio::sync::oneshot::Sender<()>)>,
    replies: tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl NativePeer {
    pub(crate) async fn send(&self, line: serde_json::Value) {
        let (sent, received) = tokio::sync::oneshot::channel();
        self.writer
            .send((line, sent))
            .await
            .expect("actual native pipe remains open");
        received
            .await
            .expect("native bytes were physically written");
    }

    pub(crate) async fn close_stdout(&self) {
        self.send(serde_json::json!({"__managed_close_stdout": true}))
            .await;
    }

    pub(crate) async fn reply(&mut self) -> serde_json::Value {
        self.replies
            .recv()
            .await
            .expect("actual native callback receives an SDK reply")
    }

    pub(crate) fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    /// Bounded metadata only; timeout diagnostics never wait on or expose native payloads.
    pub(crate) fn timeout_methods(&self) -> Vec<&'static str> {
        let Ok(requests) = self.requests.try_lock() else {
            return vec!["ledger_unavailable"];
        };
        requests
            .iter()
            .take(32)
            .map(|method| match method.as_str() {
                "initialize" => "initialize",
                "session/new" => "session/new",
                "session/load" => "session/load",
                "session/fork" => "session/fork",
                "session/prompt" => "session/prompt",
                "session/cancel" => "session/cancel",
                "session/set_mode" => "session/set_mode",
                "session/set_config_option" => "session/set_config_option",
                _ => "other_method",
            })
            .collect()
    }
}

#[derive(Clone)]
struct NativeLaunch {
    lease: Lease,
    inspection: RunnerInspection,
}
struct ManagedRetirement {
    state: Arc<AppState>,
    target: crate::acp::supervisor::RetirementTarget,
    prepared: std::sync::OnceLock<Arc<crate::server::acp_events::BindingRetirement>>,
    completed: std::sync::atomic::AtomicBool,
    active: std::sync::atomic::AtomicBool,
    operation: tokio::sync::Mutex<()>,
}

impl ManagedRetirement {
    fn capture(state: Arc<AppState>, lease: Lease) -> Self {
        let (identity, prime) = lease.constructed_ownership();
        let scopes = lease.scopes();
        Self {
            state,
            target: crate::acp::supervisor::RetirementTarget {
                lease: Some(lease),
                identity,
                scopes,
                prime,
                journalled: true,
            },
            prepared: std::sync::OnceLock::new(),
            completed: std::sync::atomic::AtomicBool::new(false),
            active: std::sync::atomic::AtomicBool::new(true),
            operation: tokio::sync::Mutex::new(()),
        }
    }

    async fn wait_original_constructor(&self) {
        let lease = self.target.lease.as_ref().expect("captured original lease");
        loop {
            let changed = self.state.acp_supervisor.worker_notify.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if lease.original_constructor_completed() {
                return;
            }
            changed.await;
        }
    }

    async fn prepare(
        &self,
    ) -> anyhow::Result<Option<Arc<crate::server::acp_events::BindingRetirement>>> {
        use crate::session::SessionStore as _;
        if let Some(prepared) = self.prepared.get() {
            return Ok(Some(prepared.clone()));
        }
        let lease = self.target.lease.as_ref().expect("captured original lease");
        let no_binding = lease.retirement().binding_checkpoint().is_none()
            && lease
                .retirement()
                .establishment_projection()
                .is_none_or(|projection| {
                    !projection.retirement_checkpoint_pending()
                        && !projection.has_unsettled_completion()
                });
        let no_current = self
            .state
            .acp_supervisor
            .lifecycle
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .current_lease(lease.session_id())
            .as_ref()
            != Some(lease);
        if lease.original_constructor_completed() && no_current && no_binding {
            // This completed original producer has no binding checkpoint to recreate or ACK.
            return Ok(None);
        }
        crate::server::acp_events::BindingRetirement::captured(
            &self.state,
            self.target.clone(),
            None,
        )
        .activate();
        let state = self.state.clone();
        let target = self.target.clone();
        let id = target
            .lease
            .as_ref()
            .expect("captured original lease")
            .session_id()
            .to_owned();
        let prepared = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let native = crate::server::session_store::NativeSessionStore::open(
                state.clone(),
                "test",
                None,
            )?;
            let _lifecycle = native.storage().acquire_instance_lifecycle_lock(&id)?;
            let row = native
                .load()?
                .into_iter()
                .find(|row| row.id == id)
                .ok_or(crate::session::SessionGone)?;
            let prepared = Arc::new(
                crate::server::acp_events::BindingRetirement::prepare_captured(
                    &state,
                    target,
                    &row,
                    native.storage(),
                )?,
            );
            prepared.activate();
            Ok(prepared)
        })
        .await??;
        let _ = self.prepared.set(prepared.clone());
        Ok(Some(prepared))
    }

    async fn retire(&self) -> anyhow::Result<()> {
        use crate::session::SessionStore as _;
        use std::sync::atomic::Ordering;

        let _operation = self.operation.lock().await;
        if self.completed.load(Ordering::Acquire) {
            return Ok(());
        }
        let id = self
            .target
            .lease
            .as_ref()
            .expect("captured original lease")
            .session_id()
            .to_owned();
        let prepared = self.prepare().await?;
        let mut physical_only;
        let target = if prepared.is_none() {
            // Only the proven no-binding branch may avoid creating journal ACK debt.
            physical_only = self.target.clone();
            physical_only.journalled = false;
            &physical_only
        } else {
            &self.target
        };
        self.state
            .acp_supervisor
            .shutdown_captured_and_wait(&id, target, Duration::from_secs(5))
            .await?;
        let Some(prepared) = prepared else {
            self.completed.store(true, Ordering::Release);
            return Ok(());
        };
        let state = self.state.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let native = crate::server::session_store::NativeSessionStore::open(
                state.clone(),
                "test",
                None,
            )?;
            let _lifecycle = native.storage().acquire_instance_lifecycle_lock(&id)?;
            prepared.commit_under_native_lock(&native)?;
            state
                .acp_supervisor
                .acknowledge_retirement(&prepared.target)?;
            Ok(())
        })
        .await??;
        self.completed.store(true, Ordering::Release);
        Ok(())
    }
}

#[derive(Default)]
struct CaseOneShot {
    options: SdkTestOptions,
    runner: Option<(PathBuf, RunnerInspection)>,
}

struct CaseInputs {
    template: SpawnConfig,
    first: Option<CaseOneShot>,
}

#[derive(Default)]
struct InputState {
    bound: HashMap<(String, u64), CaseInputs>,
    next: HashMap<String, CaseInputs>,
}

#[derive(Default)]
struct InputBindings {
    state: Mutex<InputState>,
    changed: tokio::sync::Notify,
    entered: tokio::sync::Notify,
}

impl InputBindings {
    fn bind(&self, id: String, generation: u64, inputs: CaseInputs) {
        let inserted = {
            let mut state = self.state.lock().unwrap();
            match state.bound.entry((id, generation)) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(inputs);
                    true
                }
                std::collections::hash_map::Entry::Occupied(_) => false,
            }
        };
        assert!(inserted, "duplicate managed creation input binding");
        self.changed.notify_waiters();
    }

    fn queue_next(&self, id: String, inputs: CaseInputs) {
        let inserted = {
            let mut state = self.state.lock().unwrap();
            match state.next.entry(id) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(inputs);
                    true
                }
                std::collections::hash_map::Entry::Occupied(_) => false,
            }
        };
        assert!(inserted, "next managed launch already has queued inputs");
        self.changed.notify_waiters();
    }

    async fn claim(
        &self,
        lease: &Lease,
    ) -> Result<(SpawnConfig, CaseOneShot), crate::acp::acp_client::AcpError> {
        let generation = lease.admission().expect("real launch admission").generation;
        let key = (lease.session_id().to_owned(), generation);
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if lease.cancellation().is_cancelled() {
                return Err(crate::acp::acp_client::AcpError::NotRunning);
            }
            {
                let mut state = self.state.lock().unwrap();
                if let Some(mut inputs) = state.next.remove(lease.session_id()) {
                    if state
                        .bound
                        .get(&key)
                        .is_some_and(|previous| previous.first.is_some())
                    {
                        drop(state);
                        panic!("previous managed launch has not consumed its inputs");
                    }
                    let claimed = (
                        inputs.template.clone(),
                        inputs.first.take().unwrap_or_default(),
                    );
                    state.bound.insert(key, inputs);
                    return Ok(claimed);
                }
                if let Some(inputs) = state.bound.get_mut(&key) {
                    return Ok((
                        inputs.template.clone(),
                        inputs.first.take().unwrap_or_default(),
                    ));
                }
            }
            tokio::select! {
                biased;
                _ = lease.cancellation().cancelled() => {
                    return Err(crate::acp::acp_client::AcpError::NotRunning);
                }
                _ = changed => {}
            }
        }
    }
}

/// Custody stays outside the caught body, including on constructor/body panic.
pub struct ManagedScope {
    pub(crate) state: Arc<AppState>,
    states: Vec<Arc<AppState>>,
    retirements: Vec<Arc<ManagedRetirement>>,
    inputs: Arc<InputBindings>,
    leases: Arc<Mutex<Vec<Lease>>>,
    native_launches: Arc<Mutex<Vec<NativeLaunch>>>,
    options: Arc<Mutex<Option<SdkTestOptions>>>,
    runner_override: Arc<Mutex<Option<(PathBuf, RunnerInspection)>>>,
    helpers: Vec<ManagedTask>,
    bin: PathBuf,
    clients: Vec<Arc<AcpClient>>,
    creators: Vec<tokio::task::JoinHandle<axum::response::Response>>,
    directories: Arc<Mutex<Vec<Arc<tempfile::TempDir>>>>,
    environment: Vec<node::EnvGuard>,
    native: Vec<(
        u32,
        crate::process::ProcessBirthFingerprint,
        crate::process::CapturedProcess,
    )>,
    native_births: Vec<NativeSpawnProof>,
    releases: Vec<ScopedRelease>,
}

pub async fn with_managed<F>(body: F)
where
    F: for<'a> FnOnce(&'a mut ManagedScope) -> Pin<Box<dyn Future<Output = ()> + 'a>>,
{
    with_managed_retention(1000, body).await;
}

pub async fn with_managed_retention<F>(retained_events: usize, body: F)
where
    F: for<'a> FnOnce(&'a mut ManagedScope) -> Pin<Box<dyn Future<Output = ()> + 'a>>,
{
    with_managed_capacity(retained_events, 64, body).await;
}

pub(crate) async fn with_managed_capacity<F>(
    retained_events: usize,
    max_concurrent_workers: u32,
    body: F,
) where
    F: for<'a> FnOnce(&'a mut ManagedScope) -> Pin<Box<dyn Future<Output = ()> + 'a>>,
{
    let test_name = std::thread::current()
        .name()
        .expect("managed libtest name")
        .to_owned();
    if child_if_requested(&test_name).await {
        return;
    }
    let _exclusion = crate::session::test_support::EnvGuard::read_lock();
    shim_ready().expect("managed Node prerequisite; install the original test-shim dependencies");
    let node = shim_node().expect("canonical Node prerequisite").to_owned();
    let home = crate::session::test_support::isolate_app_dir();
    let app = crate::session::get_app_dir().expect("isolated app directory");
    crate::session::purge_owners::initialize(&app).expect("initial purge ownership journal");
    crate::tmux::tool_retirement::initialize(&app).expect("initial tool retirement journal");
    node::disable_systemd_scopes(home.path());
    crate::session::config::update_app_state(|state| state.has_acknowledged_agent_hooks = true)
        .expect("acknowledge fixture hooks");
    crate::server::test_support::seed_instances_on_disk_for_test("test", Vec::new());
    let bin = home.path().join("managed-bin");
    std::fs::create_dir_all(&bin).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
    let wrapper = format!(
        "#!/bin/sh\nexec {} {} \"$@\"\n",
        quote(&node),
        quote(&shim_path()),
    );
    for name in ["claude-agent-acp", "codex-acp", "pi-acp"] {
        let path = bin.join(name);
        std::fs::write(&path, &wrapper).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // Preserve each case's original executable/argv and builtin AgentProfile.
    let node_launcher = bin.join("managed-native");
    std::fs::write(&node_launcher, "#!/bin/sh\nexec \"$@\"\n").unwrap();
    std::fs::set_permissions(&node_launcher, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut paths = node::EnvGuard::new(&["PATH"]);
    let mut entries = vec![bin];
    entries.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    paths.set("PATH", std::env::join_paths(entries).unwrap());
    let inputs: Arc<InputBindings> = Arc::default();
    let leases: Arc<Mutex<Vec<Lease>>> = Arc::default();
    let options: Arc<Mutex<Option<SdkTestOptions>>> = Arc::default();
    let runner_override: Arc<Mutex<Option<(PathBuf, RunnerInspection)>>> = Arc::default();
    let directories: Arc<Mutex<Vec<Arc<tempfile::TempDir>>>> = Arc::default();
    let native_launches: Arc<Mutex<Vec<NativeLaunch>>> = Arc::default();
    let launcher: Launcher = {
        let inputs = inputs.clone();
        let leases = leases.clone();
        let directories = directories.clone();
        let native_launches = native_launches.clone();
        Arc::new(move |mut actual, session_id| {
            let lease = actual
                .launch_lease
                .as_ref()
                .expect("real launch lease")
                .clone();
            leases.lock().unwrap().push(lease.clone());
            inputs.entered.notify_waiters();
            let inputs = inputs.clone();
            let directories = directories.clone();
            let native_launches = native_launches.clone();
            let node_launcher = node_launcher.clone();
            let test_name = test_name.clone();
            Box::pin(async move {
                let (template, first) = inputs.claim(&lease).await?;
                assert_eq!(
                    actual.agent_key, template.agent_key,
                    "profile must be fixed before admission"
                );
                assert_eq!(
                    actual.tool, template.tool,
                    "tool must be fixed before admission"
                );
                assert_eq!(actual.cwd, template.cwd, "factory must retain the case cwd");
                // Only case inputs are overlaid. Attempt, producer, native store,
                // selected identity, generation and admission remain factory-owned.
                actual.spec = template.spec;
                actual
                    .spec
                    .args
                    .insert(0, std::mem::take(&mut actual.spec.command));
                actual.spec.command = node_launcher.to_string_lossy().into_owned();
                actual.additional_dirs = template.additional_dirs;
                super::super::launch::overlay_env(&mut actual.provider_env, template.provider_env);
                super::super::launch::overlay_env(
                    &mut actual.provider_routing,
                    template.provider_routing,
                );
                super::super::launch::overlay_env(
                    &mut actual.host_environment,
                    template.host_environment,
                );
                super::super::launch::overlay_env(
                    &mut actual.base_host_environment,
                    template.base_host_environment,
                );
                actual.default_effort = template.default_effort;
                actual.default_effort_explicit = template.default_effort_explicit;
                actual.default_mode = template.default_mode;
                actual.default_model = template.default_model;
                actual.mcp_servers = template.mcp_servers;
                actual.socket_path =
                    Some(crate::process::worker_registry::socket_path_for(&session_id.0).unwrap());
                let (candidate, inspection) = first.runner.unwrap_or_else(|| {
                    let directory = Arc::new(
                        tempfile::tempdir_in("/tmp").expect("unique runner/lease directory"),
                    );
                    let prepared = RunnerInspection::prepare(directory.path(), &test_name);
                    directories.lock().unwrap().push(directory);
                    prepared
                });
                native_launches
                    .lock()
                    .unwrap()
                    .push(NativeLaunch { lease, inspection });
                AcpClient::spawn_with_test_options(actual, session_id, candidate, first.options)
                    .await
            })
        })
    };
    let store = Arc::new(
        EventStore::open(&home.path().join("managed-events.db"), retained_events).unwrap(),
    );
    let state = crate::server::test_support::build_test_app_state_with_event_capacity(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        None,
        (Some(store), 8192),
        |_| {},
        |supervisor| {
            let mut supervisor = supervisor.with_launcher(launcher);
            supervisor.max_concurrent_workers = max_concurrent_workers;
            supervisor
        },
    );
    crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
    let mut scope = ManagedScope {
        state,
        inputs,
        leases,
        native_launches,
        options,
        runner_override,
        helpers: Vec::new(),
        bin: home.path().join("managed-bin"),
        states: Vec::new(),
        retirements: Vec::new(),
        clients: Vec::new(),
        creators: Vec::new(),
        directories,
        environment: Vec::new(),
        native: Vec::new(),
        native_births: Vec::new(),
        releases: Vec::new(),
    };
    let outcome = std::panic::AssertUnwindSafe(async { body(&mut scope).await })
        .catch_unwind()
        .await;
    // Never put the bracket itself in an outer timeout/select. This watchdog
    // aborts rather than restoring HOME over an unsettled child/task.
    match tokio::time::timeout(
        Duration::from_secs(90),
        std::panic::AssertUnwindSafe(scope.finish()).catch_unwind(),
    )
    .await
    {
        Ok(Ok(Ok(()))) => {}
        failure => {
            eprintln!("managed cleanup failed before HOME restoration: {failure:?}");
            eprintln!("permanent helper failures: {:?}", scope.helper_failures());
            std::process::abort();
        }
    }
    let helper_failures = scope.helper_failures();
    drop(scope);
    drop(paths);
    drop(home);
    if !helper_failures.is_empty() {
        if outcome.is_ok() {
            panic!(
                "managed helper failure after cleanup and environment restoration: {}",
                helper_failures.join("; ")
            );
        }
        eprintln!(
            "managed helper failures after cleanup and environment restoration: {}",
            helper_failures.join("; ")
        );
    }
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

impl ManagedScope {
    pub(crate) fn inspect_next_runner(
        &mut self,
        test_name: &str,
    ) -> crate::process::runner::test_support::managed_runner::RunnerInspection {
        let directory = self.tempdir();
        let (runner, inspection) =
            crate::process::runner::test_support::managed_runner::RunnerInspection::prepare(
                directory.path(),
                test_name,
            );
        assert!(self
            .runner_override
            .lock()
            .unwrap()
            .replace((runner, inspection.clone()))
            .is_none());
        inspection
    }

    pub(crate) fn configure_sdk(&mut self, options: SdkTestOptions) {
        let previous = self.options.lock().unwrap().replace(options);
        assert!(previous.is_none(), "one original's SDK options");
    }

    fn take_inputs(&mut self, template: SpawnConfig) -> CaseInputs {
        CaseInputs {
            template,
            first: Some(CaseOneShot {
                options: self.options.lock().unwrap().take().unwrap_or_default(),
                runner: self.runner_override.lock().unwrap().take(),
            }),
        }
    }

    pub(crate) fn hold_release(
        &mut self,
        release: tokio::sync::oneshot::Sender<()>,
    ) -> ScopedRelease {
        let release = ScopedRelease(Arc::new(Mutex::new(Some(release))));
        self.releases.push(release.clone());
        release
    }

    pub(crate) fn gate(
        &mut self,
    ) -> (
        crate::acp::acp_client::SdkTestGate,
        ScopedRelease,
        tokio::sync::oneshot::Receiver<()>,
    ) {
        let (entered, observed) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let release = self.hold_release(release);
        ((entered, released), release, observed)
    }

    pub(crate) fn spawn_helper(
        &mut self,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> ManagedTask {
        let task = ManagedTask(Arc::new(Mutex::new(ManagedTaskState {
            task: Some(tokio::spawn(future)),
            abort_requested: false,
            failure: None,
        })));
        self.helpers.push(task.clone());
        task
    }

    pub(crate) fn native_peer(
        &mut self,
        cwd: PathBuf,
        agent_key: &str,
        tool: &str,
        respond: impl FnMut(&serde_json::Value) -> Option<serde_json::Value> + Send + 'static,
    ) -> (SpawnConfig, NativePeer) {
        self.native_peer_inner(cwd, agent_key, tool, respond, false)
    }

    pub(crate) fn native_peer_with_stdin_close(
        &mut self,
        cwd: PathBuf,
        agent_key: &str,
        tool: &str,
        respond: impl FnMut(&serde_json::Value) -> Option<serde_json::Value> + Send + 'static,
    ) -> (SpawnConfig, NativePeer) {
        self.native_peer_inner(cwd, agent_key, tool, respond, true)
    }

    fn native_peer_inner(
        &mut self,
        cwd: PathBuf,
        agent_key: &str,
        tool: &str,
        mut respond: impl FnMut(&serde_json::Value) -> Option<serde_json::Value> + Send + 'static,
        close_stdin: bool,
    ) -> (SpawnConfig, NativePeer) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let (daemon_write, agent_read) = tokio::io::duplex(128 * 1024);
        let (mut agent_write, daemon_read) = tokio::io::duplex(128 * 1024);
        let config = self.bridge_config_as_inner(
            cwd,
            daemon_write,
            daemon_read,
            agent_key,
            tool,
            if close_stdin {
                NativeBridgeControl::Stdin
            } else {
                NativeBridgeControl::Stdout
            },
        );
        let (writer, mut writes) =
            tokio::sync::mpsc::channel::<(serde_json::Value, tokio::sync::oneshot::Sender<()>)>(1);
        self.spawn_helper(async move {
            while let Some((line, sent)) = writes.recv().await {
                let bytes = serde_json::to_vec(&line).unwrap();
                if agent_write.write_all(&bytes).await.is_err() {
                    return;
                }
                if agent_write.write_all(b"\n").await.is_err() {
                    return;
                }
                let _ = sent.send(());
            }
        });
        let requests = Arc::new(Mutex::new(Vec::new()));
        let observed = requests.clone();
        let responses = writer.clone();
        let (reply, replies) = tokio::sync::mpsc::unbounded_channel();
        let agent_info = match agent_key {
            "claude" => Some(serde_json::json!({
                "name": "@agentclientprotocol/claude-agent-acp",
                "version": crate::acp::agent_compat::CLAUDE_AGENT_ACP_MIN_VERSION,
            })),
            "codex" => Some(serde_json::json!({
                "name": "@agentclientprotocol/codex-acp",
                "version": "1.0.0",
            })),
            _ => None,
        };
        self.spawn_helper(async move {
            let mut lines = BufReader::new(agent_read).lines();
            while let Some(line) = lines.next_line().await.unwrap() {
                let line: serde_json::Value = serde_json::from_str(&line).unwrap();
                if let Some(method) = line.get("method").and_then(serde_json::Value::as_str) {
                    observed.lock().unwrap().push(method.to_owned());
                    if let Some(mut response) = respond(&line) {
                        // This is the actual protocol peer's initialize reply.
                        // Explicit identity/error fixtures remain untouched.
                        if method == "initialize" {
                            if let (Some(result), Some(info)) = (
                                response
                                    .get_mut("result")
                                    .and_then(serde_json::Value::as_object_mut),
                                agent_info.as_ref(),
                            ) {
                                result.entry("agentInfo").or_insert_with(|| info.clone());
                            }
                        }
                        let (sent, received) = tokio::sync::oneshot::channel();
                        if responses.send((response, sent)).await.is_err() {
                            return;
                        }
                        if received.await.is_err() {
                            return;
                        }
                    }
                } else {
                    let _ = reply.send(line);
                }
            }
        });
        (
            config,
            NativePeer {
                writer,
                replies,
                requests,
            },
        )
    }

    /// A real native subprocess forwards bytes to the original unit-test peer.
    /// It does not implement ACP or manufacture any establishment result.
    pub(crate) fn bridge_config(
        &mut self,
        cwd: PathBuf,
        daemon_write: tokio::io::DuplexStream,
        daemon_read: tokio::io::DuplexStream,
    ) -> SpawnConfig {
        self.bridge_config_as(cwd, daemon_write, daemon_read, "gemini", "gemini")
    }

    pub(crate) fn bridge_config_as(
        &mut self,
        cwd: PathBuf,
        daemon_write: tokio::io::DuplexStream,
        daemon_read: tokio::io::DuplexStream,
        agent_key: &str,
        tool: &str,
    ) -> SpawnConfig {
        self.bridge_config_as_inner(
            cwd,
            daemon_write,
            daemon_read,
            agent_key,
            tool,
            NativeBridgeControl::Raw,
        )
    }

    fn bridge_config_as_inner(
        &mut self,
        cwd: PathBuf,
        mut daemon_write: tokio::io::DuplexStream,
        mut daemon_read: tokio::io::DuplexStream,
        agent_key: &str,
        tool: &str,
        control: NativeBridgeControl,
    ) -> SpawnConfig {
        let directory = self.tempdir();
        let socket = directory.path().join("native.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let script = directory.path().join("bridge.mjs");
        let script_source = match control {
            NativeBridgeControl::Stdin => {
                r#"import fs from 'node:fs';
import net from 'node:net';
import readline from 'node:readline';
const socket = net.connect(process.argv[2]);
let stdinClosed = false;
process.stdin.on('error', error => {
    if (!stdinClosed) throw error;
});
process.stdin.pipe(socket);
const input = readline.createInterface({ input: socket, crlfDelay: Infinity });
input.on('line', line => {
    if (line === '{"__managed_close_stdout":true}') {
        process.stdout.end();
        return;
    }
    if (line === '{"__managed_close_stdin":true}') {
        if (stdinClosed) throw new Error('stdin already closed');
        stdinClosed = true;
        process.stdin.unpipe(socket);
        process.stdin.pause();
        fs.closeSync(0);
        process.stdout.write('{"jsonrpc":"2.0","method":"__managed/stdin_closed","params":{}}\n');
        return;
    }
    process.stdout.write(line + '\n');
});
socket.on('end', () => process.stdout.end());
process.stdout.on('finish', () => process.exit(0));
socket.on('error', error => { console.error(error); process.exit(1); });
"#
            }
            NativeBridgeControl::Stdout => {
                r#"import net from 'node:net';
import readline from 'node:readline';
const socket = net.connect(process.argv[2]);
process.stdin.pipe(socket);
const input = readline.createInterface({ input: socket, crlfDelay: Infinity });
input.on('line', line => {
    if (line === '{"__managed_close_stdout":true}') {
        process.stdout.end();
        return;
    }
    process.stdout.write(line + '\n');
});
socket.on('end', () => process.stdout.end());
process.stdout.on('finish', () => process.exit(0));
socket.on('error', error => { console.error(error); process.exit(1); });
"#
            }
            NativeBridgeControl::Raw => {
                r#"import net from 'node:net';
const socket = net.connect(process.argv[2]);
process.stdin.pipe(socket);
socket.pipe(process.stdout);
process.stdout.on('finish', () => process.exit(0));
socket.on('error', error => { console.error(error); process.exit(1); });
"#
            }
        };
        std::fs::write(&script, script_source).unwrap();
        self.spawn_helper(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut read, mut write) = stream.into_split();
            let toward_native = tokio::io::copy(&mut daemon_read, &mut write);
            let from_native = tokio::io::copy(&mut read, &mut daemon_write);
            tokio::pin!(toward_native, from_native);
            tokio::select! {
                _ = &mut toward_native => {},
                _ = &mut from_native => {},
            }
        });
        let quote = |path: &Path| format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"));
        let command = match agent_key {
            "claude" => "claude-agent-acp",
            "codex" => "codex-acp",
            "pi" => "pi-acp",
            "gemini" => "gemini",
            _ => "managed-custom-acp",
        };
        let wrapper = self.bin.join(command);
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\nexec {} {} {}\n",
                quote(shim_node().unwrap()),
                quote(&script),
                quote(&socket),
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
        if crate::agents::get_agent(agent_key).is_none() {
            crate::session::config::update_config(|settings| {
                settings
                    .session
                    .custom_agents
                    .insert(agent_key.into(), quote(&wrapper));
                settings
                    .session
                    .agent_acp_cmd
                    .insert(agent_key.into(), quote(&wrapper));
            })
            .unwrap();
        }
        let mut config = super::runner_config(PathBuf::new());
        config.agent_key = agent_key.into();
        config.tool = tool.into();
        config.spec = crate::acp::agent_registry::AgentSpec {
            command: shim_node().unwrap().to_string_lossy().into_owned(),
            args: vec![
                script.to_string_lossy().into_owned(),
                socket.to_string_lossy().into_owned(),
            ],
            description: "original unit peer over real owned native transport".into(),
            env_allowlist: None,
        };
        config.cwd = cwd;
        config.socket_path = None;
        config
    }

    pub fn tempdir(&mut self) -> Arc<tempfile::TempDir> {
        let directory = Arc::new(tempfile::tempdir_in("/tmp").expect("managed fixture directory"));
        self.directories.lock().unwrap().push(directory.clone());
        directory
    }

    /// End an independent table case, including its stopped Fork source rows.
    /// This runs only after that case has asserted all retained state.
    pub(crate) async fn finish_conversation_case(&self) {
        let ids: Vec<_> = self
            .state
            .instances
            .read()
            .await
            .iter()
            .map(|row| row.id.clone())
            .collect();
        for id in ids {
            let stopped = crate::server::api::sessions::stop_session(
                State(self.state.clone()),
                axum::extract::Path(id.clone()),
            )
            .await
            .into_response();
            let status = stopped.status();
            let body = axum::body::to_bytes(stopped.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(
                status,
                axum::http::StatusCode::OK,
                "owned case Stop {id}: {}",
                String::from_utf8_lossy(&body)
            );
            let deleted = crate::server::api::sessions::delete_session(
                State(self.state.clone()),
                axum::extract::Path(id.clone()),
                None,
            )
            .await
            .into_response();
            let status = deleted.status();
            let body = axum::body::to_bytes(deleted.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(
                status,
                axum::http::StatusCode::OK,
                "owned case permanent deletion {id}: {}",
                String::from_utf8_lossy(&body)
            );
        }
    }

    pub fn save_environment(&mut self, keys: &[&'static str]) {
        self.environment.push(node::EnvGuard::new(keys));
    }

    fn capture_native(&mut self) {
        let launches = self.native_launches.lock().unwrap().clone();
        for launch in launches {
            if self
                .native_births
                .iter()
                .any(|proof| proof.producer == launch.lease.producer_id())
            {
                continue;
            }
            if let Some(proof) = launch
                .inspection
                .native_spawn_proof(&launch.lease)
                .expect("original native birth under its actual parent custody")
            {
                self.native_births.push(proof);
            }
        }
        for proof in &self.native_births {
            let fingerprint = proof.birth.fingerprint();
            if self
                .native
                .iter()
                .any(|(pid, birth, _)| *pid == proof.pid && *birth == fingerprint)
            {
                continue;
            }
            // This observation only compares the immutable original birth.
            // It can never attest a new occupant of a historical PID.
            if crate::process::process_birth_identity(proof.pid)
                .expect("compare original native birth")
                .as_ref()
                != Some(&proof.birth)
            {
                continue;
            }
            if let Some(process) = crate::process::CapturedProcess::capture(proof.pid, &proof.birth)
                .expect("capture only the originally owned native incarnation")
            {
                self.native.push((proof.pid, fingerprint, process));
            }
        }
    }

    fn helper_failures(&self) -> Vec<String> {
        self.helpers
            .iter()
            .filter_map(|helper| helper.0.lock().unwrap().failure.clone())
            .collect()
    }

    async fn create_with_inputs(
        &mut self,
        inputs: CaseInputs,
        label: &str,
        imported: Option<String>,
        fork_source: Option<String>,
    ) -> ManagedClient {
        let (id, events) = self
            .create_observed_with_inputs(inputs, label, imported, fork_source)
            .await;
        self.installed(id, events).await
    }

    pub(crate) async fn create_observed(
        &mut self,
        config: SpawnConfig,
        label: &str,
    ) -> (String, broadcast::Receiver<AcpBroadcastFrame>) {
        self.create_source_observed(config, label, None, None).await
    }

    async fn create_source_observed(
        &mut self,
        config: SpawnConfig,
        label: &str,
        imported: Option<String>,
        fork_source: Option<String>,
    ) -> (String, broadcast::Receiver<AcpBroadcastFrame>) {
        let inputs = self.take_inputs(config);
        self.create_observed_with_inputs(inputs, label, imported, fork_source)
            .await
    }

    async fn create_observed_with_inputs(
        &mut self,
        inputs: CaseInputs,
        label: &str,
        imported: Option<String>,
        fork_source: Option<String>,
    ) -> (String, broadcast::Receiver<AcpBroadcastFrame>) {
        let (id, generation, events, inputs) = self
            .create_unbound(inputs, label, imported, fork_source)
            .await;
        self.inputs.bind(id.clone(), generation, inputs);
        (id, events)
    }

    async fn create_unbound(
        &mut self,
        inputs: CaseInputs,
        label: &str,
        imported: Option<String>,
        fork_source: Option<String>,
    ) -> (
        String,
        u64,
        broadcast::Receiver<AcpBroadcastFrame>,
        CaseInputs,
    ) {
        let config = &inputs.template;
        assert!(
            config.launch_lease.is_none(),
            "case inputs must not invent a lease"
        );
        assert!(
            config.fork_from.is_none(),
            "fork requires its own production selection route"
        );
        let events = self.state.acp_events_tx.subscribe();
        let agent = config.agent_key.clone();
        let tool = config.tool.clone();
        let path = config.cwd.to_string_lossy().into_owned();
        let title = format!("managed-{label}-{}", uuid::Uuid::new_v4());
        let body: crate::daemon::CreateSessionBody = serde_json::from_value(serde_json::json!({
            "title": title, "path": path, "tool": tool, "profile": "test", "view": "structured",
            "agent_name": if imported.is_none() && fork_source.is_none() { Some(agent.as_str()) } else { None },
            "import_acp_session_id": imported,
            "fork_session_id": fork_source,
        }))
        .unwrap();
        let state = self.state.clone();
        self.creators.push(tokio::spawn(async move {
            crate::server::api::sessions::create_session(
                State(state),
                Query(crate::server::api::sessions::CreateSessionQuery { wait: None }),
                None,
                Ok(Json(body)),
            )
            .await
            .into_response()
        }));
        let response =
            tokio::time::timeout(Duration::from_secs(30), self.creators.last_mut().unwrap())
                .await
                .expect("creation driver deadline; retained for cleanup")
                .expect("creation driver");
        self.creators.pop();
        let status = response.status();
        if status != axum::http::StatusCode::CREATED {
            let headers = response.headers().clone();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            panic!(
                "managed factory refused {status}; agent={agent}, tool={tool}, import={imported:?}, fork={fork_source:?}, headers={headers:?}, body={}",
                String::from_utf8_lossy(&body)
            );
        }
        let (id, generation) = {
            let rows = self.state.instances.read().await;
            let row = rows
                .iter()
                .find(|row| row.title == title)
                .expect("factory-created native row");
            (row.id.clone(), row.lifecycle_generation)
        };
        (id, generation, events, inputs)
    }

    async fn installed(
        &mut self,
        id: String,
        events: broadcast::Receiver<AcpBroadcastFrame>,
    ) -> ManagedClient {
        self.installed_on(self.state.clone(), id, events).await
    }

    async fn installed_on(
        &mut self,
        state: Arc<AppState>,
        id: String,
        events: broadcast::Receiver<AcpBroadcastFrame>,
    ) -> ManagedClient {
        let supervisor = state.acp_supervisor.clone();
        let (client, lease, drain) = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let changed = supervisor.worker_notify.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                let installed = {
                    let workers = supervisor.workers.lock().await;
                    workers.get(&id).map(|handle| {
                        (
                            handle.client.clone(),
                            handle.lease.clone(),
                            handle.drain_task.abort_handle(),
                        )
                    })
                };
                if let Some(installed) = installed {
                    break installed;
                }
                changed.await;
            }
        })
        .await
        .expect("actual worker installation");
        self.clients.push(client.clone());
        self.capture_native();
        let retirement = Arc::new(ManagedRetirement::capture(state.clone(), lease.clone()));
        assert_eq!(retirement.target.identity, client.connected_runner());
        self.retirements.push(retirement.clone());
        let mut leases = self.leases.lock().unwrap();
        if !leases.contains(&lease) {
            leases.push(lease.clone());
        }
        if !Arc::ptr_eq(&self.state, &state) {
            let previous = std::mem::replace(&mut self.state, state.clone());
            if !self
                .states
                .iter()
                .any(|known| Arc::ptr_eq(known, &previous))
            {
                self.states.push(previous);
            }
        }
        ManagedClient {
            client,
            supervisor: supervisor.clone(),
            id,
            lease,
            drain,
            events,
            last_seq: None,
            retirement,
            state,
        }
    }

    pub(crate) async fn adopt(
        &mut self,
        original: ManagedClient,
        in_flight_turn: bool,
    ) -> ManagedClient {
        self.adopt_with(original, in_flight_turn, || std::future::ready(()))
            .await
    }

    pub(crate) async fn adopt_with<F: Future<Output = ()>>(
        &mut self,
        original: ManagedClient,
        in_flight_turn: bool,
        between: impl FnOnce() -> F,
    ) -> ManagedClient {
        let options = self.options.lock().unwrap().take().unwrap_or_default();
        original.ready().await;
        let row = crate::session::Storage::open_unwatched("test")
            .unwrap()
            .load_instances_complete()
            .unwrap()
            .into_iter()
            .find(|row| row.id == original.id)
            .expect("retained native row");
        original.supervisor.detach_all().await;
        between().await;
        self.attach_retained_parent(original, row, options, in_flight_turn)
            .await
    }

    pub(crate) async fn attach_retained_parent(
        &mut self,
        original: ManagedClient,
        row: crate::session::Instance,
        options: SdkTestOptions,
        in_flight_turn: bool,
    ) -> ManagedClient {
        let prior = crate::session::Storage::open_unwatched("test")
            .unwrap()
            .load_instances_complete()
            .unwrap();
        let launcher = original.supervisor.launcher.clone();
        let state = crate::server::test_support::build_test_app_state_with_event_capacity(
            prior,
            Vec::new(),
            Vec::new(),
            None,
            (Some(self.state.acp_event_store.clone()), 8192),
            |_| {},
            |supervisor| {
                Supervisor::with_capacity(supervisor.sink.clone(), 2).with_launcher(launcher)
            },
        );
        self.states.push(state.clone());
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let supervisor = state.acp_supervisor.clone();
        let events = state.acp_events_tx.subscribe();
        AcpClient::with_sdk_test_options(
            options,
            supervisor.attach(
                row.id.clone(),
                row.project_path.into(),
                Vec::new(),
                in_flight_turn,
                None,
            ),
        )
        .await
        .expect("actual retained owner attachment");
        let adopted = self.installed_on(state, row.id, events).await;
        // Only a successful actual attachment transfers controller custody.
        original
            .retirement
            .active
            .store(false, std::sync::atomic::Ordering::Release);
        adopted
    }

    pub(crate) async fn adopt_pending(
        &mut self,
        original: ManagedClient,
        inspection: &RunnerInspection,
        issued: IssuedOriginal,
        in_flight_turn: bool,
    ) -> ManagedClient {
        use crate::acp::establishment::{AcpEstablishment, NativeRequestScope};
        let options = self.options.lock().unwrap().take().unwrap_or_default();
        let expected = &issued.receipt.attempt;
        let storage = crate::session::Storage::open_unwatched("test").unwrap();
        let row = storage
            .load_instances_complete()
            .unwrap()
            .into_iter()
            .find(|row| row.id == original.id)
            .expect("actual pending native row");
        assert!(matches!(&row.acp_establishment,
        Some(AcpEstablishment::Pending { attempt })
            if attempt.same_source(expected) && attempt.destination == expected.destination
                && attempt.matches_pending(&row, &storage).unwrap()));
        assert_eq!(expected.session_id, row.id);
        assert_eq!(expected.producer, Some(original.lease.producer_id()));
        let owner = original
            .lease
            .spawned_runner()
            .expect("original launched owner");
        assert!(owner.birth_fingerprint.is_some());
        assert_eq!(issued.receipt.owner, owner);
        assert_eq!(original.actual().connected_runner(), Some(owner));
        assert!(
            matches!(&issued.scope, NativeRequestScope::Original { attempt, owner: actual, store, .. }
        if attempt.as_ref() == expected.as_ref() && *actual == owner
            && issued.receipt.same_store(store.as_deref()))
        );
        let verify_owner = || {
            let record = crate::process::worker_registry::load_strict(&row.id)
                .unwrap()
                .expect("original registry custody still exists");
            assert_eq!(
                crate::acp::runner_lifecycle::RunnerIdentity::from_record(&record),
                owner
            );
            let birth = crate::process::process_birth_identity(owner.pid)
                .unwrap()
                .expect("original physical runner is still live");
            assert_eq!(Some(birth.fingerprint()), owner.birth_fingerprint);
        };
        verify_owner();
        let before: IssuedOriginal =
            serde_json::from_value(inspection.query("pending-original").await).unwrap();
        assert_eq!(before.request_id, issued.request_id);
        assert_eq!(before.scope, issued.scope);
        assert_eq!(before.receipt.as_ref(), issued.receipt.as_ref());
        original.supervisor.detach_all().await;
        verify_owner();
        let after: IssuedOriginal =
            serde_json::from_value(inspection.query("pending-original").await).unwrap();
        assert_eq!(after.request_id, issued.request_id);
        assert_eq!(after.scope, issued.scope);
        assert_eq!(after.receipt.as_ref(), issued.receipt.as_ref());
        let adopted = self
            .attach_retained_parent(original, row, options, in_flight_turn)
            .await;
        assert_eq!(adopted.actual().connected_runner(), Some(owner));
        let attached: IssuedOriginal =
            serde_json::from_value(inspection.query("pending-original").await).unwrap();
        assert_eq!(attached.request_id, issued.request_id);
        assert_eq!(attached.scope, issued.scope);
        assert_eq!(attached.receipt.as_ref(), issued.receipt.as_ref());
        adopted
    }

    pub async fn spawn(
        &mut self,
        mut config: SpawnConfig,
        label: impl AsRef<str>,
    ) -> Result<ManagedClient, SupervisorError> {
        let label = label.as_ref();
        if let Some(pin) = config.claude_store_pin.as_ref() {
            self.save_environment(&["CLAUDE_CONFIG_DIR"]);
            std::env::set_var("CLAUDE_CONFIG_DIR", &pin.store);
        }
        let fork = config.fork_from.take().is_some();
        let load = config.stored_acp_session_id.take().is_some();
        let mut measured = self.take_inputs(config);
        if !load && !fork {
            return Ok(self.create_with_inputs(measured, label, None, None).await);
        }
        // Establish a genuinely absent original first. The literal old test ID
        // only selects the Load scenario; it never becomes native authority.
        let mut prime = measured.template.clone();
        prime.default_model = None;
        prime.default_effort = None;
        prime.default_effort_explicit = false;
        prime.default_mode = None;
        let prime = CaseInputs {
            template: prime,
            first: Some(CaseOneShot::default()),
        };
        let original = self
            .create_with_inputs(prime, &format!("{label}-prime"), None, None)
            .await;
        original.ready().await;
        if load && !fork && measured.template.seed_history_replay {
            let selected = original.selected_native_session();
            original.stop_and_forget().await;
            if !measured
                .template
                .provider_env
                .iter()
                .any(|(key, _)| key == "SHIM_LOAD_SESSION")
            {
                // Trusted import discovery uses the builtin wrapper, not the
                // later scripted transport. Its actual peer must support Load.
                measured
                    .template
                    .provider_env
                    .push(("SHIM_LOAD_SESSION".into(), "1".into()));
            }
            measured.template.provider_env.push((
                "SHIM_LIST_SESSIONS".into(),
                serde_json::json!([{
                    "sessionId": selected,
                    "cwd": measured.template.cwd,
                    "title": "actual original selected for import",
                }])
                .to_string(),
            ));
            return Ok(self.import_with_inputs(measured, label, selected).await);
        }
        Ok(self
            .relaunch_with_inputs(original, measured, label, fork)
            .await)
    }

    pub(crate) async fn retained_after(
        &mut self,
        original: ManagedClient,
        config: SpawnConfig,
        label: &str,
    ) -> ManagedClient {
        self.relaunch_after(original, config, label, false).await
    }

    pub(crate) async fn fork_after(
        &mut self,
        original: ManagedClient,
        config: SpawnConfig,
        label: &str,
    ) -> ManagedClient {
        self.relaunch_after(original, config, label, true).await
    }

    pub(crate) async fn import_after(
        &mut self,
        original: ManagedClient,
        config: SpawnConfig,
        label: &str,
    ) -> ManagedClient {
        let selected = original.selected_native_session();
        original.stop_and_forget().await;
        let inputs = self.take_inputs(config);
        self.import_with_inputs(inputs, label, selected).await
    }

    async fn relaunch_after(
        &mut self,
        original: ManagedClient,
        config: SpawnConfig,
        label: &str,
        fork: bool,
    ) -> ManagedClient {
        let inputs = self.take_inputs(config);
        self.relaunch_with_inputs(original, inputs, label, fork)
            .await
    }

    async fn relaunch_with_inputs(
        &mut self,
        original: ManagedClient,
        mut inputs: CaseInputs,
        label: &str,
        fork: bool,
    ) -> ManagedClient {
        let config = &mut inputs.template;
        original.ready().await;
        let storage = crate::session::Storage::open_unwatched("test").unwrap();
        let row = storage
            .load_instances_complete()
            .unwrap()
            .into_iter()
            .find(|row| row.id == original.id)
            .unwrap();
        // Physical shutdown alone leaves the original C checkpoint owed.
        // Use the actual Stop owner before requesting another native invocation.
        original.stop_session().await;
        config.stored_acp_session_id = None;
        config.fork_from = None;
        for (key, path) in &config.provider_env {
            if key.ends_with("_RECORD_FILE") {
                match std::fs::remove_file(path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => panic!("rotate priming record: {error}"),
                }
            }
        }
        if fork {
            return self
                .create_with_inputs(inputs, label, None, Some(row.id))
                .await;
        }
        self.resume_stopped_with_inputs(row.id, inputs).await
    }

    pub(crate) async fn resume_stopped(&mut self, id: &str, config: SpawnConfig) -> ManagedClient {
        let inputs = self.take_inputs(config);
        self.resume_stopped_with_inputs(id.to_owned(), inputs).await
    }

    async fn resume_stopped_with_inputs(
        &mut self,
        id: String,
        inputs: CaseInputs,
    ) -> ManagedClient {
        let storage = crate::session::Storage::open_unwatched("test").unwrap();
        let config = &inputs.template;
        let events = self.state.acp_events_tx.subscribe();
        // Hold source exclusions through the stopped-resume reservation.
        let namespace = self.state.profile_namespace.read().await;
        let submission = self.state.session_service.prompt_submission(&id).await;
        let lock = self.state.instance_lock(&id).await;
        let guard = lock.lock().await;
        let stopped = storage
            .load_instances_complete()
            .unwrap()
            .into_iter()
            .find(|current| current.id == id)
            .expect("stopped original remains durable");
        let sid = stopped
            .acp_session_id
            .clone()
            .expect("original returned native SID");
        assert_eq!(stopped.acp_session_id.as_deref(), Some(sid.as_str()));
        let (row, reservation) = crate::server::api::sessions::resume_structured_stop(
            &self.state,
            stopped,
            Some(crate::acp::supervisor::ResumeKind::Spawn),
        )
        .await
        .expect("real explicit resume of the settled original Stop");
        let reservation =
            reservation.expect("stopped original needs its actual resume reservation");
        assert_eq!(row.acp_session_id.as_deref(), Some(sid.as_str()));
        let state = self.state.clone();
        let (row, native_store, native_scope) = tokio::task::spawn_blocking(move || {
            use crate::session::SessionStore as _;
            let native_store = Arc::new(
                crate::server::session_store::NativeSessionStore::open(state, "test", None)
                    .expect("current native launch store"),
            );
            let native_scope =
                crate::acp::state::NativeObservationScope::capture(&row, native_store.storage())
                    .expect("actual resumed row scope");
            (row, native_store, native_scope)
        })
        .await
        .expect("capture actual resumed native launch admission");
        drop(guard);
        drop(submission);
        drop(namespace);
        let mut request = spawn_request(&row.id);
        request.agent = config.agent_key.clone();
        request.tool = config.tool.clone();
        request.cwd = config.cwd.clone();
        request.source_profile = Some("test".into());
        request.expected_lifecycle_generation = row.lifecycle_generation;
        request.stored_acp_session_id = Some(sid);
        request.prior_conversation = true;
        request.seed_history_replay = config.seed_history_replay;
        request.launch_admission = Some(crate::acp::supervisor::LaunchAdmission {
            establishment: reservation.establishment.clone(),
            store: native_store,
            generation: row.lifecycle_generation,
            namespace: None,
            native_scope: Some(native_scope),
        });
        self.inputs
            .bind(row.id.clone(), row.lifecycle_generation, inputs);
        self.state
            .acp_supervisor
            // Explicit retained Load keeps the controller's genuine reservation.
            .spawn_inner(request, reservation)
            .await
            .expect("genuine retained Load launch");
        self.installed(row.id, events).await
    }

    pub async fn import_listed(
        &mut self,
        config: SpawnConfig,
        label: &str,
        sid: String,
    ) -> Result<ManagedClient, SupervisorError> {
        let inputs = self.take_inputs(config);
        Ok(self.import_with_inputs(inputs, label, sid).await)
    }

    async fn import_with_inputs(
        &mut self,
        inputs: CaseInputs,
        label: &str,
        sid: String,
    ) -> ManagedClient {
        // Listing and production import validation both use the actual builtin
        // wrapper and the same trusted SHIM environment, before Load admission.
        let pairs = inputs
            .template
            .provider_env
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>();
        crate::session::config::update_config(|settings| settings.environment = pairs).unwrap();
        self.create_with_inputs(inputs, label, Some(sid), None)
            .await
    }

    async fn finish(&mut self) -> Result<(), String> {
        use std::sync::atomic::Ordering;
        for release in self.releases.drain(..) {
            release.release();
        }
        let states = std::iter::once(self.state.clone())
            .chain(self.states.iter().cloned())
            .collect::<Vec<_>>();
        for creator in self.creators.drain(..) {
            let _ = creator.await;
        }
        // Capture the actual constructors before cancellation can remove their entries.
        for state in &states {
            let ids = state
                .instances
                .read()
                .await
                .iter()
                .map(|row| row.id.clone())
                .collect::<Vec<_>>();
            let captured = {
                let lifecycle = state
                    .acp_supervisor
                    .lifecycle
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                ids.iter()
                    .filter_map(|id| lifecycle.current_lease(id))
                    .collect::<Vec<_>>()
            };
            for lease in captured {
                if !self
                    .retirements
                    .iter()
                    .any(|retirement| retirement.target.lease.as_ref() == Some(&lease))
                {
                    self.retirements.push(Arc::new(ManagedRetirement::capture(
                        state.clone(),
                        lease.clone(),
                    )));
                }
                let mut leases = self.leases.lock().unwrap();
                if !leases.contains(&lease) {
                    leases.push(lease);
                }
            }
            for id in ids {
                state.session_service.cancel_creation(&id);
            }
        }
        let leases = self.leases.lock().unwrap().clone();
        for lease in &leases {
            if !self
                .retirements
                .iter()
                .any(|retirement| retirement.target.lease.as_ref() == Some(lease))
            {
                self.retirements.push(Arc::new(ManagedRetirement::capture(
                    self.state.clone(),
                    lease.clone(),
                )));
            }
        }
        let control = SystemProcessControl::default();
        let mut errors = Vec::new();
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.capture_native())).is_err()
        {
            errors.push("initial original native proof capture failed".to_owned());
        }
        let mut pending = Vec::new();
        for retirement in self.retirements.iter().rev() {
            if retirement.active.load(Ordering::Acquire)
                && !retirement.completed.load(Ordering::Acquire)
            {
                let lease = retirement
                    .target
                    .lease
                    .as_ref()
                    .expect("captured original lease");
                let owner = lease.spawned_runner();
                let mut original_live = owner.is_some_and(|owner| control.is_alive(owner));
                for proof in self.native_births.iter().filter(|proof| {
                    Some(proof.runner) == owner && proof.session_id == lease.session_id()
                }) {
                    match crate::process::process_birth_identity(proof.pid) {
                        Ok(current) => original_live |= current.as_ref() == Some(&proof.birth),
                        Err(error) => {
                            errors.push(format!("original native birth comparison failed: {error}"))
                        }
                    }
                }
                let constructor_pending = !lease.original_constructor_completed();
                if let Err(error) = retirement.retire().await {
                    pending.push((
                        retirement.clone(),
                        error.to_string(),
                        original_live,
                        constructor_pending,
                    ));
                }
            }
        }
        for state in &states {
            if tokio::time::timeout(Duration::from_secs(5), state.session_service.work.drain())
                .await
                .is_err()
            {
                errors.push(
                    "owned constructor work did not settle before physical cleanup".to_owned(),
                );
            }
        }
        // A native birth can be published after the initial creation response.
        // Re-read only each invocation's immutable original-parent proof.
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.capture_native())).is_err()
        {
            errors.push("original native proof capture failed".to_owned());
        }
        let launches = self.native_launches.lock().unwrap().clone();
        for launch in &launches {
            match launch.inspection.native_spawn_proof(&launch.lease) {
                Ok(Some(_)) if launch.lease.was_cancelled_before_birth() => {
                    errors.push(format!(
                        "{}: native birth contradicts original birth exclusion",
                        launch.lease.session_id()
                    ));
                }
                Ok(Some(_)) => {}
                Ok(None) if launch.lease.was_cancelled_before_birth() => {}
                Ok(None) => {
                    errors.push(format!(
                        "{}: original native birth or no-birth proof remains unknown",
                        launch.lease.session_id()
                    ));
                }
                Err(error) => {
                    errors.push(format!(
                        "{}: original native proof read failed; custody remains unknown: {error:#}",
                        launch.lease.session_id()
                    ));
                }
            }
        }
        for (_, _, process) in &self.native {
            if let Err(error) = process.signal(libc::SIGKILL) {
                errors.push(format!("original native signal failed: {error}"));
            }
        }
        for proof in &self.native_births {
            if std::panic::AssertUnwindSafe(super::wait_original_reaped(
                proof.pid,
                proof.birth.fingerprint(),
            ))
            .catch_unwind()
            .await
            .is_err()
            {
                errors.push(format!("original native {} did not settle", proof.pid));
            }
        }
        // Keep the original native parent available to reap its child first.
        let mut reaped_parents = Vec::new();
        for lease in &leases {
            if let Some(owner) = lease.spawned_runner() {
                let mut native_known = false;
                let mut native_closed = true;
                for launch in launches
                    .iter()
                    .filter(|launch| launch.lease.spawned_runner() == Some(owner))
                {
                    native_known = true;
                    match launch.inspection.native_spawn_proof(&launch.lease) {
                        Ok(Some(proof)) => {
                            match crate::process::process_birth_identity(proof.pid) {
                                Ok(current) => {
                                    native_closed &= current.as_ref() != Some(&proof.birth)
                                }
                                Err(error) => {
                                    native_closed = false;
                                    errors.push(format!(
                                        "original native closure comparison failed: {error}"
                                    ));
                                }
                            }
                        }
                        Ok(None) => native_closed = false,
                        Err(error) => {
                            native_closed = false;
                            errors.push(format!("original native proof read failed; original parent retained: {error:#}"));
                        }
                    }
                }
                if !native_known || !native_closed {
                    errors.push(format!(
                        "{}: native settlement is unproved; original parent retained",
                        lease.session_id()
                    ));
                    continue;
                }
                if control.is_alive(owner) {
                    control.kill_group(owner);
                }
                if let Some(birth) = owner.birth_fingerprint {
                    if std::panic::AssertUnwindSafe(super::wait_original_reaped(owner.pid, birth))
                        .catch_unwind()
                        .await
                        .is_err()
                    {
                        errors.push(format!("original runner {} did not settle", owner.pid));
                    } else {
                        reaped_parents.push(owner);
                    }
                } else {
                    errors.push(format!(
                        "{}: original runner birth remains unattested",
                        lease.session_id()
                    ));
                }
            }
        }
        let mut closed_owners = Vec::new();
        for launch in &launches {
            if launch.lease.spawned_runner().is_none() && launch.lease.was_cancelled_before_birth()
            {
                continue;
            }
            let mut completion = launch.lease.test_child_wait_completed();
            let settled = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match *completion.borrow_and_update() {
                        Some(true) => return true,
                        Some(false) => return false,
                        None => {}
                    }
                    if completion.changed().await.is_err() {
                        return false;
                    }
                }
            })
            .await;
            if !matches!(settled, Ok(true)) {
                errors.push(format!(
                    "{}: original Child owner did not confirm its wait",
                    launch.lease.session_id()
                ));
            } else if let Some(owner) = launch.lease.spawned_runner() {
                if reaped_parents.contains(&owner) {
                    closed_owners.push(owner);
                }
            }
        }
        for (retirement, original_error, original_live, constructor_pending) in pending {
            let lease = retirement
                .target
                .lease
                .as_ref()
                .expect("captured original lease");
            let original_closed = match lease.spawned_runner() {
                Some(owner) => closed_owners.contains(&owner),
                None => {
                    lease.was_cancelled_before_birth() && lease.original_constructor_completed()
                }
            };
            let constructor_progress =
                constructor_pending && lease.original_constructor_completed();
            if original_closed && (original_live || constructor_progress) {
                // Continue the same captured checkpoint only after new physical facts.
                if let Err(error) = retirement.retire().await {
                    errors.push(format!("captured retirement failed: {original_error}; after physical settlement: {error}"));
                }
            } else {
                errors.push(format!(
                    "captured retirement remains unproved: {original_error}"
                ));
            }
        }
        for state in &states {
            state.shutdown.cancel();
            if tokio::time::timeout(Duration::from_secs(5), state.session_service.work.drain())
                .await
                .is_err()
            {
                errors.push("owned service work remained after shutdown".to_owned());
            }
        }
        for task in &self.helpers {
            task.abort();
        }
        for task in &self.helpers {
            let _ = task.clone().await;
        }
        if !errors.is_empty() {
            return Err(errors.join("; "));
        }
        // All actual controller ACKs, original processes and producer tasks settled.
        self.native_launches.lock().unwrap().clear();
        self.leases.lock().unwrap().clear();
        self.runner_override.lock().unwrap().take();
        self.options.lock().unwrap().take();
        *self.inputs.state.lock().unwrap() = InputState::default();
        self.retirements.clear();
        self.directories.lock().unwrap().clear();
        Ok(())
    }
    pub(crate) fn bind_external_launch(
        &mut self,
        id: &str,
        config: SpawnConfig,
    ) -> broadcast::Receiver<AcpBroadcastFrame> {
        assert!(
            crate::session::Storage::open_unwatched("test")
                .unwrap()
                .load_instances_complete()
                .unwrap()
                .iter()
                .any(|row| row.id == id),
            "existing selected Native row"
        );
        let inputs = self.take_inputs(config);
        let events = self.state.acp_events_tx.subscribe();
        self.inputs.queue_next(id.to_owned(), inputs);
        events
    }

    pub(crate) async fn prepare_external_resume(
        &mut self,
        original: ManagedClient,
        config: SpawnConfig,
    ) -> (String, broadcast::Receiver<AcpBroadcastFrame>) {
        let id = original.id.clone();
        let stopped = crate::server::api::sessions::stop_session(
            State(original.state.clone()),
            axum::extract::Path(id.clone()),
        )
        .await
        .into_response();
        assert_eq!(stopped.status(), axum::http::StatusCode::OK);
        original
            .shutdown()
            .await
            .expect("actual original retirement is settled before fixture custody ends");
        let row = crate::session::Storage::open_unwatched("test")
            .unwrap()
            .load_instances_complete()
            .unwrap()
            .into_iter()
            .find(|row| row.id == id)
            .unwrap();
        assert_eq!(row.status, crate::session::Status::Stopped);
        let events = self.bind_external_launch(&id, config);
        (id, events)
    }

    pub(crate) async fn observe_external_resume(
        &mut self,
        id: String,
        events: broadcast::Receiver<AcpBroadcastFrame>,
    ) -> ManagedClient {
        self.installed(id, events).await
    }

    pub(crate) async fn reconcile_after<F: Future<Output = ()>>(
        &mut self,
        original: ManagedClient,
        between: impl FnOnce(Arc<AppState>, String) -> F,
    ) -> ManagedClient {
        let original_receipt = original
            .actual()
            .establishment_projection()
            .unwrap()
            .receipt_snapshot()
            .unwrap();
        let id = original.id.clone();
        let prior = crate::session::Storage::open_unwatched("test")
            .unwrap()
            .load_instances_complete()
            .unwrap();
        let launcher = original.supervisor.launcher.clone();
        let state = crate::server::test_support::build_test_app_state_with_event_capacity(
            prior,
            Vec::new(),
            Vec::new(),
            None,
            (Some(original.state.acp_event_store.clone()), 8192),
            |_| {},
            |supervisor| {
                Supervisor::with_capacity(supervisor.sink.clone(), 2).with_launcher(launcher)
            },
        );
        self.states.push(state.clone());
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let events = state.acp_events_tx.subscribe();
        original.supervisor.detach_all().await;
        between(state.clone(), id.clone()).await;
        let attached = self.installed_on(state, id, events).await;
        attached.ready().await;
        let attached_receipt = attached
            .actual()
            .establishment_projection()
            .unwrap()
            .receipt_snapshot()
            .unwrap();
        assert_eq!(attached_receipt.owner, original_receipt.owner);
        assert_eq!(attached_receipt.attempt, original_receipt.attempt);
        original
            .retirement
            .active
            .store(false, std::sync::atomic::Ordering::Release);
        attached
    }
}

#[cfg(test)]
mod safety_tests {
    use super::*;
    use std::io::{Read, Write};
    use std::process::{Child, Command, Stdio};

    struct Sentinel {
        child: Child,
        birth: crate::process::ProcessBirthIdentity,
    }

    impl Sentinel {
        fn different_birth(original: &crate::process::ProcessBirthIdentity) -> Self {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                // Owned and unreaped here; no discovery of a historical PID.
                let child = Command::new("cat")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap();
                let birth = crate::process::process_birth_identity(child.id())
                    .unwrap()
                    .unwrap();
                let sentinel = Self { child, birth };
                if &sentinel.birth != original {
                    return sentinel;
                }
                // Platforms may assign the same coarse start timestamp. Wait
                // through controlled children, never through random PID reuse.
                drop(sentinel);
                assert!(
                    std::time::Instant::now() < deadline,
                    "distinct controlled birth must become observable"
                );
            }
        }

        fn assert_alive(&mut self) {
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "foreign sentinel was signalled"
            );
            assert_eq!(
                crate::process::process_birth_identity(self.child.id())
                    .unwrap()
                    .as_ref(),
                Some(&self.birth)
            );
            self.child
                .stdin
                .as_mut()
                .unwrap()
                .write_all(b"still-owned-by-test\n")
                .unwrap();
            let mut reply = [0_u8; 20];
            self.child
                .stdout
                .as_mut()
                .unwrap()
                .read_exact(&mut reply)
                .unwrap();
            assert_eq!(&reply, b"still-owned-by-test\n");
        }
    }

    impl Drop for Sentinel {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    struct Evidence {
        owner: crate::acp::runner_lifecycle::RunnerIdentity,
        native: NativeSpawnProof,
        directories: Vec<PathBuf>,
    }

    fn evidence(scope: &ManagedScope, client: &ManagedClient) -> Evidence {
        let native = scope
            .native_births
            .iter()
            .find(|proof| proof.producer == client.lease.producer_id())
            .unwrap()
            .clone();
        let owner = client.lease.spawned_runner().unwrap();
        assert_eq!(native.runner, owner);
        assert_ne!(owner.pid, std::process::id());
        let mut directories = scope
            .directories
            .lock()
            .unwrap()
            .iter()
            .map(|directory| directory.path().to_owned())
            .collect::<Vec<_>>();
        directories.push(scope.bin.parent().unwrap().to_owned());
        Evidence {
            owner,
            native,
            directories,
        }
    }

    fn assert_retired(evidence: &Evidence) {
        assert!(
            !SystemProcessControl::default().is_alive(evidence.owner),
            "original runner survived cleanup"
        );
        assert_ne!(
            crate::process::process_birth_identity(evidence.native.pid)
                .unwrap()
                .as_ref(),
            Some(&evidence.native.birth),
            "original native incarnation survived cleanup"
        );
        for directory in &evidence.directories {
            assert!(
                !directory.exists(),
                "fixture directory survived restored environment: {}",
                directory.display()
            );
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn concurrent_case_inputs_bind_exact_creation_and_cancel_only_their_waiter() {
        const NAME: &str = "acp::supervisor::test_support::managed::safety_tests::concurrent_case_inputs_bind_exact_creation_and_cancel_only_their_waiter";
        if child_if_requested(NAME).await {
            return;
        }
        struct HeldCase {
            inputs: CaseInputs,
            peer: NativePeer,
            native_new: tokio::sync::oneshot::Receiver<(serde_json::Value, serde_json::Value)>,
            sdk_seen: tokio::sync::oneshot::Receiver<()>,
            gate_seen: tokio::sync::oneshot::Receiver<()>,
            release: ScopedRelease,
            inspection: RunnerInspection,
        }
        fn held_case(scope: &mut ManagedScope, cwd: PathBuf, agent: &str) -> HeldCase {
            let inspection = scope.inspect_next_runner(NAME);
            let (gate, release, gate_seen) = scope.gate();
            let (sdk_seen, observed_sdk) = tokio::sync::oneshot::channel();
            scope.configure_sdk(SdkTestOptions {
                fresh_response_application: Some(gate),
                native_session_seen: Some(sdk_seen),
                ..Default::default()
            });
            let (native_new, observed_new) = tokio::sync::oneshot::channel();
            let mut native_new = Some(native_new);
            let (config, peer) = scope.native_peer(cwd, agent, agent, move |request| {
                let id = request.get("id")?;
                match request["method"].as_str().unwrap() {
                    "initialize" => Some(serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":1,"agentCapabilities":{}}})),
                    "session/new" => {
                        native_new.take().expect("one genuine native New")
                            .send((id.clone(), request["params"]["cwd"].clone())).unwrap();
                        None
                    }
                    other => panic!("unexpected native input in creation fixture: {other}"),
                }
            });
            HeldCase {
                inputs: scope.take_inputs(config),
                peer,
                native_new: observed_new,
                sdk_seen: observed_sdk,
                gate_seen,
                release,
                inspection,
            }
        }
        async fn entered_lease(scope: &ManagedScope, id: &str) -> Lease {
            loop {
                let entered = scope.inputs.entered.notified();
                tokio::pin!(entered);
                entered.as_mut().enable();
                if let Some(lease) = scope
                    .leases
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|lease| lease.session_id() == id)
                    .cloned()
                {
                    return lease;
                }
                entered.await;
            }
        }
        async fn modes(client: &mut ManagedClient, expected: &str) {
            loop {
                if let Event::ModesAvailable {
                    current_mode_id,
                    modes,
                } = client.recv().await.unwrap()
                {
                    assert_eq!(current_mode_id, expected);
                    assert_eq!(modes.len(), 1);
                    assert_eq!(modes[0].id, expected);
                    return;
                }
            }
        }
        let observed = Arc::new(Mutex::new(Vec::new()));
        let retained = observed.clone();
        with_managed(move |scope| Box::pin(async move {
            for (round_index, same_cwd) in [false, true].into_iter().enumerate() {
                let first_cwd = scope.tempdir();
                let second_cwd = if same_cwd { first_cwd.clone() } else { scope.tempdir() };
                let mut first = held_case(scope, first_cwd.path().into(), "gemini");
                let (first_id, first_generation, first_events, first_inputs) = scope.create_unbound(first.inputs, "inputs-first", None, None).await;
                let mut second = held_case(scope, second_cwd.path().into(), "codex");
                let (second_id, second_generation, second_events, second_inputs) = scope.create_unbound(second.inputs, "inputs-second", None, None).await;
                assert_ne!(first_id, second_id);
                const PHASES: &[&str] = &[
                    "first_lease",
                    "second_lease",
                    "second_native_new",
                    "first_native_new",
                    "second_response_written",
                    "second_native_seen",
                    "second_application_gate",
                    "second_installed",
                    "second_ready",
                    "second_modes",
                    "first_response_written",
                    "first_native_seen",
                    "first_application_gate",
                    "first_installed",
                    "first_ready",
                    "first_modes",
                    "first_retired",
                    "second_retired",
                ];
                let phase = std::sync::atomic::AtomicUsize::new(0);
                let mut native_numeric_ids = (None, None);
                let completed = tokio::time::timeout(Duration::from_secs(10), async {
                    let first_lease = entered_lease(scope, &first_id).await;
                    phase.store(1, std::sync::atomic::Ordering::Relaxed);
                    let second_lease = entered_lease(scope, &second_id).await;
                    assert_ne!(first_lease, second_lease);
                    // Let the later real factory consume inputs first.
                    scope.inputs.bind(second_id.clone(), second_generation, second_inputs);
                    phase.store(2, std::sync::atomic::Ordering::Relaxed);
                    let (second_request, second_native_cwd) = second.native_new.await.unwrap();
                    native_numeric_ids.1 = second_request.as_i64();
                    assert_eq!(second_native_cwd, second_cwd.path().to_string_lossy().as_ref());
                    assert!(first.peer.requests().is_empty());
                    assert!(matches!(second.sdk_seen.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)));
                    assert!(matches!(second.gate_seen.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)));
                    scope.inputs.bind(first_id.clone(), first_generation, first_inputs);
                    phase.store(3, std::sync::atomic::Ordering::Relaxed);
                    let (first_request, first_native_cwd) = first.native_new.await.unwrap();
                    native_numeric_ids.0 = first_request.as_i64();
                    assert_eq!(first_native_cwd, first_cwd.path().to_string_lossy().as_ref());
                    // These are replies to the observed native request ids, never row edits.
                    phase.store(4, std::sync::atomic::Ordering::Relaxed);
                    second.peer.send(serde_json::json!({"jsonrpc":"2.0","id":second_request,"result":{
                        "sessionId":"fixture-inputs","modes":{"currentModeId":"second-inputs","availableModes":[{"id":"second-inputs","name":"Second inputs"}]}
                    }})).await;
                    phase.store(5, std::sync::atomic::Ordering::Relaxed);
                    second.sdk_seen.await.unwrap();
                    phase.store(6, std::sync::atomic::Ordering::Relaxed);
                    second.gate_seen.await.unwrap();
                    assert!(matches!(first.sdk_seen.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)));
                    assert!(matches!(first.gate_seen.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)));
                    assert!(second.release.release());
                    phase.store(7, std::sync::atomic::Ordering::Relaxed);
                    let mut second_client = scope.installed(second_id, second_events).await;
                    phase.store(8, std::sync::atomic::Ordering::Relaxed);
                    second_client.ready().await;
                    phase.store(9, std::sync::atomic::Ordering::Relaxed);
                    modes(&mut second_client, "second-inputs").await;
                    phase.store(10, std::sync::atomic::Ordering::Relaxed);
                    first.peer.send(serde_json::json!({"jsonrpc":"2.0","id":first_request,"result":{
                        "sessionId":"fixture-inputs","modes":{"currentModeId":"first-inputs","availableModes":[{"id":"first-inputs","name":"First inputs"}]}
                    }})).await;
                    phase.store(11, std::sync::atomic::Ordering::Relaxed);
                    first.sdk_seen.await.unwrap();
                    phase.store(12, std::sync::atomic::Ordering::Relaxed);
                    first.gate_seen.await.unwrap();
                    assert!(first.release.release());
                    phase.store(13, std::sync::atomic::Ordering::Relaxed);
                    let mut first_client = scope.installed(first_id, first_events).await;
                    phase.store(14, std::sync::atomic::Ordering::Relaxed);
                    first_client.ready().await;
                    phase.store(15, std::sync::atomic::Ordering::Relaxed);
                    modes(&mut first_client, "first-inputs").await;
                    let first_birth = first.inspection.native_spawn_proof(&first_client.lease)
                        .expect("read original first native proof").expect("genuine first Ready has its original native birth");
                    let second_birth = second.inspection.native_spawn_proof(&second_client.lease)
                        .expect("read original second native proof").expect("genuine second Ready has its original native birth");
                    assert_eq!(first_birth.session_id, first_client.id);
                    assert_eq!(second_birth.session_id, second_client.id);
                    assert_ne!(first_birth.producer, second_birth.producer);
                    retained.lock().unwrap().push(evidence(scope, &first_client));
                    retained.lock().unwrap().push(evidence(scope, &second_client));
                    phase.store(16, std::sync::atomic::Ordering::Relaxed);
                    first_client.shutdown().await.unwrap();
                    phase.store(17, std::sync::atomic::Ordering::Relaxed);
                    second_client.shutdown().await.unwrap();
                }).await;
                if completed.is_err() {
                    let phase_index = phase.load(std::sync::atomic::Ordering::Relaxed);
                    panic!("input timeout round={round_index} same_cwd={same_cwd} phase_index={phase_index} phase={} original_native_new_numeric_ids={native_numeric_ids:?} first_native_methods={:?} second_native_methods={:?}",
                        PHASES[phase_index], first.peer.timeout_methods(), second.peer.timeout_methods());
                }
                for bind_first in [false, true] {
                    let cancelled = held_case(scope, first_cwd.path().into(), "gemini");
                    let (id, generation, _events, inputs) = scope.create_unbound(cancelled.inputs, "inputs-cancelled", None, None).await;
                    let lease = tokio::time::timeout(Duration::from_secs(10), entered_lease(scope, &id)).await.unwrap();
                    let retirement = Arc::new(ManagedRetirement::capture(scope.state.clone(), lease.clone()));
                    scope.retirements.push(retirement.clone());
                    if bind_first {
                        scope.inputs.bind(id.clone(), generation, inputs);
                        // No await separates binding from cancellation of this actual producer.
                        lease.cancellation().cancel();
                    } else {
                        lease.cancellation().cancel();
                        scope.inputs.bind(id.clone(), generation, inputs);
                    }
                    tokio::time::timeout(Duration::from_secs(5), async {
                        retirement.wait_original_constructor().await;
                        retirement.retire().await
                    }).await.expect("original cancelled constructor and qualified retirement settle within the unchanged five-second budget").unwrap();
                    assert!(lease.was_cancelled_before_birth(), "real birth exclusion, not a missing PID");
                    assert!(lease.original_constructor_completed());
                    assert!(cancelled.peer.requests().is_empty());
                }
            }
        })).await;
        for proof in observed.lock().unwrap().iter() {
            assert_retired(proof);
        }
    }

    fn native_config(scope: &mut ManagedScope, cwd: PathBuf, sid: &'static str) -> SpawnConfig {
        scope.native_peer(cwd, "codex", "codex", move |request| {
            let id = request.get("id")?;
            Some(match request["method"].as_str().unwrap() {
                "initialize" => serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":1,"agentCapabilities":{}}}),
                "session/new" => serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":sid}}),
                method => panic!("unexpected native operation in safety fixture: {method}"),
            })
        }).0
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn historical_native_birth_never_adopts_or_signals_foreign_sentinel() {
        const NAME: &str = "acp::supervisor::test_support::managed::safety_tests::historical_native_birth_never_adopts_or_signals_foreign_sentinel";
        if child_if_requested(NAME).await {
            return;
        }
        let sentinel: Arc<Mutex<Option<Sentinel>>> = Arc::default();
        let survivors = sentinel.clone();
        let observed: Arc<Mutex<Vec<Evidence>>> = Arc::default();
        let retained = observed.clone();
        with_managed(move |scope| {
            Box::pin(async move {
                let project = scope.tempdir();
                let config = native_config(scope, project.path().into(), "birth-first");
                let first = scope.spawn(config, "birth-first").await.unwrap();
                first.ready().await;
                let first_evidence = evidence(scope, &first);
                let guarded = Sentinel::different_birth(&first_evidence.native.birth);
                let sentinel_pid = guarded.child.id();
                *survivors.lock().unwrap() = Some(guarded);
                let mut historical = first_evidence.native.clone();
                // Deterministic stale numeric identity challenge. Its ORIGINAL
                // birth remains unchanged; the actual foreign child has another birth.
                historical.pid = sentinel_pid;
                scope.native_births.push(historical);
                scope.capture_native();
                assert!(!scope.native.iter().any(|(pid, _, _)| *pid == sentinel_pid));
                survivors.lock().unwrap().as_mut().unwrap().assert_alive();
                retained.lock().unwrap().push(first_evidence);
                first.shutdown().await.unwrap();
                let project = scope.tempdir();
                let config = native_config(scope, project.path().into(), "birth-second");
                let second = scope.spawn(config, "birth-second").await.unwrap();
                second.ready().await;
                scope.capture_native();
                assert!(!scope.native.iter().any(|(pid, _, _)| *pid == sentinel_pid));
                survivors.lock().unwrap().as_mut().unwrap().assert_alive();
                retained.lock().unwrap().push(evidence(scope, &second));
                // finish performs another historical scan and actual owned signalling.
            })
        })
        .await;
        sentinel.lock().unwrap().as_mut().unwrap().assert_alive();
        let proofs = observed.lock().unwrap();
        assert_eq!(proofs.len(), 2);
        for proof in proofs.iter() {
            assert_retired(proof);
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn helper_failures_survive_ignored_join_and_fail_after_real_cleanup() {
        const NAME: &str = "acp::supervisor::test_support::managed::safety_tests::helper_failures_survive_ignored_join_and_fail_after_real_cleanup";
        if child_if_requested(NAME).await {
            return;
        }
        // Two actual hidden-panic cases plus the explicitly requested cancellation control.
        for mode in 0..3 {
            let before = ["HOME", "PATH", "AOE_APP_DIR", "CLAUDE_CONFIG_DIR"]
                .map(|key| (key, std::env::var_os(key)));
            let observed: Arc<Mutex<Vec<Evidence>>> = Arc::default();
            let retained = observed.clone();
            let outcome = std::panic::AssertUnwindSafe(with_managed(move |scope| {
                Box::pin(async move {
                    let project = scope.tempdir();
                    let config = native_config(scope, project.path().into(), "helper-safety");
                    let client = scope.spawn(config, "helper-safety").await.unwrap();
                    client.ready().await;
                    retained.lock().unwrap().push(evidence(scope, &client));
                    let changed_home = scope.tempdir();
                    scope.save_environment(&["HOME", "PATH"]);
                    std::env::set_var("HOME", changed_home.path());
                    let mut paths = std::env::split_paths(&std::env::var_os("PATH").unwrap())
                        .collect::<Vec<_>>();
                    paths.push(changed_home.path().to_owned());
                    std::env::set_var("PATH", std::env::join_paths(paths).unwrap());
                    let (entered, started) = tokio::sync::oneshot::channel();
                    let helper = scope.spawn_helper(async move {
                        let _ = entered.send(());
                        if mode == 2 {
                            std::future::pending::<()>().await;
                        }
                        panic!("hidden managed helper assertion");
                    });
                    started.await.unwrap();
                    if mode == 2 {
                        helper.abort();
                        let _ = helper.await;
                    } else if mode == 1 {
                        let _ = helper.await; // Deliberately ignore the consumed JoinError.
                    } else {
                        tokio::time::timeout(Duration::from_secs(5), async {
                            loop {
                                let finished = helper
                                    .0
                                    .lock()
                                    .unwrap()
                                    .task
                                    .as_ref()
                                    .unwrap()
                                    .is_finished();
                                if finished {
                                    break;
                                }
                                tokio::task::yield_now().await;
                            }
                        })
                        .await
                        .unwrap();
                        // No body join: finish must observe and retain the panic.
                    }
                })
            }))
            .catch_unwind()
            .await;
            for (key, value) in before {
                assert_eq!(std::env::var_os(key), value, "{key} was not restored");
            }
            let proofs = observed.lock().unwrap();
            assert_eq!(
                proofs.len(),
                1,
                "the genuine native/Applied fixture must have run"
            );
            for proof in proofs.iter() {
                assert_retired(proof);
            }
            if mode == 2 {
                assert!(
                    outcome.is_ok(),
                    "requested/observed cancellation is not a helper panic"
                );
            } else {
                let panic = outcome.expect_err("ignored helper panic must still fail the host");
                let message = panic
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| panic.downcast_ref::<&str>().copied())
                    .unwrap_or("non-string panic");
                assert!(
                    message.contains(
                        "managed helper failure after cleanup and environment restoration"
                    ),
                    "{message}"
                );
                assert!(
                    message.contains("hidden managed helper assertion"),
                    "{message}"
                );
            }
        }
    }
}
