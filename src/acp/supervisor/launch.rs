//! Bringing a worker up: admission, spawn, attach, and the launch environment.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

use super::agents::{
    apply_agent_command_override, log_wrapper_substitution, wrapper_substitution_for,
};
use super::publish::collect_resumable_background_agent_launches;
use super::teardown::tear_down_runner;
use super::{
    lock_recover, BroadcastSink, PendingContextReset, ResumeKind, ResumeReservation,
    ResumeReservationOutcome, SpawnRequest, Supervisor, SupervisorError, WorkerHandle, WorkerKind,
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
        let origin = req.origin.clone().ok_or_else(|| {
            SupervisorError::Acp(AcpError::Spawn(
                "structured launch has no original stored-instance authority".into(),
            ))
        })?;
        match self
            .begin_resume(
                &req.session_id,
                crate::acp::runner_lifecycle::NativeResume::Spawn,
                origin,
                false,
            )
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
    pub(crate) fn begin_resume(
        &self,
        session_id: &str,
        operation: crate::acp::runner_lifecycle::NativeResume,
        origin: Arc<crate::session::LaunchOrigin>,
        override_stale_cancel: bool,
    ) -> impl Future<Output = Result<ResumeReservationOutcome, SupervisorError>> + Send + 'static
    {
        let kind = operation.kind();
        let admitted = (|| {
            if origin.session_id() != session_id {
                return Err(SupervisorError::Acp(AcpError::Spawn(
                    "resume origin has another session id".into(),
                )));
            }
            let mut table = lock_recover(&self.lifecycle);
            let lease = match table.admit(session_id, kind) {
                Ok(lease) => lease,
                Err(AdmitError::AlreadyPresent) => {
                    return Ok(ResumeReservationOutcome::AlreadyPresent)
                }
                Err(AdmitError::TeardownPending) => {
                    return Err(SupervisorError::TeardownPending(session_id.to_owned()))
                }
                Err(AdmitError::Cancelled(_) | AdmitError::ShuttingDown) => {
                    return Err(SupervisorError::SpawnCancelled(session_id.to_owned()))
                }
            };
            let issued = table.execution_admission(&lease);
            if let Err(error) = issued.set_origin(origin) {
                table.abandon(&lease);
                return Err(SupervisorError::Acp(AcpError::Spawn(format!(
                    "original admission: {error:#}"
                ))));
            }
            Ok(ResumeReservationOutcome::Reserved(ResumeReservation {
                lease,
                lifecycle: Arc::clone(&self.lifecycle),
                notify: Arc::clone(&self.worker_notify),
                execution: None,
                custody: Some(issued.begin_job()),
                retirement_required: true,
                issued,
            }))
        })();
        // Original scope and job custody exist before the owned driver's first await.
        let admitted = admitted.map(|outcome| match outcome {
            ResumeReservationOutcome::Reserved(reservation) => {
                let custody = Some(reservation.issued.begin_job());
                (ResumeReservationOutcome::Reserved(reservation), custody)
            }
            ResumeReservationOutcome::AlreadyPresent => (outcome, None),
        });
        let mut observation = super::ResumeObservation(admitted.as_ref().ok().and_then(
            |(outcome, _)| match outcome {
                ResumeReservationOutcome::Reserved(reservation) => Some(reservation.issued.clone()),
                ResumeReservationOutcome::AlreadyPresent => None,
            },
        ));
        let lifecycle = self.lifecycle.clone();
        let limit = self.max_concurrent_workers;
        async move {
            let driver = tokio::spawn(async move {
                let (outcome, _custody) = admitted?;
                let ResumeReservationOutcome::Reserved(reservation) = outcome else {
                    return Ok(ResumeReservationOutcome::AlreadyPresent);
                };
                if matches!(kind, ResumeKind::Spawn) {
                    let custody = reservation.issued.begin_job();
                    let retained = tokio::task::spawn_blocking(move || {
                        let _custody = custody;
                        let mut ids =
                            crate::session::runner_journal::retained_runner_session_ids()?;
                        for record in worker_registry::list()? {
                            if worker_registry::is_record_live(&record) {
                                ids.insert(record.session_id);
                            }
                        }
                        anyhow::Ok(ids)
                    })
                    .await
                    .map_err(|error| {
                        SupervisorError::Acp(AcpError::Spawn(format!(
                            "runner inventory task: {error}"
                        )))
                    })?
                    .map_err(|error| {
                        SupervisorError::Acp(AcpError::Spawn(format!(
                            "runner inventory: {error:#}"
                        )))
                    })?;
                    let table = lock_recover(&lifecycle);
                    let external_count = retained
                        .iter()
                        .filter(|id| table.counts_registry_record(id))
                        .count();
                    let combined = table.occupied_slots() + external_count;
                    if combined > limit as usize {
                        return Err(SupervisorError::CapacityFull {
                            current: combined - 1,
                            limit,
                        });
                    }
                }
                let issued = reservation.issued.clone();
                let origin = issued
                    .origin()
                    .expect("native admission registers its original scope");
                let custody = issued.begin_job();
                let preparation_lease = reservation.lease.clone();
                tokio::task::spawn_blocking(move || {
                    let _custody = custody;
                    let (prepared, preparation) =
                        origin.prepare(&operation, &issued, |commit| {
                            crate::acp::runner_lifecycle::PreparationAuthorization::acquire(
                                lock_recover(&lifecycle),
                                &preparation_lease,
                                &origin,
                                override_stale_cancel,
                                commit,
                            )
                        })?;
                    issued.set_prepared_origin(prepared, preparation)?;
                    anyhow::Ok(())
                })
                .await
                .map_err(|error| {
                    SupervisorError::Acp(AcpError::Spawn(format!(
                        "original preparation task: {error}"
                    )))
                })?
                .map_err(|error| match error.downcast_ref::<AdmitError>() {
                    Some(AdmitError::Cancelled(_)) => {
                        SupervisorError::SpawnCancelled(reservation.lease.session_id().to_owned())
                    }
                    Some(AdmitError::TeardownPending) => {
                        SupervisorError::TeardownPending(reservation.lease.session_id().to_owned())
                    }
                    _ => launch_origin_error(error),
                })?;
                reservation.issued.check_active().map_err(|_| {
                    SupervisorError::SpawnCancelled(reservation.lease.session_id().to_owned())
                })?;
                Ok(ResumeReservationOutcome::Reserved(reservation))
            });
            let result = driver.await.map_err(|error| {
                SupervisorError::Acp(AcpError::Spawn(format!("owned resume driver: {error}")))
            })?;
            observation.0.take();
            result
        }
    }

    /// Spawn body, run under the reservation from `begin_resume`.
    pub(crate) async fn spawn_inner(
        &self,
        mut req: SpawnRequest,
        mut reservation: ResumeReservation,
    ) -> Result<(), SupervisorError> {
        let _body_custody = reservation.issued.begin_job();
        let lease = reservation.lease().clone();
        let original = req.origin.as_ref().ok_or_else(|| {
            SupervisorError::Acp(AcpError::Spawn(
                "launch request has no original baseline".into(),
            ))
        })?;
        let prepared = reservation.issued.origin().ok_or_else(|| {
            SupervisorError::Acp(AcpError::Spawn(
                "resume admission lost its original preparation".into(),
            ))
        })?;
        if !Arc::ptr_eq(original, &prepared) && !prepared.is_prepared_from(original) {
            return Err(SupervisorError::Acp(AcpError::Spawn(
                "launch request replaced the admitted original baseline".into(),
            )));
        }
        req.origin = Some(prepared);
        let session_id = req.session_id.as_str();
        capture_launch_origin(&req, reservation.issued.clone()).await?;
        let warmup_guard = self.warmup_guard(&req.agent).await;
        let (mut config, context_reset) = self
            .spawn_config(
                &req,
                reservation
                    .issued
                    .origin()
                    .expect("admitted original scope")
                    .generation(),
                Some(&reservation.issued),
            )
            .await?;
        if lock_recover(&self.lifecycle)
            .cancel_requested(&lease)
            .is_some()
        {
            return Err(SupervisorError::SpawnCancelled(req.session_id));
        }
        debug!(
            target: "acp.supervisor",
            session = %session_id,
            stored_id = ?config.stored_acp_session_id,
            "spawning structured view worker"
        );
        // The hooks above may re-enter aoe, so the lifecycle lock is taken only for each check.
        validate_launch_origin(&reservation.issued).await?;
        // Clear a partial replay from a failed import before session/load re-emits it.
        if config.seed_history_replay {
            self.sink.clear_session_events(session_id);
        }

        config.execution_admission = Some(reservation.issued.clone());
        let launch_config = config.clone();
        let launched = (self.launcher)(launch_config, AcpSessionId(session_id.to_string())).await;
        let mut client = match launched {
            Ok(c) => c,
            Err(err) => {
                // A stop that landed during the launch owns the outcome.
                let cancelled = lock_recover(&self.lifecycle)
                    .cancel_requested(&lease)
                    .is_some();
                if cancelled {
                    self.reap_failed_launch(&lease, &err, reservation.execution())
                        .await;
                    return Err(SupervisorError::SpawnCancelled(req.session_id));
                }
                if matches!(err.underlying(), AcpError::IncompatibleAgent(_)) {
                    self.mark_incompatible_binary(session_id, &config.spec.command);
                }
                publish_rejection(&err, |event| {
                    self.publish_next(session_id, &event);
                });
                self.reap_failed_launch(&lease, &err, reservation.execution())
                    .await;
                return Err(SupervisorError::Acp(err.into_underlying()));
            }
        };
        let identity = reservation.execution().or_else(|| client.runner_identity());
        reservation.execution = identity;

        // A peer that archived or trashed the row during the handshake wins: retire the runner.
        if let Err(refused) = validate_launch_origin(&reservation.issued).await {
            let _ = client.shutdown().await;
            self.retire_refused_install(&lease, identity, InstallError::Stale)
                .await;
            return Err(refused);
        }

        if warmup_guard.is_some() {
            lock_recover(&self.warmed_up_agents).insert(req.agent.clone());
        }
        drop(warmup_guard);
        info!(target: "acp.supervisor", session = %session_id, "structured view worker spawned");
        lock_recover(&self.incompatible_binaries).remove(session_id);

        let inbound = client
            .take_inbound()
            .expect("freshly spawned AcpClient always has inbound receiver");

        let kind = WorkerKind::Runner {
            spawn_config: Box::new(config),
        };
        let client = self
            .install_worker(
                session_id,
                reservation,
                client,
                inbound,
                (identity, context_reset),
                kind,
            )
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
        admission: Option<&crate::acp::runner_lifecycle::ExecutionAdmission>,
    ) -> Result<(SpawnConfig, Option<PendingContextReset>), SupervisorError> {
        let profile = req
            .origin
            .as_ref()
            .map(|origin| origin.profile().to_owned())
            .unwrap_or_default();
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

        let (base_host_environment, mut host_environment) = if req.sandbox_info.is_none() {
            host_spawn_environment(
                &resolved_cfg,
                &req.session_id,
                &req.tool,
                req.origin
                    .as_ref()
                    .map(|origin| origin.profile().to_owned())
                    .unwrap_or_default(),
                req.cwd.clone(),
                admission.cloned(),
            )
            .await?
        } else {
            (Vec::new(), Vec::new())
        };

        let claude_store_pin = req.claude_store_pin.clone().filter(|_| {
            req.sandbox_info.is_none() && matches!(req.agent.as_str(), "claude" | "claude-code")
        });
        let claude_config_dir =
            apply_claude_store_pin(&mut host_environment, claude_store_pin.as_ref());

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
            req.origin
                .as_ref()
                .map(|origin| origin.profile().to_owned()),
            req.cwd.clone(),
            host_environment.clone(),
            claude_config_dir,
            "MCP resolution task failed",
        )
        .await;

        let (stored_acp_session_id, fork_from, seed_history_replay, context_reset, source_profile) =
            if req.sandbox_info.as_ref().is_some_and(|info| info.enabled) {
                let native_key = wrapper_substitution
                    .as_ref()
                    .map(|(_, base)| base.as_str())
                    .unwrap_or(&req.agent);
                let native_agent =
                    crate::acp::agent_profiles::resolve(native_key).native_config_agent;
                let admission = admission.ok_or_else(|| {
                    SupervisorError::Acp(AcpError::Spawn(
                        "sandbox launch has no execution admission".into(),
                    ))
                })?;
                let origin = admission.origin().ok_or_else(|| {
                    SupervisorError::Acp(AcpError::Spawn(
                        "sandbox launch has no original stored owner".into(),
                    ))
                })?;
                let custody = admission.begin_job();
                let continuation = req.sandbox_continuation;
                let context =
                    tokio::task::spawn_blocking(move || {
                        let _custody = custody;
                        origin.update_storage(|storage, row| {
                            crate::migrations::v033_isolate_sandbox_content::prepare_acp_context(
                            storage.profile(), row, native_agent, generation,
                            crate::migrations::v033_isolate_sandbox_content::AcpContextUse::Launch,
                            continuation,
                        )
                        }, Ok)
                    })
                    .await
                    .map_err(|error| {
                        SupervisorError::Acp(AcpError::Spawn(format!(
                            "sandbox context handoff task: {error}"
                        )))
                    })?
                    .map_err(|error| {
                        SupervisorError::Acp(AcpError::Spawn(format!(
                            "sandbox context handoff: {error}"
                        )))
                    })?;
                let reset_profile = context.profile.clone();
                let reset = context
                    .notice
                    .map(|(reason, transactions)| PendingContextReset {
                        profile: reset_profile,
                        generation,
                        reason,
                        transactions,
                    });
                (
                    context.stored_session_id,
                    context.fork_from,
                    context.seed_history_replay,
                    reset,
                    Some(context.profile),
                )
            } else {
                (
                    req.stored_acp_session_id.clone(),
                    req.fork_from.clone(),
                    req.seed_history_replay,
                    None,
                    req.origin
                        .as_ref()
                        .map(|origin| origin.profile().to_owned()),
                )
            };

        Ok((
            SpawnConfig {
                execution_admission: None,
                managed_profile: req
                    .origin
                    .as_ref()
                    .map(|origin| origin.profile().to_owned()),
                agent_key: req.agent.clone(),
                tool: req.tool.clone(),
                spec,
                cwd: req.cwd.clone(),
                additional_dirs: req.additional_dirs.clone(),
                provider_env,
                host_environment,
                base_host_environment,
                default_effort: effort,
                default_effort_explicit: req.effort_explicit,
                default_mode: acp_defaults.and_then(|defaults| defaults.mode()),
                default_model: model,
                socket_path: Some(socket_path),
                stored_acp_session_id,
                fork_from,
                sandbox_info: req.sandbox_info.clone(),
                source_profile,
                mcp_servers,
                seed_history_replay,
                artifact_dir: crate::session::artifacts::session_artifact_dir(&req.session_id).ok(),
                wrapper_substitution,
                generation,
                claude_store_pin,
                provider_routing: req
                    .provider
                    .as_deref()
                    .and_then(crate::session::environment::provider_override_env)
                    .unwrap_or_default(),
            },
            context_reset,
        ))
    }

    /// Install a launched client under the reservation's lease, or retire it
    /// when a stop or a newer epoch won the race. Returns the installed client.
    async fn install_worker(
        &self,
        session_id: &str,
        mut reservation: ResumeReservation,
        client: AcpClient,
        inbound: mpsc::Receiver<Event>,
        installation: (Option<RunnerIdentity>, Option<PendingContextReset>),
        kind: WorkerKind,
    ) -> Result<Arc<AcpClient>, SupervisorError> {
        let (identity, context_reset) = installation;
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
        reservation.installed();
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
        if context_reset.is_some() {
            lock_recover(&self.pending_context_resets).insert(session_id.to_string());
        }
        let drain_task = self.start_drain_task(
            session_id.to_string(),
            lease.clone(),
            inbound,
            context_reset,
        );
        let client_for_resume = (!resumable.is_empty()).then(|| Arc::clone(&client));
        workers.insert(
            session_id.to_string(),
            WorkerHandle {
                client: Arc::clone(&client),
                native_session_id: None,
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

    async fn reap_failed_launch(
        &self,
        lease: &Lease,
        error: &AcpError,
        captured: Option<RunnerIdentity>,
    ) {
        let reported = error.issued_execution();
        let Some((identity, settled)) = captured
            .map(|identity| (Some(identity), reported == Some((Some(identity), true))))
            .or(reported)
        else {
            return;
        };
        if lock_recover(&self.lifecycle).convert_to_stopping(lease, identity) {
            let settlement = if settled {
                crate::acp::runner_lifecycle::Settlement::Proven
            } else {
                tear_down_runner(lease.session_id(), identity).await
            };
            self.settle(lease, settlement);
        }
    }

    /// Tear down a built worker whose install the lifecycle table refused.
    async fn retire_refused_install(
        &self,
        lease: &Lease,
        identity: Option<RunnerIdentity>,
        refusal: InstallError,
    ) -> SupervisorError {
        let session_id = lease.session_id();
        lock_recover(&self.lifecycle).convert_to_stopping(lease, identity);
        let settlement = tear_down_runner(session_id, identity).await;
        self.settle(lease, settlement);
        match refusal {
            InstallError::Cancelled { reason } => {
                debug!(
                    target: "acp.supervisor",
                    session = %session_id,
                    %reason,
                    "resume cancelled by a concurrent shutdown; runner torn down"
                );

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
        origin: Arc<crate::session::LaunchOrigin>,
    ) -> Result<(), SupervisorError> {
        let record = Arc::new(
            match worker_registry::load_strict(&session_id).map_err(|error| {
                SupervisorError::Acp(AcpError::Spawn(format!("registry load: {error}")))
            })? {
                Some(record) if worker_registry::is_record_live(&record) => record,
                _ => return Err(SupervisorError::UnknownSession(session_id)),
            },
        );
        match self
            .begin_resume(
                &session_id,
                crate::acp::runner_lifecycle::NativeResume::Attach(record.clone()),
                origin.clone(),
                false,
            )
            .await?
        {
            ResumeReservationOutcome::Reserved(r) => {
                self.attach_inner(
                    super::AttachRequest {
                        session_id,
                        cwd,
                        additional_dirs,
                        in_flight_turn,
                        sandbox,
                        origin,
                    },
                    &record,
                    r,
                )
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
        request: super::AttachRequest,
        record: &worker_registry::WorkerRecord,
        mut reservation: ResumeReservation,
    ) -> Result<(), SupervisorError> {
        let _body_custody = reservation.issued.begin_job();
        let super::AttachRequest {
            session_id,
            cwd,
            additional_dirs,
            in_flight_turn,
            sandbox,
            origin: original,
        } = request;
        let nonce = record.launch_nonce.ok_or_else(|| {
            SupervisorError::Acp(AcpError::Spawn(
                "legacy runner lacks an authenticated execution ticket; session remains protected"
                    .into(),
            ))
        })?;
        let id = session_id.clone();
        let pid = record.pid;
        let generation = record.generation;
        let issued = reservation.issued.clone();
        let origin = issued.origin().ok_or_else(|| {
            SupervisorError::Acp(AcpError::Spawn(
                "resident attach lost its prepared scope".into(),
            ))
        })?;
        if !origin.is_prepared_from(&original) {
            return Err(SupervisorError::Acp(AcpError::Spawn(
                "resident attach replaced its original baseline".into(),
            )));
        }
        let custody = issued.begin_job();
        let (origin, identity) = tokio::task::spawn_blocking(move || {
            let _custody = custody;
            origin.validate()?;
            let identity = crate::session::runner_journal::verify_published_runner(
                origin.storage(),
                &id,
                nonce,
                pid,
                generation,
            )?;
            anyhow::Ok((origin, identity))
        })
        .await
        .map_err(|e| {
            SupervisorError::Acp(AcpError::Spawn(format!(
                "runner ticket verification task: {e}"
            )))
        })?
        .map_err(|e| {
            SupervisorError::Acp(AcpError::Spawn(format!(
                "runner ticket verification: {e:#}"
            )))
        })?;
        reservation.issued.capture(identity);
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
            let settlement = tear_down_runner(&session_id, Some(identity)).await;
            if settlement != crate::acp::runner_lifecycle::Settlement::Proven {
                warn!(target: "acp.supervisor", session = %session_id, "disallowed runner remains protected; stop proof unavailable");
            }
            return Err(SupervisorError::AgentNotAllowed(agent_key));
        }

        let Some(stored_acp_session_id) = record.stored_acp_session_id.clone() else {
            return Err(SupervisorError::Acp(AcpError::Spawn(
                "runner registry has no stored_acp_session_id; need fresh spawn".into(),
            )));
        };
        let sandbox_resources = match sandbox.as_ref() {
            Some(info) => {
                let info = info.clone();
                let cwd = cwd.clone();
                let origin = origin.clone();
                let custody = reservation.issued.begin_job();
                Some(
                    tokio::task::spawn_blocking(move || {
                        let _custody = custody;
                        origin
                            .with_storage(|storage, _row| {
                                crate::acp::acp_client::SessionSandbox::from_info(
                                    &info,
                                    cwd.as_path(),
                                    Some(storage.profile().to_owned()),
                                )
                                .map_err(anyhow::Error::from)
                            })
                            .map_err(|error| {
                                AcpError::Spawn(format!("sandbox original owner: {error:#}"))
                            })
                    })
                    .await
                    .map_err(|e| {
                        AcpError::Spawn(format!("sandbox resolve task panicked: {e}"))
                    })??,
                )
            }
            None => None,
        };
        let context_reset = if sandbox.as_ref().is_some_and(|info| info.enabled) {
            let origin = origin.clone();
            let custody = reservation.issued.begin_job();
            let native_agent = crate::acp::agent_profiles::resolve(&agent_key).native_config_agent;
            let generation = reservation.lease().epoch();
            let context = tokio::task::spawn_blocking(move || {
                let _custody = custody;
                origin.update_storage(
                    |storage, row| {
                        crate::migrations::v033_isolate_sandbox_content::prepare_acp_context(
                            storage.profile(),
                            row,
                            native_agent,
                            generation,
                            crate::migrations::v033_isolate_sandbox_content::AcpContextUse::Attach,
                            super::SandboxContinuation::Persisted,
                        )
                    },
                    Ok,
                )
            })
            .await
            .map_err(|error| {
                SupervisorError::Acp(AcpError::Spawn(format!(
                    "sandbox context handoff task: {error}"
                )))
            })?
            .map_err(|error| {
                SupervisorError::Acp(AcpError::Spawn(format!("sandbox context handoff: {error}")))
            })?;
            context
                .notice
                .map(|(reason, transactions)| PendingContextReset {
                    profile: context.profile,
                    generation,
                    reason,
                    transactions,
                })
        } else {
            None
        };

        let connected = AcpClient::attach(
            record.socket_path.clone(),
            cwd,
            additional_dirs,
            stored_acp_session_id,
            in_flight_turn,
            AcpSessionId(session_id.clone()),
            sandbox_resources,
            agent_key,
            Some(origin.storage().profile().to_owned()),
            nonce,
        );
        let mut client = tokio::select! {
            result = connected => result?,
            _ = reservation.issued.shutdown_cancelled() => {
                reservation.retirement_required = false;
                return Err(SupervisorError::SpawnCancelled(session_id));
            }
        };
        client.capture_runner(identity);

        let inbound = client
            .take_inbound()
            .expect("freshly attached AcpClient always has inbound receiver");
        self.install_worker(
            &session_id,
            reservation,
            client,
            inbound,
            (Some(identity), context_reset),
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
    match err.underlying() {
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

fn launch_origin_error(error: anyhow::Error) -> SupervisorError {
    if let Some(blocked) = error.downcast_ref::<crate::session::StartBlocked>() {
        SupervisorError::Blocked(*blocked)
    } else if let Some(gone) =
        error.downcast_ref::<crate::session::runner_journal::LaunchSessionGone>()
    {
        SupervisorError::SessionGone(gone.0.clone())
    } else {
        SupervisorError::Acp(AcpError::Spawn(format!("launch origin: {error:#}")))
    }
}

pub(super) async fn capture_launch_origin(
    req: &SpawnRequest,
    admission: crate::acp::runner_lifecycle::ExecutionAdmission,
) -> Result<(), SupervisorError> {
    let origin = req.origin.clone().ok_or_else(|| {
        SupervisorError::Acp(AcpError::Spawn(
            "native launch has no original prepared authority".into(),
        ))
    })?;
    let session_id = req.session_id.clone();
    let cwd = req.cwd.clone();
    let tool = req.tool.clone();
    let sandbox = req.sandbox_info.clone();
    let yolo_mode = req.yolo_mode;
    let provider = req.provider.clone();
    let command = req
        .agent_command_override
        .as_ref()
        .map(|command| command.command.clone());
    let custody = admission.begin_job();
    tokio::task::spawn_blocking(move || {
        let _custody = custody;
        anyhow::ensure!(
            origin.session_id() == session_id,
            "launch authority belongs to another session"
        );
        origin.validate_request(
            &cwd,
            &tool,
            sandbox.as_ref(),
            yolo_mode,
            command.as_deref(),
            provider.as_deref(),
        )?;
        origin.with_storage(|_, _row| Ok(()))?;
        let existing = admission
            .origin()
            .context("native admission lost its original authority")?;
        anyhow::ensure!(
            Arc::ptr_eq(&existing, &origin),
            "launch handoff replaced its prepared authority"
        );
        anyhow::Ok(())
    })
    .await
    .map_err(|error| SupervisorError::Acp(AcpError::Spawn(format!("launch origin task: {error}"))))?
    .map_err(launch_origin_error)
}

pub(super) async fn validate_launch_origin(
    admission: &crate::acp::runner_lifecycle::ExecutionAdmission,
) -> Result<(), SupervisorError> {
    let origin = admission.origin().ok_or_else(|| {
        SupervisorError::Acp(AcpError::Spawn(
            "native admission lost its original prepared authority".into(),
        ))
    })?;
    let custody = admission.begin_job();
    tokio::task::spawn_blocking(move || {
        let _custody = custody;
        origin.validate()
    })
    .await
    .map_err(|error| SupervisorError::Acp(AcpError::Spawn(format!("launch origin task: {error}"))))?
    .map_err(launch_origin_error)
}

/// Resolve trusted host environment and overlay the before_session hook output.
pub(crate) async fn host_spawn_environment(
    cfg: &crate::session::Config,
    session_id: &str,
    tool: &str,
    profile: String,
    cwd: PathBuf,
    admission: Option<crate::acp::runner_lifecycle::ExecutionAdmission>,
) -> Result<(Vec<(String, String)>, Vec<(String, String)>), SupervisorError> {
    let base = crate::session::environment::resolve_host_environment_pairs(&cfg.environment);
    let mut host = base.clone();
    if !cfg.host_hooks.before_session.is_empty() {
        let minted = before_session_env(session_id, tool, profile, cwd, admission)
            .await
            .map_err(|e| {
                SupervisorError::InvalidAgentCommand(format!(
                    "before_session hook task failed: {e}"
                ))
            })?
            .map_err(|e| {
                SupervisorError::Acp(AcpError::Spawn(format!("before_session hook: {e}")))
            })?;
        overlay_env(&mut host, minted);
    }
    Ok((base, host))
}

/// Run the profile's `before_session` host hooks and return the env they mint.
pub(super) async fn before_session_env(
    session_id: &str,
    tool: &str,
    profile: String,
    cwd: PathBuf,
    admission: Option<crate::acp::runner_lifecycle::ExecutionAdmission>,
) -> Result<anyhow::Result<Vec<(String, String)>>, tokio::task::JoinError> {
    let session_id = session_id.to_string();
    let tool = tool.to_string();
    let custody = admission.as_ref().map(|admission| admission.begin_job());
    tokio::task::spawn_blocking(move || {
        let _custody = custody;
        use crate::session::config::repo_config::{
            resolve_before_session_hooks, run_before_session_hooks,
        };
        if let Some(origin) = admission.as_ref().and_then(|admission| admission.origin()) {
            origin.validate()?;
        }
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
pub(super) fn apply_claude_store_pin(
    environment: &mut Vec<(String, String)>,
    pin: Option<&crate::session::capture::ClaudeStorePin>,
) -> Option<std::path::PathBuf> {
    let pin = pin?;
    let value = |key: &str| {
        environment
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
            .or_else(|| std::env::var(key).ok())
            .filter(|value| !value.is_empty())
    };
    let home = value("HOME").map(std::path::PathBuf::from);
    let export = crate::session::capture::exports_claude_store(pin, home.as_deref());
    environment.retain(|(key, _)| key != "CLAUDE_CONFIG_DIR");
    if export {
        environment.push((
            "CLAUDE_CONFIG_DIR".into(),
            pin.store.to_string_lossy().into_owned(),
        ));
        Some(pin.store.clone())
    } else {
        home
    }
}

pub(super) async fn resolve_mcp_servers(
    agent_key: &str,
    session_id: &str,
    profile: Option<String>,
    cwd: PathBuf,
    session_env: Vec<(String, String)>,
    native_config_override: Option<PathBuf>,
    failure: &'static str,
) -> Vec<agent_client_protocol::schema::v1::McpServer> {
    let agent_key = agent_key.to_string();
    let session = session_id.to_string();
    tokio::task::spawn_blocking(move || {
        resolve_mcp_layers(
            &agent_key,
            &session,
            profile.as_deref(),
            &cwd,
            &session_env,
            native_config_override.as_deref(),
        )
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
    native_config_override: Option<&std::path::Path>,
) -> Vec<agent_client_protocol::schema::v1::McpServer> {
    use crate::session::mcp::mcp_model::{resolve_effective, summarize};

    let merged = resolve_effective(agent_key, profile, cwd, session_env, native_config_override);
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
    use crate::daemon::AcpWorkerState;

    fn mcp_names(servers: &[agent_client_protocol::schema::v1::McpServer]) -> Vec<&str> {
        use agent_client_protocol::schema::v1::McpServer;
        servers
            .iter()
            .map(|server| match server {
                McpServer::Stdio(server) => server.name.as_str(),
                McpServer::Http(server) => server.name.as_str(),
                McpServer::Sse(server) => server.name.as_str(),
                _ => "unknown",
            })
            .collect()
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn selected_claude_store_controls_spawn_environment_and_native_mcp() {
        let (_home, temp) = isolate_home();
        let declared = temp.path().join("declared");
        let selected = temp.path().join("selected");
        std::fs::create_dir_all(&declared).unwrap();
        std::fs::create_dir_all(&selected).unwrap();
        std::fs::write(
            declared.join(".claude.json"),
            r#"{ "mcpServers": { "declared": { "command": "declared" } } }"#,
        )
        .unwrap();
        std::fs::write(
            selected.join(".claude.json"),
            r#"{ "mcpServers": { "selected": { "command": "selected" } } }"#,
        )
        .unwrap();
        let app_dir = crate::session::get_app_dir().unwrap();
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("config.toml"),
            format!(
                "[host_hooks]\nbefore_session = \"printf 'CLAUDE_CONFIG_DIR={}\\nHOOK_VALUE=kept\\n'\"\n\
                 [session.agent_config_dir]\nclaude-code = \"{}\"\n",
                temp.path().join("hook").display(),
                declared.display()
            ),
        )
        .unwrap();

        let supervisor = Supervisor::new(VecSink::new());
        let mut request = spawn_request("selected-store");
        request.claude_store_pin = Some(crate::session::capture::ClaudeStorePin {
            store: selected.clone(),
            exported_default_store: Some(false),
        });
        let (config, context_reset) = supervisor.spawn_config(&request, 1, None).await.unwrap();
        assert!(context_reset.is_none());

        assert_eq!(mcp_names(&config.mcp_servers), ["selected"]);
        assert_eq!(
            config
                .host_environment
                .iter()
                .find(|(key, _)| key == "CLAUDE_CONFIG_DIR")
                .map(|(_, value)| value.as_str()),
            selected.to_str()
        );
        assert!(config
            .host_environment
            .contains(&("HOOK_VALUE".into(), "kept".into())));
    }

    #[test]
    #[serial_test::serial]
    fn claude_route_distinguishes_implicit_explicit_and_custom_stores() {
        let (_home, temp) = isolate_home();
        let home = temp.path().to_path_buf();
        let default = home.join(".claude");
        let custom = home.join("custom");
        std::fs::create_dir_all(&default).unwrap();
        std::fs::create_dir_all(&custom).unwrap();
        let route = |store: &std::path::Path, provenance: Option<bool>, hook: &str| {
            let mut environment = vec![
                ("HOME".into(), home.display().to_string()),
                ("CLAUDE_CONFIG_DIR".into(), hook.into()),
                ("AUTH_SENTINEL".into(), "kept".into()),
            ];
            let effective = apply_claude_store_pin(
                &mut environment,
                Some(&crate::session::capture::ClaudeStorePin {
                    store: store.to_path_buf(),
                    exported_default_store: provenance,
                }),
            );
            let exported = environment
                .iter()
                .find(|(key, _)| key == "CLAUDE_CONFIG_DIR")
                .map(|(_, value)| PathBuf::from(value));
            assert!(environment.contains(&("AUTH_SENTINEL".into(), "kept".into())));
            (effective, exported)
        };

        assert_eq!(
            route(&default, Some(false), "/other"),
            (Some(home.clone()), None)
        );
        assert_eq!(route(&default, None, "/other"), (Some(home.clone()), None));
        assert_eq!(
            route(&default, Some(true), "/other"),
            (Some(default.clone()), Some(default.clone()))
        );
        assert_eq!(
            route(&custom, Some(false), "/other"),
            (Some(custom.clone()), Some(custom.clone()))
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn implicit_default_claude_store_aligns_native_mcp_with_home() {
        let (_home, temp) = isolate_home();
        let hook_store = temp.path().join("hook-store");
        let home = temp.path().to_path_buf();
        let default = home.join(".claude");
        std::fs::create_dir_all(&hook_store).unwrap();
        std::fs::write(
            hook_store.join(".claude.json"),
            r#"{ "mcpServers": { "stale": { "command": "stale" } } }"#,
        )
        .unwrap();
        std::fs::write(
            home.join(".claude.json"),
            r#"{ "mcpServers": { "home": { "command": "home" } } }"#,
        )
        .unwrap();
        let app_dir = crate::session::get_app_dir().unwrap();
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("config.toml"),
            format!(
                "[host_hooks]\nbefore_session = \"printf 'CLAUDE_CONFIG_DIR={}\\n'\n",
                hook_store.display()
            ),
        )
        .unwrap();

        let supervisor = Supervisor::new(VecSink::new());
        let mut request = spawn_request("implicit-default");
        request.claude_store_pin = Some(crate::session::capture::ClaudeStorePin {
            store: default,
            exported_default_store: Some(false),
        });
        let (config, _) = supervisor.spawn_config(&request, 1, None).await.unwrap();
        assert!(!config
            .host_environment
            .iter()
            .any(|(key, _)| key == "CLAUDE_CONFIG_DIR"));
        assert_eq!(mcp_names(&config.mcp_servers), ["home"]);
    }

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
        let storage = crate::session::Storage::new_unwatched("default").unwrap();
        let mut inst = crate::session::Instance::new("s-prov", "/tmp");
        inst.id = "s-prov".into();
        inst.source_profile = storage.profile().to_owned();
        inst.view = crate::session::View::Structured;
        inst.storage_origin = Some(std::sync::Arc::new(storage.clone()));
        storage
            .update(|rows, _| {
                rows.push(inst.clone());
                Ok(())
            })
            .unwrap();
        let gate = Gate::default();
        let sup = Arc::new(Supervisor::new(VecSink::new()).with_launcher(gated_launcher(&gate)));
        let mut req = spawn_request("s-prov");
        req.origin = Some(crate::session::LaunchOrigin::capture(&inst).unwrap());
        req.effort = Some("low".into());

        let mut spawner = {
            let sup = Arc::clone(&sup);
            tokio::spawn(async move { sup.spawn(req).await })
        };
        tokio::select! {
            result = &mut spawner => {
                panic!("spawn ended before entering the handshake gate: {result:?}");
            }
            entered = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                gate.entered.notified(),
            ) => {
                if entered.is_err() {
                    spawner.abort();
                    let result = spawner.await;
                    panic!("spawn did not enter the handshake gate within 5s: {result:?}");
                }
            }
        }
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
        sup.shutdown(crate::acp::supervisor::test_support::stop_receipt("s-prov"))
            .await
            .expect("fixture shutdown");
    }

    /// #4116: the handshake holds no lifecycle lock, so an archive (which a TUI takes on its
    /// input thread) commits without waiting; the post-handshake recheck retires the runner.
    #[tokio::test]
    #[serial_test::serial]
    async fn spawn_retires_a_runner_whose_row_was_archived_during_the_handshake() {
        let _home = isolate_home();
        let gate = Gate::default();
        let sup = Arc::new(Supervisor::new(VecSink::new()).with_launcher(gated_launcher(&gate)));
        let storage = crate::session::Storage::new_unwatched("default").unwrap();
        let mut inst = crate::session::Instance::new("s-archived", "/tmp");
        inst.id = "s-archived".into();
        inst.source_profile = storage.profile().to_owned();
        inst.view = crate::session::View::Structured;
        inst.storage_origin = Some(std::sync::Arc::new(storage.clone()));
        storage
            .update(|rows, _| {
                rows.push(inst.clone());
                Ok(())
            })
            .unwrap();
        let mut req = spawn_request("s-archived");
        req.origin = Some(crate::session::LaunchOrigin::capture(&inst).unwrap());
        let mut spawner = {
            let sup = Arc::clone(&sup);
            tokio::spawn(async move { sup.spawn(req).await })
        };
        tokio::select! {
            result = &mut spawner => {
                panic!("spawn ended before entering the handshake gate: {result:?}");
            }
            entered = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                gate.entered.notified(),
            ) => {
                if entered.is_err() {
                    spawner.abort();
                    let result = spawner.await;
                    panic!("spawn did not enter the handshake gate within 5s: {result:?}");
                }
            }
        }
        let born = crate::process::worker_registry::load("s-archived")
            .unwrap()
            .unwrap()
            .incarnation
            .unwrap();
        assert_eq!(
            crate::process::process_incarnation(born.pid).unwrap(),
            Some(born)
        );
        assert!(crate::process::worker::is_process_group_alive(born.group));

        assert!(
            !storage.instance_lifecycle_lock_is_held_for_test("s-archived"),
            "the handshake must not hold the lifecycle lock"
        );
        {
            let _lock = storage
                .acquire_instance_lifecycle_lock("s-archived")
                .unwrap();
            storage
                .update(|rows, _| {
                    rows.iter_mut()
                        .find(|row| row.id == "s-archived")
                        .expect("stored archive fixture")
                        .archive();
                    Ok(())
                })
                .unwrap();
        }
        gate.open.notify_one();

        assert!(matches!(
            spawner.await.unwrap(),
            Err(SupervisorError::Blocked(
                crate::session::StartBlocked::Archived
            ))
        ));
        assert!(!crate::process::worker::is_process_group_alive(born.group));
        // The failed body and its native group have actually completed. Advance
        // only orphan-maintenance eligibility, not the execution birth proof.
        lock_recover(&sup.lifecycle).age_stopping("s-archived", std::time::Duration::from_secs(15));
        sup.retry_pending_teardowns(|lease| assert_eq!(lease.session_id(), "s-archived"))
            .await;
        assert!(storage
            .load()
            .unwrap()
            .into_iter()
            .find(|row| row.id == "s-archived")
            .unwrap()
            .is_archived());
        assert_eq!(sup.worker_state("s-archived").await, AcpWorkerState::Absent);
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
        let _a = reserve(
            sup.begin_resume(
                "s-a",
                crate::acp::runner_lifecycle::NativeResume::Spawn,
                crate::acp::supervisor::test_support::stored_origin("s-a"),
                false,
            )
            .await,
        );
        let _attach = reserve(
            crate::acp::supervisor::test_support::memory_resume(
                &sup,
                "s-attach",
                ResumeKind::Attach,
            )
            .await,
        );
        let _b = reserve(
            sup.begin_resume(
                "s-b",
                crate::acp::runner_lifecycle::NativeResume::Spawn,
                crate::acp::supervisor::test_support::stored_origin("s-b"),
                false,
            )
            .await,
        );
        match sup
            .begin_resume(
                "s-c",
                crate::acp::runner_lifecycle::NativeResume::Spawn,
                crate::acp::supervisor::test_support::stored_origin("s-c"),
                false,
            )
            .await
        {
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
                resolve_mcp_layers("claude", "resolve-test", profile, &cwd, &[], None)
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
                Ok(client)
            })
        });
        let sup = Supervisor::new(sink).with_launcher(launcher);
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
        let _ = sup
            .shutdown(crate::acp::supervisor::test_support::stop_receipt(
                "s-startup",
            ))
            .await;
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
                Ok(client)
            })
        });
        let sup = Supervisor::new(sink).with_launcher(launcher);
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
        let _ = sup
            .shutdown(crate::acp::supervisor::test_support::stop_receipt(
                "s-startup",
            ))
            .await;
    }
}
