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
use crate::acp::runner_lifecycle::{Lease, ProcessControl, RunnerIdentity, Settlement};
use crate::acp::state::{AcpSessionId, Event};
use crate::process::worker_registry::{self, WorkerRecord};

impl<S: BroadcastSink> Supervisor<S> {
    pub(crate) fn with_process_control(mut self, control: Arc<dyn ProcessControl>) -> Self {
        self.process_control = control;
        self
    }

    pub(crate) fn with_launcher(mut self, launcher: Launcher) -> Self {
        self.launcher = launcher;
        self
    }

    pub(crate) fn test_cancel_requested(&self, lease: &Lease) -> bool {
        lock_recover(&self.lifecycle)
            .cancel_requested(lease)
            .is_some()
    }

    /// What the drain task records for a worker that failed before establishing a session.
    pub(crate) fn note_startup_failure(&self, session_id: &str) {
        lock_recover(&self.startup_failures).insert(session_id.to_string());
    }

    /// Leave the session mid-teardown, where an unproven settlement parks it.
    pub(crate) fn test_hold_stopping(&self, session_id: &str) -> Option<Lease> {
        lock_recover(&self.lifecycle).adopt_for_stop(session_id)
    }

    /// End a teardown `test_hold_stopping` left open.
    pub(crate) fn test_settle_stopping(&self, lease: &Lease) {
        lock_recover(&self.lifecycle).settle(lease, Settlement::Proven);
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

    pub(crate) async fn test_install_attached(&self, session_id: &str, identity: RunnerIdentity) {
        let (client, _tx) = AcpClient::fake_for_test(AcpSessionId(session_id.into()));
        self.test_install_handle(session_id, client, WorkerKind::Attached, Some(identity))
            .await;
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
            let prime = crate::process::worker_registry::load(session_id)
                .ok()
                .flatten()
                .filter(|record| identity.is_some_and(|identity| identity.owns_record(record)))
                .map(|record| crate::acp::runner_lifecycle::PrimeOwnership::from_record(&record))
                .unwrap_or_else(|| match &kind {
                    WorkerKind::Runner { spawn_config } if spawn_config.uses_native_prime() => {
                        crate::acp::runner_lifecycle::PrimeOwnership::Unresolved
                    }
                    _ => crate::acp::runner_lifecycle::PrimeOwnership::NotPrime,
                });
            lease.record_prime(prime);
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
                launch_epoch: lease.epoch(),
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

pub(crate) fn spawn_request(session_id: &str) -> SpawnRequest {
    SpawnRequest {
        launch_admission: None,
        expected_lifecycle_generation: 0,
        provider: None,
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
        prior_conversation: false,
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
        launch_lease: None,
        provider_routing: Vec::new(),
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
        prior_conversation: false,
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

pub(crate) fn save_record(session_id: &str, pid: u32, generation: u64) {
    let socket = worker_registry::socket_path_for(session_id).unwrap();
    let mut record = worker_record(session_id, pid, socket).with_generation(generation);
    record.runner_identity = None;
    worker_registry::save(&record).unwrap();
}

pub(crate) fn attested_test_birth(
    pid: u32,
    generation: u64,
) -> crate::process::ProcessBirthIdentity {
    crate::process::ProcessBirthIdentity {
        boot_id: "supervisor-test-boot".into(),
        start_id: format!("supervisor-test:{pid}:{generation}"),
    }
}

pub(crate) fn attested_test_identity(pid: u32, generation: u64) -> RunnerIdentity {
    RunnerIdentity {
        pid,
        generation,
        birth_fingerprint: Some(attested_test_birth(pid, generation).fingerprint()),
    }
}

pub(crate) fn save_attested_test_record(session_id: &str, pid: u32, generation: u64) {
    let socket = worker_registry::socket_path_for(session_id).unwrap();
    let mut record = worker_record(session_id, pid, socket).with_generation(generation);
    record.runner_identity = Some(attested_test_birth(pid, generation));
    record.scope = worker_registry::ScopeOwnership::Unscoped;
    worker_registry::save(&record).unwrap();
}

/// Holds a gated launch until the test opens it.
#[derive(Default)]
pub(crate) struct Gate {
    pub(crate) entered: Arc<tokio::sync::Notify>,
    pub(crate) open: Arc<tokio::sync::Notify>,
}

/// Freeze the fake producer's proof, then park before registry publication.
/// Event senders are retained so drains never see a close.
pub(super) fn gated_launcher(gate: &Gate, pid: u32) -> Launcher {
    let entered = Arc::clone(&gate.entered);
    let open = Arc::clone(&gate.open);
    let senders: Arc<std::sync::Mutex<Vec<mpsc::Sender<Event>>>> = Default::default();
    Arc::new(move |config: SpawnConfig, session_id: AcpSessionId| {
        let entered = Arc::clone(&entered);
        let open = Arc::clone(&open);
        let senders = Arc::clone(&senders);
        Box::pin(async move {
            let lease = config
                .launch_lease
                .as_ref()
                .expect("supervisor retains its producer lease");
            lease.record_scope(worker_registry::ScopeOwnership::Unscoped);
            assert!(lease.record_runner(attested_test_identity(pid, config.generation)));
            entered.notify_one();
            tokio::select! {
                biased;
                _ = lease.cancellation().cancelled() => {
                    Err(crate::acp::acp_client::AcpError::Spawn(
                        "constructor cancelled before registry publication".into(),
                    ))
                }
                _ = open.notified() => {
                    save_attested_test_record(&session_id.0, pid, config.generation);
                    let (client, tx) = AcpClient::fake_for_test(session_id);
                    senders.lock().unwrap().push(tx);
                    Ok(client.with_runner_pid(pid))
                }
            }
        })
    })
}

/// Park after the runner record exists but before the handshake returns.
pub(crate) fn recording_gated_launcher(
    gate: &Gate,
    pid: u32,
    session_id: &str,
    native_id: &str,
    runner_identity: Option<crate::process::ProcessBirthIdentity>,
) -> (Launcher, Arc<std::sync::atomic::AtomicBool>) {
    let (client, sender, deleted) =
        AcpClient::fake_for_test_recording(AcpSessionId(session_id.into()));
    let client = Arc::new(std::sync::Mutex::new(Some(client)));
    let entered = gate.entered.clone();
    let open = gate.open.clone();
    let native_id = native_id.to_owned();
    let launcher: Launcher = Arc::new(move |config, session_id| {
        let client = client.clone();
        let entered = entered.clone();
        let open = open.clone();
        let native_id = native_id.clone();
        let sender = sender.clone();
        let runner_identity = runner_identity.clone();
        Box::pin(async move {
            let _sender = sender;
            let socket = worker_registry::socket_path_for(&session_id.0).unwrap();
            let mut record = worker_record(&session_id.0, pid, socket.clone())
                .with_generation(config.generation);
            record.stored_acp_session_id = Some(native_id);
            record.source_profile = config.source_profile;
            record.runner_identity = runner_identity;
            let lease = config
                .launch_lease
                .as_ref()
                .expect("supervisor retains its producer lease");
            lease.record_scope(record.scope.clone());
            assert!(lease.record_runner(RunnerIdentity::from_record(&record)));
            worker_registry::save(&record).unwrap();
            worker_registry::touch_live_socket(&socket);
            entered.notify_one();
            open.notified().await;
            let client = client.lock().unwrap().take().expect("one gated launch");
            Ok(client.with_runner_pid(pid))
        })
    });
    (launcher, deleted)
}

pub(super) fn reserve(
    outcome: Result<ResumeReservationOutcome, SupervisorError>,
) -> ResumeReservation {
    match outcome.expect("begin_resume must not error") {
        ResumeReservationOutcome::Reserved(r) => r,
        ResumeReservationOutcome::AlreadyPresent => panic!("expected a fresh reservation"),
    }
}

/// Records real process-group signals while treating fixture scope names as exact probes.
#[derive(Default)]
pub(crate) struct ObservedProcessControl {
    inner: crate::acp::runner_lifecycle::SystemProcessControl,
    signals: std::sync::Mutex<Vec<(RunnerIdentity, &'static str)>>,
    scopes: std::sync::Mutex<Vec<worker_registry::ScopeOwnership>>,
}

impl ObservedProcessControl {
    pub(crate) fn signals(&self) -> Vec<(RunnerIdentity, &'static str)> {
        self.signals.lock().unwrap().clone()
    }

    pub(crate) fn scopes(&self) -> Vec<worker_registry::ScopeOwnership> {
        self.scopes.lock().unwrap().clone()
    }
}

impl ProcessControl for ObservedProcessControl {
    fn is_alive(&self, identity: RunnerIdentity) -> bool {
        self.inner.is_alive(identity)
    }

    fn terminate_group(&self, identity: RunnerIdentity) {
        self.signals.lock().unwrap().push((identity, "TERM"));
        self.inner.terminate_group(identity);
    }

    fn kill_group(&self, identity: RunnerIdentity) {
        self.signals.lock().unwrap().push((identity, "KILL"));
        self.inner.kill_group(identity);
    }

    fn forget(&self, identity: RunnerIdentity) {
        self.inner.forget(identity);
    }

    fn release_scope(&self, _session_id: &str, scopes: &[worker_registry::ScopeOwnership]) -> bool {
        self.scopes.lock().unwrap().extend_from_slice(scopes);
        true
    }

    fn scopes_stopped(
        &self,
        _session_id: &str,
        _scopes: &[worker_registry::ScopeOwnership],
    ) -> bool {
        true
    }
}

/// A real child identity and compatible control socket. The first initialize stays
/// unanswered until its exact connection closes. The second resumes the same turn.
pub(crate) struct HeldAttachRunner {
    pub(crate) record: WorkerRecord,
    pub(crate) identity: RunnerIdentity,
    entered: Option<tokio::sync::oneshot::Receiver<()>>,
    disconnected: Option<tokio::sync::oneshot::Receiver<()>>,
    completion: Option<tokio::sync::oneshot::Sender<()>>,
    peer: tokio::task::JoinHandle<()>,
    child: std::process::Child,
    _socket_dir: tempfile::TempDir,
}

impl HeldAttachRunner {
    pub(crate) fn new(
        session_id: &str,
        cwd: &std::path::Path,
        generation: u64,
        profile: Option<&str>,
    ) -> Self {
        use crate::acp::control_protocol::{self, ControlBody, PromptOutcome};
        use std::os::unix::process::CommandExt as _;

        let socket_dir = tempfile::TempDir::with_prefix_in("aoe-attach-", "/tmp").unwrap();
        let socket = socket_dir.path().join("runner.sock");
        let listener =
            tokio::net::UnixListener::bind(crate::process::worker::control_socket_sibling(&socket))
                .unwrap();
        let child = std::process::Command::new("sleep")
            .arg("60")
            .process_group(0)
            .spawn()
            .unwrap();
        let native_id = format!("native-{session_id}-{generation}");
        let mut record = WorkerRecord::new(
            session_id.into(),
            child.id(),
            socket,
            "test-agent-acp".into(),
            "test-agent".into(),
            cwd.into(),
            None,
            vec![],
            vec![],
            Some(native_id.clone()),
            profile.map(str::to_owned),
        )
        .with_generation(generation);
        record.scope = worker_registry::ScopeOwnership::Owned {
            unit: format!("aoe-attach-{session_id}-{generation}.scope"),
        };
        let identity = RunnerIdentity::from_record(&record);
        assert!(identity.birth_fingerprint.is_some());
        worker_registry::save(&record).unwrap();
        let (entered_tx, entered) = tokio::sync::oneshot::channel();
        let (disconnected_tx, disconnected) = tokio::sync::oneshot::channel();
        let (completion, completion_rx) = tokio::sync::oneshot::channel();
        let id = session_id.to_owned();
        let peer = tokio::spawn(async move {
            let mut entered_tx = Some(entered_tx);
            let mut disconnected_tx = Some(disconnected_tx);
            let mut completion_rx = Some(completion_rx);
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                control_protocol::write_frame(
                    &mut stream,
                    &ControlBody::Hello {
                        control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
                        session_id: id.clone(),
                    },
                )
                .await
                .unwrap();
                while let Some(frame) = control_protocol::read_frame(&mut stream).await.unwrap() {
                    match frame {
                        ControlBody::Attach {
                            control_protocol_version,
                        } => {
                            assert_eq!(
                                control_protocol_version,
                                control_protocol::CONTROL_PROTOCOL_VERSION
                            );
                        }
                        ControlBody::Initialize { .. } if attempt == 0 => {
                            entered_tx.take().unwrap().send(()).unwrap();
                            // A failed observation may close only this connection.
                            assert_eq!(
                                control_protocol::read_frame(&mut stream).await.unwrap(),
                                None,
                                "timeout/drop must not cancel, close, or delete the producer turn"
                            );
                            disconnected_tx.take().unwrap().send(()).unwrap();
                            break;
                        }
                        ControlBody::Initialize { .. } => {
                            control_protocol::write_frame(
                                &mut stream,
                                &ControlBody::Initialized {
                                    result: serde_json::json!({
                                        "protocolVersion": 1,
                                        "agentCapabilities": {}
                                    }),
                                },
                            )
                            .await
                            .unwrap();
                        }
                        ControlBody::ResumeSession => {
                            control_protocol::write_frame(
                                &mut stream,
                                &ControlBody::SessionReady {
                                    acp_session_id: native_id.clone(),
                                    result: serde_json::json!({}),
                                },
                            )
                            .await
                            .unwrap();
                            completion_rx.take().unwrap().await.unwrap();
                            control_protocol::write_frame(
                                &mut stream,
                                &ControlBody::Notify {
                                    method: "session/update".into(),
                                    params: serde_json::json!({
                                        "sessionId": native_id,
                                        "update": {
                                            "sessionUpdate": "agent_message_chunk",
                                            "content": {
                                                "type": "text",
                                                "text": "original producer completed"
                                            }
                                        }
                                    }),
                                },
                            )
                            .await
                            .unwrap();
                            control_protocol::write_frame(
                                &mut stream,
                                &ControlBody::PromptCompleted {
                                    prompt_req_id: 77,
                                    outcome: PromptOutcome::Completed {
                                        stop_reason: Some("end_turn".into()),
                                    },
                                },
                            )
                            .await
                            .unwrap();
                        }
                        frame => panic!("unexpected attach request: {frame:?}"),
                    }
                }
            }
        });
        Self {
            record,
            identity,
            entered: Some(entered),
            disconnected: Some(disconnected),
            completion: Some(completion),
            peer,
            child,
            _socket_dir: socket_dir,
        }
    }

    pub(crate) async fn entered(&mut self) {
        self.entered.take().unwrap().await.unwrap();
    }

    pub(crate) async fn disconnected(&mut self) {
        self.disconnected.take().unwrap().await.unwrap();
    }

    pub(crate) fn complete_turn(&mut self) {
        self.completion.take().unwrap().send(()).unwrap();
    }
}

impl Drop for HeldAttachRunner {
    fn drop(&mut self) {
        self.peer.abort();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::AcpWorkerState;
    use tokio::sync::oneshot;

    #[tokio::test]
    #[serial_test::serial]
    async fn stop_request_observes_a_gated_respawn_install_as_running() {
        let supervisor = Arc::new(Supervisor::new(VecSink::new()));
        let id = "gated-respawn-install".to_owned();
        supervisor.test_install_stdio(&id).await;
        let respawning_lease = {
            let mut workers = supervisor.workers.lock().await;
            let handle = workers.get_mut(&id).expect("test worker");
            let (lease, _) = lock_recover(&supervisor.lifecycle)
                .begin_respawn(&handle.lease)
                .expect("running worker can begin a respawn");
            handle.lease = lease.clone();
            lease
        };
        assert_eq!(supervisor.worker_state(&id).await, AcpWorkerState::Resuming);

        let workers_gate = supervisor.workers.lock().await;
        let (install_started_tx, install_started_rx) = oneshot::channel();
        let installing = {
            let supervisor = Arc::clone(&supervisor);
            let id = id.clone();
            tokio::spawn(async move {
                let (client, _events) = AcpClient::fake_for_test(AcpSessionId(format!("acp-{id}")));
                install_started_tx.send(()).expect("test is waiting");
                let mut workers = supervisor.workers.lock().await;
                lock_recover(&supervisor.lifecycle)
                    .install(&respawning_lease, None)
                    .expect("the gated respawn installs before stop admission");
                let handle = workers.get_mut(&id).expect("test worker");
                handle.client = Arc::new(client);
                handle.lease = respawning_lease;
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(3), install_started_rx)
            .await
            .expect("respawn reaches its gated install")
            .expect("install task remains alive");

        let stopping = {
            let supervisor = Arc::clone(&supervisor);
            let id = id.clone();
            tokio::spawn(async move {
                supervisor
                    .begin_stop_request(&id, "user_stopped", None, false)
                    .await
            })
        };
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        while !lock_recover(&supervisor.spawn_requests)
            .stop_fences
            .contains_key(&id)
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "stop request did not register its fence"
            );
            tokio::task::yield_now().await;
        }
        drop(workers_gate);

        tokio::time::timeout(std::time::Duration::from_secs(3), installing)
            .await
            .expect("respawn install completes")
            .expect("install task does not panic");
        let stop_request = tokio::time::timeout(std::time::Duration::from_secs(3), stopping)
            .await
            .expect("stop request observes the installed worker")
            .expect("stop request task does not panic");
        assert_eq!(supervisor.worker_state(&id).await, AcpWorkerState::Running);
        assert!(!stop_request.cancelled_launch());
        drop(stop_request);
        supervisor.test_remove_worker(&id).await;
    }
}
