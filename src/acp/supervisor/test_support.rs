//! Test hooks and fixtures for driving supervisor lifecycles without real runners.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc};

use super::{
    lock_recover, BroadcastSink, ChannelSink, Launcher, ResumeKind, ResumeReservation,
    ResumeReservationOutcome, SpawnRequest, Supervisor, SupervisorError, WorkerHandle, WorkerKind,
};
use crate::acp::acp_client::{AcpClient, SpawnConfig};
use crate::acp::agent_registry::AgentSpec;
use crate::acp::approvals::Nonce;
use crate::acp::event_store::EventStore;
use crate::acp::runner_lifecycle::{ExecutionAdmission, Lease, RunnerIdentity};
use crate::acp::state::{AcpSessionId, Event};
use crate::process::worker_registry::{self, WorkerRecord};

impl<S: BroadcastSink> Supervisor<S> {
    pub(crate) fn with_launcher(mut self, launcher: Launcher) -> Self {
        self.launcher = launcher;
        self
    }

    /// What the drain task records for a worker that failed before establishing a session.
    pub(crate) fn note_startup_failure(&self, session_id: &str) {
        lock_recover(&self.startup_failures).insert(session_id.to_string());
    }

    /// Reports each session a `wait_for_worker` call starts parking on.
    pub(crate) fn watch_worker_waits(&self) -> broadcast::Receiver<String> {
        self.worker_waits.subscribe()
    }

    pub(crate) async fn test_flush_worker_commands(&self, session_id: &str) {
        self.client_for_session(session_id)
            .await
            .unwrap()
            .test_flush_commands()
            .await;
    }

    /// Occupy a slot with a fake in-memory worker; returns its epoch.
    pub(crate) async fn test_insert_worker(&self, session_id: &str) -> u64 {
        let (client, _tx) = AcpClient::fake_for_test(AcpSessionId(format!("acp-{session_id}")));
        self.test_install_handle(session_id, client, WorkerKind::Stdio, None)
            .await
            .epoch()
    }

    /// Occupy a slot with a fake worker already carrying what a real drain
    /// publishes before the assigned frame reaches the listener: the
    /// agent-assigned id, and the store the launch observed (#4127).
    pub(crate) async fn test_insert_worker_with_native_handoff(
        &self,
        session_id: &str,
        acp_session_id: &str,
        store: Option<crate::session::ExecutionBinding>,
    ) -> u64 {
        let (mut client, _tx) = AcpClient::fake_for_test(AcpSessionId(format!("acp-{session_id}")));
        client.native_store = store;
        let lease = self
            .test_install_handle(session_id, client, WorkerKind::Stdio, None)
            .await;
        let mut workers = self.workers.lock().await;
        workers
            .get_mut(session_id)
            .expect("test worker")
            .native_session_id = Some(acp_session_id.to_string());
        lease.epoch()
    }

    /// Replace a fixture's client under a fresh respawn epoch, as a respawn does.
    pub(crate) async fn test_respawn_worker(&self, session_id: &str) -> u64 {
        let (client, _tx) = AcpClient::fake_for_test(AcpSessionId(format!("acp-{session_id}")));
        let mut workers = self.workers.lock().await;
        let handle = workers.get_mut(session_id).expect("test worker");
        let (respawn, _) = lock_recover(&self.lifecycle)
            .begin_respawn(&handle.lease)
            .expect("fixture is running");
        lock_recover(&self.lifecycle)
            .install(&respawn, None)
            .expect("fixture installs its respawn");
        handle.client = Arc::new(client);
        handle.lease = respawn.clone();
        handle.native_session_id = None;
        respawn.epoch()
    }

    /// A fake worker whose command loop records every command it receives.
    pub(crate) async fn test_insert_worker_cmd_recording(
        &self,
        session_id: &str,
    ) -> Arc<std::sync::Mutex<Vec<&'static str>>> {
        let (client, _tx, cmds) =
            AcpClient::fake_for_test_cmd_recording(AcpSessionId(format!("acp-{session_id}")));
        self.test_install_handle(session_id, client, WorkerKind::Stdio, None)
            .await;
        cmds
    }

    pub(super) async fn test_install_stdio(&self, session_id: &str) -> Lease {
        let (client, _tx) = AcpClient::fake_for_test(AcpSessionId(session_id.into()));
        self.test_install_handle(session_id, client, WorkerKind::Stdio, None)
            .await
    }

    pub(super) async fn test_install_runner(
        &self,
        session_id: &str,
        config: SpawnConfig,
        identity: Option<RunnerIdentity>,
    ) -> Lease {
        let (client, _tx) = AcpClient::fake_for_test(AcpSessionId(session_id.into()));
        let kind = WorkerKind::Runner {
            spawn_config: Box::new(config),
        };
        self.test_install_handle(session_id, client, kind, identity)
            .await
    }

    /// Install a fake worker under a fresh lease, as `spawn_inner` would.
    pub(super) async fn test_install_handle(
        &self,
        session_id: &str,
        client: AcpClient,
        kind: WorkerKind,
        identity: Option<RunnerIdentity>,
    ) -> Lease {
        let mut workers = self.workers.lock().await;
        let lease = {
            let mut table = lock_recover(&self.lifecycle);
            let lease = table
                .admit(session_id, ResumeKind::Spawn)
                .expect("test fixture admits a fresh session");
            table
                .install(&lease, identity)
                .expect("test fixture installs its own lease");
            lease
        };
        workers.insert(
            session_id.to_string(),
            WorkerHandle {
                client: Arc::new(client),
                native_session_id: None,
                drain_task: tokio::spawn(async {}),
                restart_history: vec![],
                kind,
                lease: lease.clone(),
            },
        );
        lease
    }

    /// Drop a fake worker, freeing its capacity slot.
    pub(crate) async fn test_remove_worker(&self, session_id: &str) {
        let mut workers = self.workers.lock().await;
        if let Some(handle) = workers.remove(session_id) {
            lock_recover(&self.lifecycle).release_running(&handle.lease);
        }
    }
}

/// In-memory sink that captures published frames.
#[derive(Default)]
pub(super) struct VecSink {
    pub(super) frames: std::sync::Mutex<Vec<(String, u64, Event)>>,
    pub(super) stale_nonces: std::sync::Mutex<Vec<Nonce>>,
    pub(super) stale_elicitation_nonces: std::sync::Mutex<Vec<Nonce>>,
    pub(super) stale_background_agent_ids: std::sync::Mutex<Vec<String>>,
}

impl VecSink {
    pub(super) fn new() -> Arc<Self> {
        Arc::default()
    }
}

impl BroadcastSink for VecSink {
    fn publish(&self, session_id: &str, seq: u64, event: &Event) {
        self.frames
            .lock()
            .unwrap()
            .push((session_id.to_string(), seq, event.clone()));
    }
    fn unresolved_approval_nonces(&self, _session_id: &str) -> Vec<Nonce> {
        self.stale_nonces.lock().unwrap().clone()
    }
    fn unresolved_elicitation_nonces(&self, _session_id: &str) -> Vec<Nonce> {
        self.stale_elicitation_nonces.lock().unwrap().clone()
    }
    fn unresolved_background_agent_ids(&self, _session_id: &str) -> Vec<String> {
        self.stale_background_agent_ids.lock().unwrap().clone()
    }
}

pub(super) fn stopped_reasons(sink: &VecSink, session_id: &str) -> Vec<String> {
    sink.frames
        .lock()
        .unwrap()
        .iter()
        .filter(|(id, _, _)| id == session_id)
        .filter_map(|(_, _, ev)| match ev {
            Event::Stopped { reason } => Some(reason.clone()),
            _ => None,
        })
        .collect()
}

pub(super) fn channel_sink() -> (
    Arc<ChannelSink>,
    Arc<EventStore>,
    broadcast::Receiver<crate::server::AcpBroadcastFrame>,
    tempfile::TempDir,
) {
    let tmp = tempfile::TempDir::new().unwrap();
    let event_store = Arc::new(EventStore::open(&tmp.path().join("acp.db"), 1000).unwrap());
    let (tx, rx) = broadcast::channel(16);
    let sink = Arc::new(ChannelSink {
        tx,
        event_store: event_store.clone(),
        control_cache: Arc::new(crate::acp::control_cache::ControlStateCache::new()),
    });
    (sink, event_store, rx, tmp)
}

/// Isolate HOME under a short `/tmp` path so runner socket paths stay valid.
pub(super) fn isolate_home() -> (crate::session::test_support::AppDirGuard, tempfile::TempDir) {
    let tmp = tempfile::TempDir::with_prefix_in("aoe-lease-", "/tmp").unwrap();
    let home = crate::session::test_support::isolate_app_dir_at(tmp.path());
    (home, tmp)
}

pub(super) fn spawn_request(session_id: &str) -> SpawnRequest {
    SpawnRequest {
        session_id: session_id.into(),
        agent: "claude-code".into(),
        tool: "claude-code".into(),
        cwd: std::env::temp_dir(),
        additional_dirs: vec![],
        provider_env: vec![],
        model: None,
        effort: None,
        effort_explicit: false,
        stored_acp_session_id: None,
        fork_from: None,
        sandbox_continuation: super::SandboxContinuation::Persisted,
        seed_history_replay: false,
        sandbox_info: None,
        source_profile: None,
        yolo_mode: false,
        acp_mode_id: None,
        agent_command_override: None,
        claude_store_pin: None,
    }
}

pub(super) fn runner_config(socket_path: PathBuf) -> SpawnConfig {
    SpawnConfig {
        execution_admission: None,
        managed_profile: None,
        wrapper_substitution: None,
        agent_key: "claude".into(),
        tool: "claude".into(),
        spec: AgentSpec {
            command: "/bin/true".into(),
            args: vec![],
            description: "test fixture".into(),
            env_allowlist: None,
        },
        cwd: std::env::temp_dir(),
        additional_dirs: vec![],
        provider_env: vec![],
        host_environment: vec![],
        default_effort: None,
        default_effort_explicit: false,
        default_mode: None,
        default_model: None,
        socket_path: Some(socket_path),
        stored_acp_session_id: None,
        fork_from: None,
        seed_history_replay: false,
        artifact_dir: None,
        sandbox_info: None,
        source_profile: None,
        mcp_servers: Vec::new(),
        generation: 0,
        claude_store_pin: None,
        base_host_environment: vec![],
    }
}

pub(super) fn worker_record(session_id: &str, pid: u32, socket: PathBuf) -> WorkerRecord {
    WorkerRecord::new(
        session_id.into(),
        pid,
        socket,
        "claude-agent-acp".into(),
        "claude-code".into(),
        std::env::temp_dir(),
        None,
        vec![],
        vec![],
        None,
        None,
    )
}

pub(super) fn save_record(session_id: &str, pid: u32, generation: u64) {
    let socket = worker_registry::socket_path_for(session_id).unwrap();
    worker_registry::save(&worker_record(session_id, pid, socket).with_generation(generation))
        .unwrap();
}

/// Real kernel execution published through the production launch journal. Its private
/// authorization pipe is consumed before the fixture waits for its stop marker.
pub(super) struct PublishedExecution {
    pub pid: u32,
    pub nonce: uuid::Uuid,
    release: PathBuf,
    stop: tokio::task::JoinHandle<()>,
    _directory: tempfile::TempDir,
}

impl Drop for PublishedExecution {
    fn drop(&mut self) {
        std::fs::write(&self.release, b"stop").unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while crate::process::worker::is_process_group_alive(self.pid)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        self.stop.abort();
    }
}

pub(super) fn published_execution(
    id: &str,
    generation: u64,
    profile: &str,
    admission: Option<&ExecutionAdmission>,
) -> PublishedExecution {
    let storage = crate::session::Storage::new_unwatched(profile).unwrap();
    storage
        .update(|rows, _| {
            if !rows.iter().any(|row| row.id == id) {
                let mut row = crate::session::Instance::new(id, "/tmp");
                row.id = id.to_owned();
                row.source_profile = profile.to_owned();
                row.view = crate::session::View::Structured;
                rows.push(row);
            }
            Ok(())
        })
        .unwrap();
    let directory = tempfile::TempDir::new().unwrap();
    let release = directory.path().join("stop");
    let mut command = tokio::process::Command::new("sh");
    command.arg("-c").arg(r#"received="$(dd bs=1 count=16 2>/dev/null | od -An -tx1 | tr -d '[:space:]')"; [ "$received" = "$2" ] || exit 70; while [ ! -e "$1" ]; do sleep 0.01; done"#)
        .arg("journal-fixture").arg(&release);
    unsafe {
        command.pre_exec(|| {
            nix::unistd::setsid().map_err(std::io::Error::other)?;
            Ok(())
        });
    }
    let launch = crate::session::runner_journal::ManagedLaunch::new(
        crate::session::deletion::SessionPathOwner {
            profile,
            session_id: id,
        },
        generation,
    )
    .unwrap();
    let nonce = launch.nonce();
    command.arg(nonce.simple().to_string());
    let pid = launch
        .spawn(&mut command, |identity| {
            if let Some(admission) = admission {
                admission.capture(identity);
            }
        })
        .unwrap();
    let listener = tokio::net::UnixListener::bind(
        crate::session::runner_journal::stop_socket(id, pid).unwrap(),
    )
    .unwrap();
    let stop_release = release.clone();
    let stop = tokio::spawn(async move {
        crate::session::runner_journal::wait_for_stop(listener, nonce)
            .await
            .unwrap();
        std::fs::write(stop_release, b"stop").unwrap();
    });
    PublishedExecution {
        pid,
        nonce,
        release,
        stop,
        _directory: directory,
    }
}

/// Holds a gated launch until the test opens it.
#[derive(Default)]
pub(super) struct Gate {
    pub(super) entered: Arc<tokio::sync::Notify>,
    pub(super) open: Arc<tokio::sync::Notify>,
}

/// Holds a real published execution behind the launch gate. Event senders are retained
/// until supervisor shutdown, so the drain cannot mistake the handshake for a crash.
pub(super) fn gated_launcher(gate: &Gate) -> Launcher {
    let entered = Arc::clone(&gate.entered);
    let open = Arc::clone(&gate.open);
    let senders: Arc<std::sync::Mutex<Vec<mpsc::Sender<Event>>>> = Default::default();
    let executions: Arc<std::sync::Mutex<Vec<PublishedExecution>>> = Default::default();
    Arc::new(move |config: SpawnConfig, session_id: AcpSessionId| {
        let entered = Arc::clone(&entered);
        let open = Arc::clone(&open);
        let senders = Arc::clone(&senders);
        let executions = Arc::clone(&executions);
        Box::pin(async move {
            let profile = config
                .managed_profile
                .as_deref()
                .expect("gated launch requires an explicit stored owner");
            let execution = published_execution(
                &session_id.0,
                config.generation,
                profile,
                config.execution_admission.as_ref(),
            );
            let pid = execution.pid;
            let nonce = execution.nonce;
            let mut record = worker_record(
                &session_id.0,
                pid,
                worker_registry::socket_path_for(&session_id.0).unwrap(),
            )
            .with_generation(config.generation);
            record.source_profile = Some(profile.to_owned());
            record.launch_nonce = Some(nonce);
            worker_registry::save(&record).unwrap();
            executions.lock().unwrap().push(execution);
            entered.notify_one();
            open.notified().await;
            let (mut client, tx) = AcpClient::fake_for_test(session_id);
            client.capture_runner(pid, nonce);
            senders.lock().unwrap().push(tx);
            Ok(client)
        })
    })
}

pub(super) fn reserve(
    outcome: Result<ResumeReservationOutcome, SupervisorError>,
) -> ResumeReservation {
    match outcome.expect("begin_resume must not error") {
        ResumeReservationOutcome::Reserved(r) => r,
        ResumeReservationOutcome::AlreadyPresent => panic!("expected a fresh reservation"),
    }
}
