//! Bringing a worker up: admission, spawn, attach, and the launch environment.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

use super::agents::{
    apply_agent_command_override, log_wrapper_substitution, wrapper_substitution_for,
};
use super::publish::collect_resumable_background_agent_launches;
use super::teardown::tear_down_runner;
use super::{
    lock_recover, BroadcastSink, ResumeKind, ResumeReservation, ResumeReservationOutcome,
    SpawnRequest, Supervisor, SupervisorError, WorkerHandle, WorkerKind,
};
use crate::acp::acp_client::{AcpClient, AcpError, SpawnConfig};
use crate::acp::agent_policy::AgentPolicy;
use crate::acp::runner_lifecycle::{AdmitError, InstallError, Lease, RunnerIdentity};
use crate::acp::state::{AcpSessionId, Event};
use crate::process::worker_registry;
use crate::session::config::repo_config::resolve_config_with_repo_or_warn;
use crate::session::SandboxInfo;

impl<S: BroadcastSink> Supervisor<S> {
    /// Spawn a structured view worker for the given session.
    pub async fn spawn(&self, req: SpawnRequest) -> Result<(), SupervisorError> {
        match self
            .begin_resume(&req.session_id, ResumeKind::Spawn)
            .await?
        {
            ResumeReservationOutcome::Reserved(r) => self.spawn_inner(req, r).await,
            ResumeReservationOutcome::AlreadyPresent => {
                Err(SupervisorError::AlreadyRunning(req.session_id))
            }
        }
    }

    /// Admit a resume and reserve its capacity slot; only spawns count, since
    /// an attach takes over a runner the registry already counts.
    pub(crate) async fn begin_resume(
        &self,
        session_id: &str,
        kind: ResumeKind,
    ) -> Result<ResumeReservationOutcome, SupervisorError> {
        // Held so a concurrent shutdown observes the lease or its absence, never a torn state.
        let _workers = self.workers.lock().await;
        let mut table = lock_recover(&self.lifecycle);
        let lease = match table.admit(session_id, kind) {
            Ok(lease) => lease,
            Err(AdmitError::AlreadyPresent) => return Ok(ResumeReservationOutcome::AlreadyPresent),
            Err(AdmitError::TeardownPending) => {
                return Err(SupervisorError::TeardownPending(session_id.to_string()))
            }
            Err(AdmitError::Cancelled(reason)) => {
                drop(table);
                drop(_workers);
                self.publish_next(session_id, &Event::Stopped { reason });
                return Err(SupervisorError::SpawnCancelled(session_id.to_string()));
            }
        };
        if matches!(kind, ResumeKind::Spawn) {
            let registry_count = worker_registry::list()
                .map(|recs| {
                    recs.into_iter()
                        .filter(|r| {
                            worker_registry::is_record_live(r)
                                && table.counts_registry_record(&r.session_id)
                        })
                        .count()
                })
                .unwrap_or(0);
            let combined = table.occupied_slots() + registry_count;
            if combined > self.max_concurrent_workers as usize {
                table.abandon(&lease);
                return Err(SupervisorError::CapacityFull {
                    current: combined - 1,
                    limit: self.max_concurrent_workers,
                });
            }
        }
        Ok(ResumeReservationOutcome::Reserved(ResumeReservation {
            lease,
            lifecycle: Arc::clone(&self.lifecycle),
            notify: Arc::clone(&self.worker_notify),
        }))
    }

    /// Spawn body, run under the reservation from `begin_resume`.
    pub(crate) async fn spawn_inner(
        &self,
        req: SpawnRequest,
        reservation: ResumeReservation,
    ) -> Result<(), SupervisorError> {
        let lease = reservation.lease().clone();
        let session_id = req.session_id.as_str();
        let warmup_guard = self.warmup_guard(&req.agent).await;
        let config = self.spawn_config(&req, lease.epoch()).await?;
        debug!(
            target: "acp.supervisor",
            session = %session_id,
            stored_id = ?req.stored_acp_session_id,
            "spawning structured view worker"
        );
        // Clear a partial replay from a failed import before session/load re-emits it.
        if req.seed_history_replay {
            self.sink.clear_session_events(session_id);
        }

        let launched = (self.launcher)(config.clone(), AcpSessionId(session_id.to_string())).await;
        let mut client = match launched {
            Ok(c) => c,
            Err(err) => {
                // A stop that landed during the launch owns the outcome.
                let cancelled = lock_recover(&self.lifecycle)
                    .cancel_requested(&lease)
                    .is_some();
                if cancelled {
                    self.reap_failed_launch(&lease).await;
                    return Err(SupervisorError::SpawnCancelled(req.session_id));
                }
                if matches!(err, AcpError::IncompatibleAgent(_)) {
                    self.mark_incompatible_binary(session_id, &config.spec.command);
                }
                publish_rejection(&err, |event| {
                    self.publish_next(session_id, &event);
                });
                self.reap_failed_launch(&lease).await;
                return Err(SupervisorError::Acp(err));
            }
        };

        if warmup_guard.is_some() {
            lock_recover(&self.warmed_up_agents).insert(req.agent.clone());
        }
        drop(warmup_guard);
        info!(target: "acp.supervisor", session = %session_id, "structured view worker spawned");
        lock_recover(&self.incompatible_binaries).remove(session_id);

        let inbound = client
            .take_inbound()
            .expect("freshly spawned AcpClient always has inbound receiver");
        let identity = client.runner_pid().map(|pid| RunnerIdentity {
            pid,
            generation: lease.epoch(),
        });
        let kind = WorkerKind::Runner {
            spawn_config: Box::new(config),
        };
        let client = self
            .install_worker(session_id, reservation, client, inbound, identity, kind)
            .await?;

        if req.acp_mode_id.is_some() || req.yolo_mode {
            let mode_id = req
                .acp_mode_id
                .as_deref()
                .or_else(|| crate::acp::agent_profiles::resolve(&req.agent).yolo_mode_id);
            apply_mode(&client, session_id, mode_id, "spawn").await;
        }
        Ok(())
    }

    /// Serializes an agent's spawns until its first one finishes.
    async fn warmup_guard(&self, agent: &str) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        if lock_recover(&self.warmed_up_agents).contains(agent) {
            return None;
        }
        let lock = lock_recover(&self.agent_warmup_locks)
            .entry(agent.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        Some(lock.lock_owned().await)
    }

    async fn spawn_config(
        &self,
        req: &SpawnRequest,
        generation: u64,
    ) -> Result<SpawnConfig, SupervisorError> {
        let profile = req.source_profile.clone().unwrap_or_default();
        let cwd = req.cwd.clone();
        let (resolved_cfg, policy) = tokio::task::spawn_blocking(move || {
            (
                resolve_config_with_repo_or_warn(&profile, &cwd),
                AgentPolicy::load(),
            )
        })
        .await
        .map_err(|e| {
            SupervisorError::InvalidAgentCommand(format!("config load task failed: {e}"))
        })?;
        let (mut spec, spec_from_registry) = self
            .resolve_agent_spec(&req.agent, &resolved_cfg.session, &policy)
            .await?;
        if let Some(ovr) = &req.agent_command_override {
            apply_agent_command_override(&req.agent, spec_from_registry, ovr, &mut spec)?;
        }
        let wrapper_substitution = wrapper_substitution_for(
            &*self.registry.lock().await,
            &req.tool,
            &req.agent,
            spec_from_registry,
            &resolved_cfg.session.agent_detect_as,
        );
        if let Some((wrapper, base)) = &wrapper_substitution {
            log_wrapper_substitution(&req.session_id, &req.tool, wrapper, base);
        }
        if spec.command.contains("${aoe_data_dir}") {
            if let Ok(data_dir) = crate::session::get_app_dir() {
                spec.command = spec
                    .command
                    .replace("${aoe_data_dir}", &data_dir.to_string_lossy());
            }
        }

        let acp_defaults = resolved_cfg.acp.acp_defaults_for(&req.agent);
        let (model, effort) = crate::session::config::resolve_spawn_model_effort(
            acp_defaults,
            req.model.clone(),
            req.effort.clone(),
        );

        let mut host_environment = Vec::new();
        if req.sandbox_info.is_none() {
            // Trusted global/profile configuration; repo overrides cannot contribute it.
            host_environment = crate::session::environment::resolve_host_environment_pairs(
                &resolved_cfg.environment,
            );
            if !resolved_cfg.host_hooks.before_session.is_empty() {
                let minted = before_session_env(
                    &req.session_id,
                    &req.tool,
                    req.source_profile.clone().unwrap_or_default(),
                    req.cwd.clone(),
                )
                .await
                .map_err(|e| {
                    SupervisorError::InvalidAgentCommand(format!(
                        "before_session hook task failed: {e}"
                    ))
                })?
                .map_err(|e| {
                    SupervisorError::Acp(AcpError::Spawn(format!("before_session hook: {e}")))
                })?;
                overlay_env(&mut host_environment, minted);
            }
        }

        let mut provider_env = req.provider_env.clone();
        if let Some(model) = model.clone() {
            provider_env.push(("AOE_AGENT_MODEL".into(), model));
        }
        // Every worker runs through `aoe __acp-runner` so it survives `aoe serve --stop`.
        let socket_path = worker_registry::socket_path_for(&req.session_id).map_err(|e| {
            SupervisorError::Acp(AcpError::Spawn(format!("worker socket path: {e}")))
        })?;
        let mcp_servers = resolve_mcp_servers(
            &req.agent,
            &req.session_id,
            req.source_profile.clone(),
            req.cwd.clone(),
            host_environment.clone(),
            "MCP resolution task failed",
        )
        .await;

        Ok(SpawnConfig {
            agent_key: req.agent.clone(),
            tool: req.tool.clone(),
            spec,
            cwd: req.cwd.clone(),
            additional_dirs: req.additional_dirs.clone(),
            provider_env,
            host_environment,
            default_effort: effort,
            default_effort_explicit: req.effort_explicit,
            default_mode: acp_defaults.and_then(|defaults| defaults.mode()),
            default_model: model,
            socket_path: Some(socket_path),
            stored_acp_session_id: req.stored_acp_session_id.clone(),
            fork_from: req.fork_from.clone(),
            sandbox_info: req.sandbox_info.clone(),
            source_profile: req.source_profile.clone(),
            mcp_servers,
            seed_history_replay: req.seed_history_replay,
            artifact_dir: crate::session::artifacts::session_artifact_dir(&req.session_id).ok(),
            wrapper_substitution,
            generation,
        })
    }

    /// Install a launched client under the reservation's lease, or retire it
    /// when a stop or a newer epoch won the race. Returns the installed client.
    async fn install_worker(
        &self,
        session_id: &str,
        reservation: ResumeReservation,
        client: AcpClient,
        inbound: mpsc::Receiver<Event>,
        identity: Option<RunnerIdentity>,
        kind: WorkerKind,
    ) -> Result<Arc<AcpClient>, SupervisorError> {
        let lease = reservation.lease().clone();
        let client = Arc::new(client);
        let mut workers = self.workers.lock().await;
        let install = lock_recover(&self.lifecycle).install(&lease, identity);
        if let Err(refusal) = install {
            drop(workers);
            let _ = client.shutdown().await;
            drop(client);
            return Err(self.retire_refused_install(&lease, identity, refusal).await);
        }
        // Retire the previous worker's requests before this worker's events
        // publish; once the drain starts, this worker's own are in the log and
        // the sweep can no longer tell them apart. Background sub-agents split
        // by kind: a replaced worker took its tailers with it, so nothing will
        // ever report their outcome, while an attached worker is provably
        // alive and whatever still has a transcript resumes instead (#4001).
        // Only the queries and their publishes belong before the drain; the
        // resume send is a real await, so it runs after `workers` is dropped.
        self.cancel_orphaned_requests(session_id);
        let resumable = if matches!(kind, WorkerKind::Attached) {
            collect_resumable_background_agent_launches(&*self.sink, &self.next_seqs, session_id)
        } else {
            self.detach_orphaned_background_agents(session_id);
            Vec::new()
        };
        let drain_task = self.start_drain_task(session_id.to_string(), lease.clone(), inbound);
        let client_for_resume = (!resumable.is_empty()).then(|| Arc::clone(&client));
        workers.insert(
            session_id.to_string(),
            WorkerHandle {
                client: Arc::clone(&client),
                drain_task,
                restart_history: vec![],
                kind,
                lease,
            },
        );
        drop(workers);
        if let Some(resuming) = client_for_resume {
            info!(
                target: "acp.supervisor",
                session = %session_id,
                resumed = resumable.len(),
                "resuming background sub-agent tailing after daemon restart"
            );
            // Swallowed deliberately: a send failure proves only that the
            // client connection task died, not the runner, and propagating it
            // routes into the caller's fresh-spawn fallback, which terminates
            // a runner that may still be serving. The drain task shares this
            // channel, so it runs its own closed-inbound handling and
            // `readopt_orphan_runners` reattaches on the next tick.
            let _ = resuming.resume_background_tailing(resumable).await;
        }
        drop(reservation);
        self.worker_notify.notify_waiters();
        Ok(client)
    }

    /// Retire a runner a failed spawn left behind under this lease's generation.
    async fn reap_failed_launch(&self, lease: &Lease) {
        let session_id = lease.session_id();
        let Ok(Some(record)) = worker_registry::load(session_id) else {
            return;
        };
        if record.generation != lease.epoch() {
            return;
        }
        let identity = RunnerIdentity {
            pid: record.pid,
            generation: record.generation,
        };
        if !lock_recover(&self.lifecycle).convert_to_stopping(lease) {
            return;
        }
        let settlement = tear_down_runner(&*self.process_control, session_id, Some(identity)).await;
        self.settle(lease, settlement);
    }

    /// Tear down a built worker whose install the lifecycle table refused.
    async fn retire_refused_install(
        &self,
        lease: &Lease,
        identity: Option<RunnerIdentity>,
        refusal: InstallError,
    ) -> SupervisorError {
        let session_id = lease.session_id();
        let settlement = tear_down_runner(&*self.process_control, session_id, identity).await;
        match refusal {
            InstallError::Cancelled { reason } => {
                debug!(
                    target: "acp.supervisor",
                    session = %session_id,
                    %reason,
                    "resume cancelled by a concurrent shutdown; runner torn down"
                );
                self.settle(lease, settlement);
                self.publish_next(session_id, &Event::Stopped { reason });
            }
            InstallError::Stale => {
                warn!(
                    target: "acp.supervisor",
                    session = %session_id,
                    "resume completed under a stale lease; runner torn down"
                );
                self.worker_notify.notify_waiters();
            }
        }
        SupervisorError::SpawnCancelled(session_id.to_string())
    }

    /// Reattach to a runner left by a previous daemon by dialing its socket.
    pub async fn attach(
        &self,
        session_id: String,
        cwd: PathBuf,
        additional_dirs: Vec<PathBuf>,
        in_flight_turn: bool,
        sandbox: Option<SandboxInfo>,
    ) -> Result<(), SupervisorError> {
        match self.begin_resume(&session_id, ResumeKind::Attach).await? {
            ResumeReservationOutcome::Reserved(r) => {
                self.attach_inner(session_id, cwd, additional_dirs, in_flight_turn, sandbox, r)
                    .await
            }
            ResumeReservationOutcome::AlreadyPresent => {
                Err(SupervisorError::AlreadyRunning(session_id))
            }
        }
    }

    /// Attach body, run under a reservation from `begin_resume`.
    pub(crate) async fn attach_inner(
        &self,
        session_id: String,
        cwd: PathBuf,
        additional_dirs: Vec<PathBuf>,
        in_flight_turn: bool,
        sandbox: Option<SandboxInfo>,
        reservation: ResumeReservation,
    ) -> Result<(), SupervisorError> {
        let record = match worker_registry::load(&session_id)
            .map_err(|e| SupervisorError::Acp(AcpError::Spawn(format!("registry load: {e}"))))?
        {
            Some(r) if worker_registry::is_record_live(&r) => r,
            _ => return Err(SupervisorError::UnknownSession(session_id)),
        };
        let identity = RunnerIdentity {
            pid: record.pid,
            generation: record.generation,
        };
        lock_recover(&self.lifecycle).note_generation(&session_id, record.generation);

        let agent_key = if record.agent_key.is_empty() {
            record.agent_name.clone()
        } else {
            record.agent_key.clone()
        };
        let key = agent_key.clone();
        let agent_allowed = tokio::task::spawn_blocking(move || AgentPolicy::load().allows(&key))
            .await
            .map_err(|e| {
                SupervisorError::InvalidAgentCommand(format!("agent policy load task failed: {e}"))
            })?;
        if !agent_allowed {
            warn!(
                target: "acp.supervisor",
                session = %session_id,
                agent = %agent_key,
                "detached structured view worker runs an agent that [acp] allowed_agents no longer \
                 permits; terminating it instead of reattaching"
            );
            let id = session_id.clone();
            if let Err(e) =
                tokio::task::spawn_blocking(move || worker_registry::terminate(&id)).await
            {
                warn!(
                    target: "acp.supervisor",
                    session = %session_id,
                    "terminate task for a disallowed worker failed: {e}; the next \
                     reconciler tick retries"
                );
            }
            return Err(SupervisorError::AgentNotAllowed(agent_key));
        }

        let Some(stored_acp_session_id) = record.stored_acp_session_id.clone() else {
            return Err(SupervisorError::Acp(AcpError::Spawn(
                "runner registry has no stored_acp_session_id; need fresh spawn".into(),
            )));
        };
        let sandbox_resources = match sandbox {
            Some(info) => {
                let cwd = cwd.clone();
                let profile = record.source_profile.clone();
                Some(
                    tokio::task::spawn_blocking(move || {
                        crate::acp::acp_client::SessionSandbox::from_info(
                            &info,
                            cwd.as_path(),
                            profile,
                        )
                    })
                    .await
                    .map_err(|e| {
                        AcpError::Spawn(format!("sandbox resolve task panicked: {e}"))
                    })??,
                )
            }
            None => None,
        };
        let mut client = AcpClient::attach(
            record.socket_path.clone(),
            cwd,
            additional_dirs,
            stored_acp_session_id,
            in_flight_turn,
            AcpSessionId(session_id.clone()),
            sandbox_resources,
            agent_key,
            record.source_profile.clone(),
        )
        .await?;

        let inbound = client
            .take_inbound()
            .expect("freshly attached AcpClient always has inbound receiver");
        self.install_worker(
            &session_id,
            reservation,
            client,
            inbound,
            Some(identity),
            WorkerKind::Attached,
        )
        .await?;
        info!(
            target: "acp.supervisor",
            session = %session_id,
            socket = %record.socket_path.display(),
            pid = record.pid,
            "reattached to existing structured view worker"
        );
        Ok(())
    }
}

pub(super) async fn apply_mode(
    client: &AcpClient,
    session_id: &str,
    mode_id: Option<&str>,
    after: &str,
) {
    let Some(mode_id) = mode_id else {
        return;
    };
    if let Err(e) = client.set_mode(mode_id).await {
        warn!(
            target: "acp.supervisor",
            session = %session_id,
            "set_mode({mode_id}) after {after} failed: {e}"
        );
    }
}

/// Publish what a launch rejection means for the session; false for any other error.
pub(super) fn publish_rejection(err: &AcpError, mut publish: impl FnMut(Event)) -> bool {
    match err {
        AcpError::IncompatibleAgent(payload) => {
            publish(Event::IncompatibleAgent {
                detail: payload.detail.clone(),
            });
            publish(Event::AgentStartupError {
                message: payload.message.clone(),
            });
        }
        // The provider limit parks the session rather than reporting a crash.
        AcpError::RateLimited(info) => {
            publish(Event::RateLimit {
                info: (**info).clone(),
            });
            publish(Event::Stopped {
                reason: "rate_limited".into(),
            });
        }
        _ => return false,
    }
    true
}

/// Run the profile's `before_session` host hooks and return the env they mint.
pub(super) async fn before_session_env(
    session_id: &str,
    tool: &str,
    profile: String,
    cwd: PathBuf,
) -> Result<anyhow::Result<Vec<(String, String)>>, tokio::task::JoinError> {
    let session_id = session_id.to_string();
    let tool = tool.to_string();
    tokio::task::spawn_blocking(move || {
        use crate::session::config::repo_config::{
            resolve_before_session_hooks, run_before_session_hooks,
        };
        let commands = resolve_before_session_hooks(&profile);
        if commands.is_empty() {
            return Ok(Vec::new());
        }
        let hook_env: Vec<(&'static str, String)> = vec![
            ("AOE_SESSION_ID", session_id),
            ("AOE_PROFILE", profile.clone()),
            ("AOE_TOOL", tool),
            ("AOE_PROJECT_PATH", cwd.to_string_lossy().to_string()),
        ];
        run_before_session_hooks(&commands, &cwd, &hook_env, &[])
    })
    .await
}

pub(super) fn overlay_env(env: &mut Vec<(String, String)>, minted: Vec<(String, String)>) {
    for (key, value) in minted {
        env.retain(|(k, _)| k != &key);
        env.push((key, value));
    }
}

pub(super) async fn resolve_mcp_servers(
    agent_key: &str,
    session_id: &str,
    profile: Option<String>,
    cwd: PathBuf,
    session_env: Vec<(String, String)>,
    failure: &'static str,
) -> Vec<agent_client_protocol::schema::v1::McpServer> {
    let agent_key = agent_key.to_string();
    let session = session_id.to_string();
    tokio::task::spawn_blocking(move || {
        resolve_mcp_layers(&agent_key, &session, profile.as_deref(), &cwd, &session_env)
    })
    .await
    .unwrap_or_else(|e| {
        warn!(
            target: "acp.mcp",
            session = %session_id,
            error = %e,
            "{failure}; forwarding no servers"
        );
        Vec::new()
    })
}

fn resolve_mcp_layers(
    agent_key: &str,
    session_id: &str,
    profile: Option<&str>,
    cwd: &std::path::Path,
    session_env: &[(String, String)],
) -> Vec<agent_client_protocol::schema::v1::McpServer> {
    use crate::session::mcp::mcp_model::{resolve_effective, summarize};

    let merged = resolve_effective(agent_key, profile, cwd, session_env);
    if !merged.is_empty() {
        info!(
            target: "acp.mcp",
            session = %session_id,
            count = merged.len(),
            servers = %summarize(&merged),
            "forwarding MCP servers"
        );
    }
    crate::acp::mcp_config::project_servers_to_acp(merged.into_iter().map(|s| s.def).collect())
}

/// Apply current model pins and effort defaults to a cached respawn config,
/// keeping an explicit effort.
pub(super) fn refresh_spawn_model_effort(
    config: &mut SpawnConfig,
    defaults: Option<&crate::session::config::AcpAgentDefaults>,
) {
    let cached_model = config
        .provider_env
        .iter()
        .find(|(key, _)| key == "AOE_AGENT_MODEL")
        .map(|(_, value)| value.clone());
    let explicit_effort = if config.default_effort_explicit {
        config.default_effort.take()
    } else {
        None
    };
    let (model, effort) =
        crate::session::config::resolve_spawn_model_effort(defaults, cached_model, explicit_effort);
    set_spawn_model(config, model);
    config.default_effort = effort;
}

/// Point both model channels of a cached respawn config at `model`.
pub(super) fn set_spawn_model(config: &mut SpawnConfig, model: Option<String>) {
    config
        .provider_env
        .retain(|(key, _)| key != "AOE_AGENT_MODEL");
    if let Some(model) = model.clone() {
        config.provider_env.push(("AOE_AGENT_MODEL".into(), model));
    }
    config.default_model = model;
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::WorkerKind;
    use super::*;
    use crate::acp::approvals::{ApprovalDecision, Nonce};
    use crate::acp::runner_lifecycle::test_support::FakeProcessControl;
    use crate::daemon::AcpWorkerState;

    #[test]
    fn respawn_refreshes_the_model_pin_and_keeps_explicit_effort() {
        use crate::session::config::AcpAgentDefaults;
        let pin = |model: &str| AcpAgentDefaults {
            model: Some(model.into()),
            pin_model: true,
            effort: Some("low".into()),
            effort_by_model: [("model-b".to_string(), "high".to_string())].into(),
            ..Default::default()
        };
        let unpinned = AcpAgentDefaults {
            model: Some("model-b".into()),
            effort: Some("low".into()),
            ..Default::default()
        };
        // (name, cached effort explicit, defaults, want model, want effort)
        let cases = [
            (
                "pin moved to b",
                false,
                Some(pin("model-b")),
                "model-b",
                Some("high"),
            ),
            (
                "pin still a",
                false,
                Some(pin("model-a")),
                "model-a",
                Some("low"),
            ),
            ("no entry", false, None, "model-a", None),
            (
                "plain default",
                false,
                Some(unpinned),
                "model-a",
                Some("low"),
            ),
            (
                "explicit effort survives a pin move",
                true,
                Some(pin("model-b")),
                "model-b",
                Some("low"),
            ),
        ];
        for (name, explicit, defaults, want_model, want_effort) in cases {
            let mut config = runner_config(std::env::temp_dir().join("unused.sock"));
            config.provider_env = vec![
                ("AOE_AGENT_MODEL".into(), "model-a".into()),
                ("OTHER".into(), "kept".into()),
            ];
            config.default_effort = Some("low".into());
            config.default_effort_explicit = explicit;
            refresh_spawn_model_effort(&mut config, defaults.as_ref());
            let models: Vec<&str> = config
                .provider_env
                .iter()
                .filter(|(key, _)| key == "AOE_AGENT_MODEL")
                .map(|(_, value)| value.as_str())
                .collect();
            assert_eq!(models, [want_model], "{name}");
            assert_eq!(config.default_effort.as_deref(), want_effort, "{name}");
            assert!(
                config
                    .provider_env
                    .contains(&("OTHER".into(), "kept".into())),
                "{name}"
            );
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn spawn_does_not_rederive_effort_provenance_from_the_value() {
        let _home = isolate_home();
        let control = Arc::new(FakeProcessControl::default());
        control.alive(4343);
        let gate = Gate::default();
        let sup = Arc::new(
            Supervisor::new(VecSink::new())
                .with_process_control(control)
                .with_launcher(gated_launcher(&gate, 4343)),
        );
        let mut req = spawn_request("s-prov");
        req.effort = Some("low".into());

        let spawner = {
            let sup = Arc::clone(&sup);
            tokio::spawn(async move { sup.spawn(req).await })
        };
        gate.entered.notified().await;
        gate.open.notify_one();
        spawner.await.unwrap().expect("spawn");

        let explicit = sup
            .workers
            .lock()
            .await
            .get("s-prov")
            .map(|handle| match &handle.kind {
                WorkerKind::Runner { spawn_config } => spawn_config.default_effort_explicit,
                _ => panic!("runner handle expected"),
            })
            .expect("worker installed");
        assert!(
            !explicit,
            "a resolved default effort must not read as a session pin"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn spawn_rejects_unknown_agents_and_running_sessions() {
        let _home = isolate_home();
        let sup = Supervisor::new(VecSink::new());
        let mut req = spawn_request("s-1");
        req.agent = "no-such-agent".into();
        assert!(matches!(
            sup.spawn(req).await,
            Err(SupervisorError::UnknownAgent(_))
        ));

        sup.test_insert_worker("s-1").await;
        assert!(matches!(
            sup.spawn(spawn_request("s-1")).await,
            Err(SupervisorError::AlreadyRunning(_))
        ));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn capacity_counts_workers_detached_runners_and_pending_spawns() {
        let (_home, _tmp) = isolate_home();

        let sup = Supervisor::with_capacity(VecSink::new(), 1);
        sup.test_insert_worker("s-1").await;
        match sup.spawn(spawn_request("s-2")).await {
            Err(SupervisorError::CapacityFull { current, limit }) => {
                assert_eq!((current, limit), (1, 1));
            }
            other => panic!("in-memory worker: expected CapacityFull, got {other:?}"),
        }

        let sup = Supervisor::with_capacity(VecSink::new(), 1);
        let socket = worker_registry::workers_dir()
            .unwrap()
            .join("detached-1.sock");
        worker_registry::touch_live_socket(&socket);
        let record = worker_record("detached-1", std::process::id(), socket);
        worker_registry::save(&record).unwrap();
        assert!(worker_registry::is_record_live(&record));
        match sup.spawn(spawn_request("fresh")).await {
            Err(SupervisorError::CapacityFull { current, limit }) => {
                assert_eq!(
                    (current, limit),
                    (1, 1),
                    "detached registry entry must count"
                );
            }
            other => panic!("detached runner: expected CapacityFull, got {other:?}"),
        }
        worker_registry::delete("detached-1").unwrap();

        let sup = Supervisor::with_capacity(VecSink::new(), 2);
        let _a = reserve(sup.begin_resume("s-a", ResumeKind::Spawn).await);
        let _attach = reserve(sup.begin_resume("s-attach", ResumeKind::Attach).await);
        let _b = reserve(sup.begin_resume("s-b", ResumeKind::Spawn).await);
        match sup.begin_resume("s-c", ResumeKind::Spawn).await {
            Err(SupervisorError::CapacityFull { current, limit }) => {
                assert_eq!(
                    (current, limit),
                    (2, 2),
                    "in-flight spawns hold a slot, an attach does not"
                );
            }
            Err(other) => panic!("expected CapacityFull, got {other:?}"),
            Ok(_) => panic!("expected CapacityFull, got an admission"),
        }
        assert_eq!(
            sup.worker_state("s-c").await,
            AcpWorkerState::Absent,
            "a refused admission leaves nothing behind"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn resolve_mcp_layers_merges_native_global_profile_and_trusted_project() {
        let (_home, tmp) = isolate_home();
        let _env = crate::session::test_support::EnvGuard::unset(&["CLAUDE_CONFIG_DIR"]);
        let write = |path: std::path::PathBuf, servers: &str| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, format!(r#"{{ "mcpServers": {{ {servers} }} }}"#)).unwrap();
        };
        write(
            tmp.path().join(".claude.json"),
            r#""native-only": { "command": "n" }, "shared": { "command": "from-native" }"#,
        );
        write(
            crate::session::get_app_dir().unwrap().join("mcp.json"),
            r#""global-only": { "command": "g" }, "shared": { "command": "from-global" }"#,
        );
        write(
            crate::session::get_profile_dir_path("work")
                .unwrap()
                .join("mcp.json"),
            r#""profile-only": { "command": "p" }, "shared": { "command": "from-profile" }"#,
        );
        let repo = tmp.path().join("repo");
        write(
            repo.join(".mcp.json"),
            r#""project-only": { "command": "pl" }, "shared": { "command": "from-project" }"#,
        );

        let resolve = |profile: Option<&'static str>, cwd: std::path::PathBuf| async move {
            let merged = tokio::task::spawn_blocking(move || {
                resolve_mcp_layers("claude", "resolve-test", profile, &cwd, &[])
            })
            .await
            .unwrap();
            let val = serde_json::to_value(&merged).unwrap();
            let mut names: Vec<String> = val
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s["name"].as_str().unwrap().to_string())
                .collect();
            names.sort();
            let shared = val
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["name"] == "shared")
                .map(|s| s["command"].as_str().unwrap().to_string());
            (names, shared)
        };

        let (names, shared) = resolve(Some("work"), tmp.path().to_path_buf()).await;
        assert_eq!(
            names,
            ["global-only", "native-only", "profile-only", "shared"],
            "native + global + profile union"
        );
        assert_eq!(shared.as_deref(), Some("from-profile"), "profile wins");

        // A profile with no mcp.json, so the project-local trust gate is read
        // against the global layer: with no profile argument at all,
        // `resolve_default_profile` would pick "work" (the only profile on
        // disk) and its `shared` would win.
        let (names, shared) = resolve(Some("empty"), repo.clone()).await;
        assert!(
            !names.contains(&"project-only".to_string()),
            "untrusted project is skipped"
        );
        assert_eq!(shared.as_deref(), Some("from-global"));

        let servers = crate::session::mcp::project_mcp::load_project_mcp_servers(&repo).unwrap();
        let hash = crate::session::mcp::project_mcp::fingerprint(&servers);
        crate::session::config::repo_config::trust_repo(&repo, None, Some(&hash)).unwrap();
        let (names, shared) = resolve(Some("empty"), repo).await;
        assert!(
            names.contains(&"project-only".to_string()),
            "trusted project is forwarded"
        );
        assert_eq!(
            shared.as_deref(),
            Some("from-project"),
            "trusted project wins"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn spawn_retires_old_approval_before_publishing_queued_request() {
        use crate::acp::approvals::Approval;
        use crate::acp::state::ToolCall;

        let (_home, _tmp) = isolate_home();
        let (sink, store, mut rx, _tmp) = channel_sink();
        let approval = |nonce: &str| Approval {
            nonce: Nonce(nonce.into()),
            tool_call: ToolCall {
                id: nonce.into(),
                name: "Bash".into(),
                kind: "execute".into(),
                args_preview: r#"{"command":"pwd"}"#.into(),
                started_at: chrono::Utc::now(),
                parent_tool_call_id: None,
                memory_recall: None,
                diffs: Vec::new(),
            },
            destructive: false,
            options: Vec::new(),
            choice: false,
            requested_at: chrono::Utc::now(),
            resolved: None,
        };
        sink.publish(
            "s-startup",
            1,
            &Event::ApprovalRequested {
                approval: approval("old"),
            },
        );
        let fresh = approval("live");
        let senders: Arc<std::sync::Mutex<Vec<mpsc::Sender<Event>>>> = Default::default();
        let launcher: super::super::Launcher = Arc::new(move |config, session_id| {
            let fresh = fresh.clone();
            let senders = senders.clone();
            Box::pin(async move {
                save_record(&session_id.0, 4345, config.generation);
                let (client, tx) = AcpClient::fake_for_test(session_id);
                tx.send(Event::ApprovalRequested { approval: fresh })
                    .await
                    .unwrap();
                senders.lock().unwrap().push(tx);
                Ok(client.with_runner_pid(4345))
            })
        });
        let control = Arc::new(FakeProcessControl::default());
        control.alive(4345);
        let sup = Supervisor::new(sink)
            .with_process_control(control)
            .with_launcher(launcher);
        sup.hydrate_seqs(store.all_session_seqs());
        sup.spawn(spawn_request("s-startup")).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !store
                .unresolved_approval_nonces("s-startup")
                .contains(&Nonce("live".into()))
            {
                rx.recv().await.unwrap();
            }
        })
        .await
        .expect("queued approval must reach the durable log");
        assert_eq!(
            store.unresolved_approval_nonces("s-startup"),
            vec![Nonce("live".into())]
        );
        let events: Vec<_> = store
            .replay_from("s-startup", 0)
            .into_iter()
            .filter_map(|(_, event)| match event {
                Event::ApprovalRequested { approval } => {
                    Some(format!("requested:{}", approval.nonce.0))
                }
                Event::ApprovalResolved { nonce, decision } => {
                    assert_eq!(decision, ApprovalDecision::Cancelled);
                    Some(format!("cancelled:{}", nonce.0))
                }
                _ => None,
            })
            .collect();
        assert_eq!(events, ["requested:old", "cancelled:old", "requested:live"]);
        sup.shutdown("s-startup").await.unwrap();
    }

    /// A launch left outstanding by a previous daemon (its tailer died with
    /// that daemon, so no completion will ever arrive) gets a synthetic
    /// `Detached` the moment a fresh worker spawns over it, instead of showing
    /// as running forever (#4001).
    #[tokio::test]
    #[serial_test::serial]
    async fn spawn_detaches_background_agent_orphaned_by_previous_daemon() {
        use crate::acp::state::BackgroundAgentStatus;

        let (_home, _tmp) = isolate_home();
        let (sink, store, mut rx, _store_tmp) = channel_sink();
        sink.publish(
            "s-startup",
            1,
            &Event::BackgroundAgentLaunched {
                agent_id: "sub-1".into(),
                tool_call_id: "tc-1".into(),
                description: "do a thing".into(),
                prompt: "do a thing".into(),
                model: "claude".into(),
                output_file: "/tmp/nonexistent.jsonl".into(),
                started_at: chrono::Utc::now(),
            },
        );
        sink.publish(
            "s-startup",
            2,
            &Event::BackgroundAgentProgress {
                agent_id: "sub-1".into(),
                status: BackgroundAgentStatus::Running,
                tool_count: 1,
                tools: Vec::new(),
                last_tool: None,
                last_text: None,
                at: chrono::Utc::now(),
            },
        );
        let launcher: super::super::Launcher = Arc::new(move |config: SpawnConfig, session_id| {
            Box::pin(async move {
                save_record(&session_id.0, 4345, config.generation);
                let (client, _tx) = AcpClient::fake_for_test(session_id);
                Ok(client.with_runner_pid(4345))
            })
        });
        let control = Arc::new(FakeProcessControl::default());
        control.alive(4345);
        let sup = Supervisor::new(sink)
            .with_process_control(control)
            .with_launcher(launcher);
        sup.hydrate_seqs(store.all_session_seqs());
        sup.spawn(spawn_request("s-startup")).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !store
                .unresolved_background_agent_ids("s-startup")
                .is_empty()
            {
                rx.recv().await.unwrap();
            }
        })
        .await
        .expect("stale background agent must be detached in the durable log");
        let detached = store
            .replay_from("s-startup", 0)
            .into_iter()
            .any(|(_, event)| {
                matches!(
                    event,
                    Event::BackgroundAgentCompleted {
                        status: BackgroundAgentStatus::Detached,
                        ..
                    }
                )
            });
        assert!(
            detached,
            "the orphaned launch must be closed out as Detached"
        );
        sup.shutdown("s-startup").await.unwrap();
    }

    /// Everything [`attach_with_orphaned_background_agent`] sets up. The
    /// stand-in runner and its handshake task are reaped by `Drop`, so a
    /// panicking assertion still cleans them up instead of leaking the child
    /// process. The rest exists only to keep the connection alive until the
    /// fixture drops.
    struct AttachBackgroundAgentFixture {
        store: Arc<crate::acp::event_store::EventStore>,
        rx: tokio::sync::broadcast::Receiver<crate::server::AcpBroadcastFrame>,
        _sup: Supervisor<super::super::ChannelSink>,
        runner_handshake: tokio::task::JoinHandle<()>,
        fake_runner: std::process::Child,
        _tmp: tempfile::TempDir,
        _store_tmp: tempfile::TempDir,
        _home: crate::session::test_support::AppDirGuard,
    }

    impl Drop for AttachBackgroundAgentFixture {
        fn drop(&mut self) {
            self.runner_handshake.abort();
            let _ = self.fake_runner.kill();
            let _ = self.fake_runner.wait();
        }
    }

    /// One `attach` fixture for the survivor and untrackable cases below: a
    /// fake runner behind a control socket, an unresolved
    /// `BackgroundAgentLaunched` (+`Progress`) already on disk, and the
    /// connection left by a successful `attach`. `output_file` is the launch
    /// payload's transcript path, empty for the untrackable case.
    async fn attach_with_orphaned_background_agent(
        session_id: &str,
        output_file: &str,
    ) -> AttachBackgroundAgentFixture {
        use crate::acp::control_protocol::{self, ControlBody};
        use crate::acp::state::BackgroundAgentStatus;
        use std::os::unix::process::CommandExt as _;

        let (_home, tmp) = isolate_home();
        let (sink, store, rx, _store_tmp) = channel_sink();
        sink.publish(
            session_id,
            1,
            &Event::BackgroundAgentLaunched {
                agent_id: "sub-1".into(),
                tool_call_id: "tc-1".into(),
                description: "do a thing".into(),
                prompt: "do a thing".into(),
                model: "claude".into(),
                output_file: output_file.into(),
                started_at: chrono::Utc::now(),
            },
        );
        sink.publish(
            session_id,
            2,
            &Event::BackgroundAgentProgress {
                agent_id: "sub-1".into(),
                status: BackgroundAgentStatus::Running,
                tool_count: 1,
                tools: Vec::new(),
                last_tool: None,
                last_text: None,
                at: chrono::Utc::now(),
            },
        );

        // `is_record_live` requires a live pid; `sleep 60` as its own process
        // group leader keeps the cleanup kill off the test process.
        let fake_runner = std::process::Command::new("sleep")
            .arg("60")
            .process_group(0)
            .spawn()
            .expect("spawn stand-in runner");

        let socket = tmp.path().join(format!("{session_id}.sock"));
        let control_socket = crate::process::worker::control_socket_sibling(&socket);
        let listener = tokio::net::UnixListener::bind(&control_socket).unwrap();
        let session_id_owned = session_id.to_string();
        let runner_handshake = tokio::spawn(async move {
            let (mut peer, _) = listener.accept().await.unwrap();
            control_protocol::write_frame(
                &mut peer,
                &ControlBody::Hello {
                    control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
                    session_id: session_id_owned,
                },
            )
            .await
            .unwrap();
            while let Some(frame) = control_protocol::read_frame(&mut peer).await.unwrap() {
                let reply = match frame {
                    ControlBody::Attach { .. } => continue,
                    ControlBody::Initialize { .. } => ControlBody::Initialized {
                        result: serde_json::json!({
                            "protocolVersion": 1, "agentCapabilities": {}
                        }),
                    },
                    ControlBody::ResumeSession => ControlBody::SessionReady {
                        acp_session_id: "acp-sid".into(),
                        result: serde_json::json!({}),
                    },
                    frame => panic!("unexpected attach handshake frame: {frame:?}"),
                };
                control_protocol::write_frame(&mut peer, &reply)
                    .await
                    .unwrap();
            }
        });

        // An agent key absent from the registry resolves to
        // `ExpectedAgent::Other`, skipping the per-adapter compat gate: this
        // fixture tests attach wiring, not agent compatibility.
        let record = worker_registry::WorkerRecord::new(
            session_id.to_string(),
            fake_runner.id(),
            socket,
            "test-agent-acp".into(),
            "test-agent".into(),
            tmp.path().to_path_buf(),
            None,
            vec![],
            vec![],
            Some("acp-sid".into()),
            None,
        );
        worker_registry::save(&record).unwrap();

        let sup = Supervisor::new(sink);
        sup.hydrate_seqs(store.all_session_seqs());
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            sup.attach(
                session_id.to_string(),
                tmp.path().to_path_buf(),
                vec![],
                false,
                None,
            ),
        )
        .await
        .expect("attach must not hang")
        .expect("attach must succeed against the fake runner");

        AttachBackgroundAgentFixture {
            store,
            rx,
            _sup: sup,
            runner_handshake,
            fake_runner,
            _tmp: tmp,
            _store_tmp,
            _home,
        }
    }

    async fn await_background_agent_resolved(
        fixture: &mut AttachBackgroundAgentFixture,
        session_id: &str,
        within: std::time::Duration,
        what: &str,
    ) -> crate::acp::state::AcpState {
        use crate::acp::state::{AcpSessionId, AcpState, AgentName};

        tokio::time::timeout(within, async {
            while !fixture
                .store
                .unresolved_background_agent_ids(session_id)
                .is_empty()
            {
                match fixture.rx.recv().await {
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(e) => panic!("broadcast channel closed: {e}"),
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{what}"));

        let mut state = AcpState::new(
            AcpSessionId(session_id.into()),
            AgentName("claude".into()),
            None,
        );
        for (_, event) in fixture.store.replay_from(session_id, 0) {
            state.apply_event(event).unwrap();
        }
        state
    }

    /// The primary daemon-restart path: the worker `attach` just reached is
    /// provably alive, so an orphaned launch with a real transcript resumes
    /// tailing instead of being eagerly detached, and the tailer picks up the
    /// `end_turn` record already on disk (#4029).
    #[tokio::test]
    #[serial_test::serial]
    async fn attach_resumes_tailing_a_background_agent_that_survived_the_restart() {
        use crate::acp::state::BackgroundAgentStatus;

        let transcript = tempfile::NamedTempFile::new().unwrap();
        tokio::fs::write(
            transcript.path(),
            r#"{"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"final answer"}]}}"#
                .to_string()
                + "\n",
        )
        .await
        .unwrap();

        let mut fixture = attach_with_orphaned_background_agent(
            "s-attach-survivor",
            &transcript.path().to_string_lossy(),
        )
        .await;
        let state = await_background_agent_resolved(
            &mut fixture,
            "s-attach-survivor",
            std::time::Duration::from_secs(10),
            "resumed tailer must report the sub-agent completed",
        )
        .await;

        assert_eq!(
            state.background_agents[0].status,
            BackgroundAgentStatus::Completed,
            "a survivor's resumed tailer must report its real outcome, not Detached"
        );
        assert!(
            state.background_agents[0].warning.is_none(),
            "a spurious Detached-then-Completed sequence folds to the same status but \
             leaves the synthetic sweep's warning set"
        );
    }

    /// The untrackable counterpart: a launch with no transcript path can never
    /// be resumed, so `attach` still detaches it eagerly, same as `spawn`.
    #[tokio::test]
    #[serial_test::serial]
    async fn attach_still_detaches_a_background_agent_with_no_transcript_path() {
        use crate::acp::state::BackgroundAgentStatus;

        let mut fixture = attach_with_orphaned_background_agent("s-attach-untrackable", "").await;
        let state = await_background_agent_resolved(
            &mut fixture,
            "s-attach-untrackable",
            std::time::Duration::from_secs(5),
            "a launch with no transcript must be detached on attach",
        )
        .await;

        assert_eq!(
            state.background_agents[0].status,
            BackgroundAgentStatus::Detached
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn attach_terminates_a_worker_whose_agent_is_no_longer_allowed() {
        use std::os::unix::process::CommandExt as _;
        let (_home, tmp) = isolate_home();
        let mut fake_runner = std::process::Command::new("sleep")
            .arg("60")
            .process_group(0)
            .spawn()
            .expect("spawn stand-in runner");

        let sup = Supervisor::new(VecSink::new());
        let socket = worker_registry::socket_path_for("s-policy").unwrap();
        worker_registry::touch_live_socket(&socket);
        let mut record = worker_registry::WorkerRecord::new(
            "s-policy".into(),
            fake_runner.id(),
            socket,
            "codex-acp".into(),
            "codex".into(),
            tmp.path().to_path_buf(),
            None,
            vec![],
            vec![],
            Some("acp-session".into()),
            None,
        );
        record.detached_at = Some(1);
        let attach = || {
            sup.attach(
                "s-policy".into(),
                tmp.path().to_path_buf(),
                vec![],
                false,
                None,
            )
        };

        crate::session::config::update_config(|c| {
            c.acp.restrict_agents = true;
            c.acp.allowed_agents = vec!["claude".to_string(), "codex".to_string()];
        })
        .unwrap();
        worker_registry::save(&record).unwrap();
        let allowed = attach().await;
        assert!(
            !matches!(allowed, Err(SupervisorError::AgentNotAllowed(_))),
            "a permitted agent must clear the policy gate, got {allowed:?}"
        );

        crate::session::config::update_config(|c| {
            c.acp.allowed_agents = vec!["claude".to_string()];
        })
        .unwrap();
        worker_registry::save(&record).unwrap();
        let got = attach().await;
        assert!(
            matches!(got, Err(SupervisorError::AgentNotAllowed(ref n)) if n == "codex"),
            "expected AgentNotAllowed(codex), got {got:?}"
        );
        assert!(
            worker_registry::load("s-policy").unwrap().is_none(),
            "the disallowed worker's registry record must be cleared"
        );
        let exit = (0..50)
            .find_map(|_| {
                let got = fake_runner.try_wait().unwrap();
                if got.is_none() {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                got
            })
            .expect("the disallowed runner must be signalled, not left running");
        assert!(
            exit.code().is_none(),
            "the runner must exit from a signal: {exit:?}"
        );
    }
}
