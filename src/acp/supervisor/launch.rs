//! Bringing a worker up: admission, spawn, attach, and the launch environment.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

use super::agents::{
    apply_agent_command_override, log_wrapper_substitution, wrapper_substitution_for,
};
use super::publish::collect_resumable_background_agent_launches;
use super::{
    lock_recover, BroadcastSink, PendingContextReset, RestartRequestGuard, ResumeKind,
    ResumeReservation, ResumeReservationOutcome, SpawnRequest, Supervisor, SupervisorError,
    WorkerHandle, WorkerKind,
};
use crate::acp::acp_client::{AcpClient, AcpError, SpawnConfig};
use crate::acp::agent_policy::AgentPolicy;
use crate::acp::runner_lifecycle::{AdmitError, InstallError, Lease, RunnerIdentity};
use crate::acp::state::{AcpSessionId, Event};
use crate::process::worker_registry;
use crate::session::config::repo_config::resolve_config_with_repo_or_warn;
use crate::session::SandboxInfo;
use tokio_util::sync::CancellationToken;

impl<S: BroadcastSink> Supervisor<S> {
    /// Spawn a structured view worker for the given session.
    pub async fn spawn(
        &self,
        req: SpawnRequest,
        authority: crate::process::worker_registry::ResumeAuthority,
    ) -> Result<(), SupervisorError> {
        self.spawn_with_restart_claim(req, authority, None, None)
            .await
    }

    /// Start the replacement worker under the restart claim that excludes
    /// concurrent ordinary starts for this session.
    pub(crate) async fn spawn_for_restart(
        &self,
        req: SpawnRequest,
        authority: crate::process::worker_registry::ResumeAuthority,
        restart: &RestartRequestGuard,
    ) -> Result<(), SupervisorError> {
        self.spawn_with_restart_claim(req, authority, Some(restart), None)
            .await
    }

    /// Reserve a worker slot for callers that perform the launch body
    /// themselves. Keep this compatibility path on the same admission logic
    /// as the request-aware spawn helpers.
    pub(crate) async fn reserve_spawn(
        &self,
        session_id: &str,
        kind: ResumeKind,
        authority: crate::process::worker_registry::ResumeAuthority,
    ) -> Result<ResumeReservation, SupervisorError> {
        match self.begin_resume(session_id, kind, authority).await? {
            ResumeReservationOutcome::Reserved(reservation) => Ok(reservation),
            ResumeReservationOutcome::AlreadyPresent => {
                Err(SupervisorError::AlreadyRunning(session_id.to_owned()))
            }
        }
    }

    async fn spawn_with_restart_claim(
        &self,
        req: SpawnRequest,
        authority: crate::process::worker_registry::ResumeAuthority,
        restart: Option<&RestartRequestGuard>,
        cancellation: Option<CancellationToken>,
    ) -> Result<(), SupervisorError> {
        let kind = if req.stored_acp_session_id.is_some() {
            ResumeKind::Spawn
        } else {
            ResumeKind::SpawnFirst
        };
        match self
            .begin_resume_with_restart_claim(
                &req.session_id,
                kind,
                authority,
                restart,
                cancellation,
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
    pub(crate) async fn begin_resume(
        &self,
        session_id: &str,
        kind: ResumeKind,
        authority: crate::process::worker_registry::ResumeAuthority,
    ) -> Result<ResumeReservationOutcome, SupervisorError> {
        self.begin_resume_with_restart_claim(session_id, kind, authority, None, None)
            .await
    }

    async fn begin_resume_with_restart_claim(
        &self,
        session_id: &str,
        kind: ResumeKind,
        authority: crate::process::worker_registry::ResumeAuthority,
        restart: Option<&RestartRequestGuard>,
        cancellation: Option<CancellationToken>,
    ) -> Result<ResumeReservationOutcome, SupervisorError> {
        let observed_stop = worker_registry::stop_intent(session_id)
            .map_err(|error| SupervisorError::Acp(AcpError::Spawn(error.to_string())))?;
        // Hold the worker gate while claiming the matching durable stop token.
        let _workers = self.workers.lock().await;
        let request_state = lock_recover(&self.spawn_requests);
        if (request_state.stop_fences.contains_key(session_id) && restart.is_none())
            || cancellation
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
        {
            return Err(SupervisorError::SpawnCancelled(session_id.to_string()));
        }
        drop(request_state);
        let restart_requests = lock_recover(&self.restart_requests);
        let active_restart = restart_requests.get(session_id);
        let restart_in_progress = active_restart.is_some();
        let restart_authorized = restart.is_some_and(|claim| {
            claim.session_id == session_id
                && Arc::ptr_eq(&claim.requests, &self.restart_requests)
                && active_restart.is_some_and(|(request_id, token)| {
                    *request_id == claim.request_id && !token.is_cancelled()
                })
        });
        if (restart_in_progress || restart.is_some()) && !restart_authorized {
            if restart.is_some_and(|claim| claim.session_id == session_id && claim.is_cancelled()) {
                return Err(SupervisorError::SpawnCancelled(session_id.to_string()));
            }
            return Err(SupervisorError::RestartInProgress(session_id.to_string()));
        }
        drop(restart_requests);
        worker_registry::admit_resume(session_id, authority, &observed_stop)
            .map_err(|error| SupervisorError::Acp(AcpError::Spawn(error.to_string())))?;
        // Freeze the observed runner under the same gate as stop and installation.
        // Later registry contents may describe a replacement, not this attach.
        let attach_record = if kind == ResumeKind::Attach {
            worker_registry::load_strict(session_id)
                .map_err(|error| SupervisorError::Acp(AcpError::Spawn(error.to_string())))?
                .map(Box::new)
        } else {
            None
        };
        let mut table = lock_recover(&self.lifecycle);
        if authority == crate::process::worker_registry::ResumeAuthority::Explicit {
            table.forget_stale_cancel(session_id);
        }
        let lease = match table.admit(session_id, kind) {
            Ok(lease) => lease,
            Err(AdmitError::AlreadyPresent) => return Ok(ResumeReservationOutcome::AlreadyPresent),
            Err(AdmitError::TeardownPending) => {
                return Err(SupervisorError::TeardownPending(session_id.to_string()))
            }
            Err(AdmitError::Cancelled(_reason)) => {
                drop(table);
                drop(_workers);
                return Err(SupervisorError::SpawnCancelled(session_id.to_string()));
            }
        };
        // A stop may have installed its fence while durable admission was
        // being prepared. Abandon this exact Lease before any launch work.
        let request_state = lock_recover(&self.spawn_requests);
        if (request_state.stop_fences.contains_key(session_id) && restart.is_none())
            || cancellation
                .as_ref()
                .is_some_and(CancellationToken::is_cancelled)
        {
            table.abandon(&lease);
            return Err(SupervisorError::SpawnCancelled(session_id.to_string()));
        }
        drop(request_state);
        if kind != ResumeKind::Attach {
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
        if let Some(record) = &attach_record {
            table.note_generation(session_id, record.generation);
            lease.record_runner(RunnerIdentity::from_record(record));
            lease.record_prime(crate::acp::runner_lifecycle::PrimeOwnership::from_record(
                record,
            ));
            lease.record_scope(record.scope.clone());
        }
        Ok(ResumeReservationOutcome::Reserved(ResumeReservation {
            lease,
            expected_lifecycle_generation: None,
            attach_record,
            lifecycle: Arc::clone(&self.lifecycle),
            notify: Arc::clone(&self.worker_notify),
        }))
    }

    /// Spawn body, run under the reservation from `begin_resume`.
    pub(crate) async fn spawn_inner(
        &self,
        mut req: SpawnRequest,
        reservation: ResumeReservation,
    ) -> Result<(), SupervisorError> {
        if self.native_admission_required
            && req.source_profile.is_none()
            && req.launch_admission.is_none()
        {
            return Err(SupervisorError::NativeStoreUnavailable);
        }
        let lease = reservation.lease().clone();
        if req.launch_admission.is_none() {
            match await_resume_to_completion(
                reservation.cancellation(),
                capture_durable_launch(
                    req.source_profile.as_deref(),
                    &req.session_id,
                    Some(req.expected_lifecycle_generation),
                    false,
                ),
            )
            .await
            {
                Ok(Ok((admission, _))) => req.launch_admission = admission,
                Ok(Err(error)) => return Err(error),
                Err(()) => return Err(self.cancelled_spawn(&lease).await),
            }
        }
        lease.set_admission(req.launch_admission.clone());
        match await_resume_to_completion(
            reservation.cancellation(),
            admit_durable_launch(req.launch_admission.as_ref(), &req.session_id),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error),
            Err(()) => return Err(self.cancelled_spawn(&lease).await),
        }
        let session_id = req.session_id.as_str();
        let warmup_guard =
            match await_resume(reservation.cancellation(), self.warmup_guard(&req.agent)).await {
                Ok(guard) => guard,
                Err(()) => return Err(self.cancelled_spawn(&lease).await),
            };
        let (mut config, context_reset) = match await_resume_to_completion(
            reservation.cancellation(),
            self.spawn_config(&req, lease.epoch()),
        )
        .await
        {
            Ok(Ok(config)) => config,
            Ok(Err(error)) => return Err(error),
            Err(()) => {
                drop(warmup_guard);
                return Err(self.cancelled_spawn(&lease).await);
            }
        };
        if !config.uses_native_prime() {
            lease.record_prime(crate::acp::runner_lifecycle::PrimeOwnership::NotPrime);
        }
        config.launch_lease = Some(lease.clone());
        debug!(
            target: "acp.supervisor",
            session = %session_id,
            stored_id = ?config.stored_acp_session_id,
            "spawning structured view worker"
        );
        // The hooks above may re-enter aoe, so the lifecycle lock is taken only for each check.
        match await_resume_to_completion(
            reservation.cancellation(),
            admit_durable_launch(req.launch_admission.as_ref(), &req.session_id),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error),
            Err(()) => {
                drop(warmup_guard);
                return Err(self.cancelled_spawn(&lease).await);
            }
        }
        // Clear a partial replay from a failed import before session/load re-emits it.
        if config.seed_history_replay {
            self.sink.clear_session_events(session_id);
        }

        // The client observes the Lease cancellation during its handshake.
        // Retain its result so retirement can still delete adapter-side state.
        let launched = (self.launcher)(config.clone(), AcpSessionId(session_id.to_string())).await;
        let mut client = match launched {
            Ok(c) => c,
            Err(err) => {
                // A stop that landed during the launch owns the outcome.
                let cancelled = lock_recover(&self.lifecycle)
                    .cancel_requested(&lease)
                    .is_some();
                if cancelled {
                    drop(warmup_guard);
                    return Err(self.cancelled_spawn(&lease).await);
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

        let inbound = client
            .take_inbound()
            .expect("freshly spawned AcpClient always has inbound receiver");
        let client = Arc::new(client);
        lease.retirement().record_client(&client, false);
        if !lease.attest_runner().await {
            let identity = lease.spawned_runner();
            super::teardown::retire_client(&self.lifecycle, &lease, &client, identity).await;
            self.reap_failed_launch(&lease).await;
            return Err(SupervisorError::TeardownPending(session_id.to_owned()));
        }
        let identity = client.runner_pid().map(|pid| {
            lease
                .spawned_runner()
                .or_else(|| {
                    worker_registry::load(session_id)
                        .ok()
                        .flatten()
                        .filter(|record| record.pid == pid && record.generation == lease.epoch())
                        .map(|record| RunnerIdentity::from_record(&record))
                })
                .unwrap_or_else(|| RunnerIdentity::unattested(pid, lease.epoch()))
        });
        // A peer that archived or trashed the row during the handshake wins: retire the runner.
        match admit_durable_launch(req.launch_admission.as_ref(), &req.session_id).await {
            Ok(()) => {}
            Err(refused) => {
                super::teardown::retire_client(&self.lifecycle, &lease, &client, identity).await;
                drop(client);
                drop(warmup_guard);
                self.reap_failed_launch(&lease).await;
                return Err(refused);
            }
        }

        if warmup_guard.is_some() {
            lock_recover(&self.warmed_up_agents).insert(req.agent.clone());
        }
        drop(warmup_guard);
        info!(target: "acp.supervisor", session = %session_id, "structured view worker spawned");
        lock_recover(&self.incompatible_binaries).remove(session_id);

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

    async fn cancelled_spawn(&self, lease: &Lease) -> SupervisorError {
        let session_id = lease.session_id();
        self.reap_failed_launch(lease).await;
        SupervisorError::SpawnCancelled(session_id.to_string())
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
    ) -> Result<(SpawnConfig, Option<PendingContextReset>), SupervisorError> {
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

        let (base_host_environment, mut host_environment) = if req.sandbox_info.is_none() {
            host_spawn_environment(
                &resolved_cfg,
                &req.session_id,
                &req.tool,
                req.source_profile.clone().unwrap_or_default(),
                req.cwd.clone(),
            )
            .await?
        } else {
            (Vec::new(), Vec::new())
        };

        let claude_store_pin = req.claude_store_pin.clone().filter(|_| {
            req.sandbox_info.is_none() && matches!(req.agent.as_str(), "claude" | "claude-code")
        });
        let claude_config_dir =
            apply_claude_store_pin(&mut host_environment, claude_store_pin.as_ref()).or_else(
                || {
                    if req.sandbox_info.is_some() {
                        return None;
                    }
                    let (target, native) = wrapper_substitution.as_ref().map_or(
                        (req.agent.as_str(), req.agent.as_str()),
                        |(wrapper, base)| (wrapper.as_str(), base.as_str()),
                    );
                    apply_declared_config_dir(
                        &mut host_environment,
                        &resolved_cfg,
                        &req.tool,
                        target,
                        native,
                    )
                },
            );

        let mut provider_env = req.provider_env.clone();
        if let Some(model) = model.clone() {
            provider_env.push(("AOE_AGENT_MODEL".into(), model));
        }
        // Every worker runs through `aoe __acp-runner` so it survives `aoe serve --stop`.
        let socket_path = worker_registry::socket_path_for(&req.session_id).map_err(|e| {
            SupervisorError::Acp(AcpError::Spawn(format!("worker socket path: {e:#}")))
        })?;
        let mcp_servers = resolve_mcp_servers(
            &req.agent,
            &req.session_id,
            req.source_profile.clone(),
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
                let profile = req.source_profile.clone().unwrap_or_default();
                let id = req.session_id.clone();
                let continuation = req.sandbox_continuation;
                let context = tokio::task::spawn_blocking(move || {
                    crate::migrations::v033_isolate_sandbox_content::prepare_acp_context(
                        &profile,
                        &id,
                        native_agent,
                        generation,
                        crate::migrations::v033_isolate_sandbox_content::AcpContextUse::Launch,
                        continuation,
                    )
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
                    req.source_profile.clone(),
                )
            };

        Ok((
            SpawnConfig {
                launch_lease: None,
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
                prior_conversation: req.prior_conversation,
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
        reservation: ResumeReservation,
        client: Arc<AcpClient>,
        inbound: mpsc::Receiver<Event>,
        installation: (Option<RunnerIdentity>, Option<PendingContextReset>),
        kind: WorkerKind,
    ) -> Result<Arc<AcpClient>, SupervisorError> {
        let (identity, context_reset) = installation;
        let lease = reservation.lease().clone();
        let mut workers = self.workers.lock().await;
        if worker_registry::stop_requested(session_id) {
            lock_recover(&self.lifecycle).begin_stop(session_id, "user_stopped", false);
        }
        let mut install = lock_recover(&self.lifecycle).install(&lease, identity);
        if install.is_ok() && worker_registry::stop_requested(session_id) {
            lock_recover(&self.lifecycle).begin_stop(session_id, "user_stopped", false);
            install = Err(InstallError::Cancelled {
                reason: "user_stopped".into(),
            });
        }
        if let Err(refusal) = install {
            drop(workers);
            super::teardown::retire_client(&self.lifecycle, &lease, &client, identity).await;
            drop(client);
            return Err(self.retire_refused_install(&lease, refusal).await);
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
        // Tasks follow the same split: an attached worker's tasks may still be
        // running, so their tailers resume; a replaced worker's are finished
        // by the detach.
        let (resumable, open_tasks) = if matches!(kind, WorkerKind::Attached) {
            (
                collect_resumable_background_agent_launches(
                    &*self.sink,
                    &self.next_seqs,
                    session_id,
                ),
                self.sink.unresolved_background_task_launches(session_id),
            )
        } else {
            self.detach_orphaned_background_agents(session_id);
            (Vec::new(), Vec::new())
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
        let client_for_resume =
            (!resumable.is_empty() || !open_tasks.is_empty()).then(|| Arc::clone(&client));
        workers.insert(
            session_id.to_string(),
            WorkerHandle {
                client: Arc::clone(&client),
                native_session_id: None,
                drain_task,
                restart_history: vec![],
                kind,
                launch_epoch: lease.epoch(),
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
            if !resumable.is_empty() {
                let _ = resuming.resume_background_tailing(resumable).await;
            }
            if !open_tasks.is_empty() {
                let _ = resuming.resume_task_tailing(open_tasks).await;
            }
        }
        drop(reservation);
        self.worker_notify.notify_waiters();
        Ok(client)
    }

    /// Retire a runner a failed spawn left behind under this lease's generation
    /// and release the scope it created, while the lease still holds the
    /// session. A first launch has no previous runner to keep. Under a pending
    /// stop the scope is released even when no runner of this generation exists.
    async fn reap_failed_launch(&self, lease: &Lease) {
        if lease.is_borrowed_attach() {
            return;
        }
        let session_id = lease.session_id();
        let identity = lease.spawned_runner().or_else(|| {
            if lock_recover(&self.lifecycle).pending_attach(lease) {
                // An explicit stop owns only the runner captured at admission.
                return None;
            }
            worker_registry::load(session_id)
                .ok()
                .flatten()
                .filter(|record| record.generation == lease.epoch())
                .map(|record| RunnerIdentity::from_record(&record))
        });
        let identity = match identity {
            Some(identity) => identity,
            None if lease.stop_completion_pending() => {
                // abandon parks cancellation-before-birth instead of deleting its debt.
                let abandoned = lock_recover(&self.lifecycle).abandon(lease);
                if abandoned {
                    self.settle_sweeping_if_owed(
                        lease,
                        crate::acp::runner_lifecycle::Settlement::Proven,
                    )
                    .await;
                    return;
                }

                // The producer can acquire the birth lock, record its runner,
                // and release it while abandon waits on that same lock. In
                // that interleaving cancel_before_birth returns false, so the
                // first identity snapshot was stale. Re-read ownership before
                // deciding that there is nothing left to reap.
                let Some(identity) = lease.spawned_runner() else {
                    self.worker_notify.notify_waiters();
                    return;
                };
                identity
            }
            None => return,
        };
        if lease.spawned_runner().is_none() {
            lease.record_runner(identity);
        }
        if !lock_recover(&self.lifecycle).convert_to_stopping(lease) {
            // A newer lifecycle epoch may have displaced this construction
            // while its cancellable launcher was still returning. The stale
            // Lease still owns the runner it built, so retire that runner
            // without touching the newer epoch's scope or lifecycle entry.
            super::teardown::retire_registered_clients(&self.lifecycle, lease).await;
            let _ = super::teardown::tear_down_lease(&*self.process_control, lease, false).await;
            return;
        }
        // Scope debt must exist before any cancellable physical teardown.
        lock_recover(&self.lifecycle).owe_sweep(lease);
        super::teardown::retire_registered_clients(&self.lifecycle, lease).await;
        let settlement =
            super::teardown::tear_down_lease(&*self.process_control, lease, false).await;
        self.settle_sweeping_if_owed(lease, settlement).await;
    }

    /// Tear down a built worker whose install the lifecycle table refused.
    async fn retire_refused_install(
        &self,
        lease: &Lease,
        refusal: InstallError,
    ) -> SupervisorError {
        let session_id = lease.session_id();
        if lease.is_borrowed_attach() {
            return SupervisorError::SpawnCancelled(session_id.to_string());
        }
        match refusal {
            InstallError::Cancelled { reason } => {
                debug!(target: "acp.supervisor", session = %session_id, %reason,
                        "resume cancelled by a concurrent shutdown; retiring matching construction");
            }
            InstallError::Stale => {
                warn!(target: "acp.supervisor", session = %session_id,
                        "resume completed under a stale lease; retiring its frozen runner only");
                lock_recover(&self.lifecycle).convert_to_stopping(lease);
                lock_recover(&self.lifecycle).owe_sweep(lease);
            }
        }
        let settlement =
            super::teardown::tear_down_lease(&*self.process_control, lease, false).await;
        self.settle_sweeping_if_owed(lease, settlement).await;
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
        match self
            .begin_resume(
                &session_id,
                ResumeKind::Attach,
                crate::process::worker_registry::ResumeAuthority::Automatic,
            )
            .await?
        {
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
        mut reservation: ResumeReservation,
    ) -> Result<(), SupervisorError> {
        let record = match reservation.attach_record.take() {
            Some(record) if worker_registry::is_record_live(&record) => record,
            _ => return Err(SupervisorError::UnknownSession(session_id)),
        };
        let (admission, claude_home) = capture_durable_launch(
            record.source_profile.as_deref(),
            &session_id,
            reservation.expected_lifecycle_generation,
            true,
        )
        .await?;
        admit_durable_launch(admission.as_ref(), &session_id).await?;
        let identity = RunnerIdentity::from_record(&record);

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
            reservation.lease().claim_runner_ownership();
            self.reap_failed_launch(reservation.lease()).await;
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
        let context_reset = if sandbox.as_ref().is_some_and(|info| info.enabled) {
            let profile = record.source_profile.clone().unwrap_or_default();
            let id = session_id.clone();
            let native_agent = if record.prime_runtime.is_some() {
                Some("prime-agent")
            } else {
                crate::acp::agent_profiles::resolve(&agent_key).native_config_agent
            };
            let generation = reservation.lease().epoch();
            let context = tokio::task::spawn_blocking(move || {
                crate::migrations::v033_isolate_sandbox_content::prepare_acp_context(
                    &profile,
                    &id,
                    native_agent,
                    generation,
                    crate::migrations::v033_isolate_sandbox_content::AcpContextUse::Attach,
                    super::SandboxContinuation::Persisted,
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
                    reason,
                    transactions,
                })
        } else {
            None
        };

        let client = await_resume(
            reservation.cancellation(),
            AcpClient::attach(
                record.socket_path.clone(),
                cwd,
                additional_dirs,
                stored_acp_session_id,
                in_flight_turn,
                AcpSessionId(session_id.clone()),
                sandbox_resources,
                agent_key,
                record.source_profile.clone(),
                Some(tokio::time::Instant::now() + std::time::Duration::from_secs(3)),
                Some(reservation.lease().clone()),
                claude_home,
            ),
        )
        .await;
        let mut client = match client {
            Ok(Ok(client)) => client,
            Ok(Err(error)) => {
                self.reap_failed_launch(reservation.lease()).await;
                return Err(error.into());
            }
            Err(()) => return Err(self.cancelled_spawn(reservation.lease()).await),
        };

        let inbound = client
            .take_inbound()
            .expect("freshly attached AcpClient always has inbound receiver");
        let client = Arc::new(client);
        reservation
            .lease()
            .retirement()
            .record_client(&client, false);
        if let Err(refused) = admit_durable_launch(admission.as_ref(), &session_id).await {
            let lease = reservation.lease().clone();
            super::teardown::retire_client(&self.lifecycle, &lease, &client, Some(identity)).await;
            drop(client);
            self.reap_failed_launch(&lease).await;
            return Err(refused);
        }
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

async fn await_resume<T>(
    cancellation: &CancellationToken,
    future: impl Future<Output = T>,
) -> Result<T, ()> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(()),
        result = future => Ok(result),
    }
}

/// Cancellation stops launch admission, but a started blocking commit must finish
/// before its caller can release the old generation's ownership.
pub(super) async fn await_resume_to_completion<T>(
    cancellation: &CancellationToken,
    future: impl Future<Output = T>,
) -> Result<T, ()> {
    if cancellation.is_cancelled() {
        return Err(());
    }
    tokio::pin!(future);
    tokio::select! {
        biased;
        result = &mut future => Ok(result),
        _ = cancellation.cancelled() => {
            let _ = future.await;
            Err(())
        }
    }
}

async fn capture_durable_launch(
    profile: Option<&str>,
    session_id: &str,
    expected: Option<u64>,
    capture_store: bool,
) -> Result<(Option<super::LaunchAdmission>, Option<PathBuf>), SupervisorError> {
    let Some(profile) = profile.map(str::to_owned) else {
        return Ok((None, None));
    };
    let id = session_id.to_owned();
    tokio::task::spawn_blocking(move || {
        let storage = crate::session::Storage::open_unwatched(&profile)
            .map_err(|error| SupervisorError::Acp(AcpError::Spawn(error.to_string())))?;
        let _lock = storage
            .acquire_instance_lifecycle_lock(&id)
            .map_err(|error| SupervisorError::Acp(AcpError::Spawn(error.to_string())))?;
        let row = storage
            .load()
            .map_err(|error| SupervisorError::Acp(AcpError::Spawn(error.to_string())))?
            .into_iter()
            .find(|row| row.id == id)
            .ok_or_else(|| SupervisorError::SessionGone(id.clone()))?;
        let claude_home = if capture_store {
            row.selected_claude_conversation()
                .and_then(|(_, binding)| binding.stores.first())
                .cloned()
        } else {
            None
        };
        Ok((
            Some(super::LaunchAdmission {
                store: std::sync::Arc::new(storage),
                generation: expected.unwrap_or(row.lifecycle_generation),
                namespace: None,
            }),
            claude_home,
        ))
    })
    .await
    .map_err(|error| SupervisorError::Acp(AcpError::Spawn(error.to_string())))?
}

fn admission_error(error: anyhow::Error, session_id: &str) -> SupervisorError {
    if let Some(blocked) = error.downcast_ref::<crate::session::StartBlocked>() {
        SupervisorError::Blocked(*blocked)
    } else if error.is::<crate::session::SessionGone>() {
        SupervisorError::SessionGone(session_id.to_owned())
    } else if error.is::<crate::session::LifecycleReservationError>() {
        SupervisorError::SpawnCancelled(session_id.to_owned())
    } else if error.is::<crate::session::NativeStoreUnavailable>() {
        SupervisorError::NativeStoreUnavailable
    } else {
        SupervisorError::Acp(AcpError::Spawn(format!("launch admission: {error:#}")))
    }
}

/// Admit a launch with no captured admission, such as a crash respawn, against the row as stored now.
pub(super) async fn admit_stored_row(
    profile: Option<&str>,
    session_id: &str,
) -> Result<(), SupervisorError> {
    let (admission, _) = capture_durable_launch(profile, session_id, None, false).await?;
    admit_durable_launch(admission.as_ref(), session_id).await
}

/// Check the same authority and generation before hooks, after hooks and after handshake.
async fn admit_durable_launch(
    admission: Option<&super::LaunchAdmission>,
    session_id: &str,
) -> Result<(), SupervisorError> {
    with_durable_launch(admission.cloned(), session_id.to_owned(), || Ok(())).await
}

/// The blocking producer owns the native birth check, child spawn and publication.
pub(crate) async fn with_durable_launch<T: Send + 'static>(
    admission: Option<super::LaunchAdmission>,
    session_id: String,
    operation: impl FnOnce() -> Result<T, AcpError> + Send + 'static,
) -> Result<T, SupervisorError> {
    let Some(mut admission) = admission else {
        return operation().map_err(SupervisorError::Acp);
    };
    let namespace = match admission.namespace.take() {
        Some(namespace) => Some(namespace.read_owned().await),
        None => None,
    };
    tokio::task::spawn_blocking(move || {
        let _namespace = namespace;
        let _lock = admission
            .store
            .storage()
            .acquire_instance_lifecycle_lock(&session_id)
            .map_err(|error| admission_error(error, &session_id))?;
        let stored = admission
            .store
            .load()
            .map_err(|error| admission_error(error, &session_id))?
            .into_iter()
            .find(|row| row.id == session_id);
        match stored {
            None => Err(SupervisorError::SessionGone(session_id)),
            Some(row) => {
                row.ensure_startable().map_err(SupervisorError::Blocked)?;
                if row.lifecycle_generation != admission.generation
                    || row.status == crate::session::Status::Stopped
                    || row.is_idle_dormant()
                {
                    return Err(SupervisorError::SpawnCancelled(session_id));
                }
                if row.terminal_handoff.is_some()
                    || !row.is_structured()
                    || !row.launch_is_finalized()
                {
                    return Err(SupervisorError::Acp(AcpError::Spawn(
                        "structured session launch is not finalized".into(),
                    )));
                }
                operation().map_err(SupervisorError::Acp)
            }
        }
    })
    .await
    .map_err(|error| SupervisorError::Acp(AcpError::Spawn(error.to_string())))?
}

/// Unsandboxed host environment as `(base, host)`: trusted global/profile `[environment]` pairs
/// (repo overrides cannot contribute them), then `host` overlays the before_session hook output.
pub(crate) async fn host_spawn_environment(
    cfg: &crate::session::Config,
    session_id: &str,
    tool: &str,
    profile: String,
    cwd: PathBuf,
) -> Result<(Vec<(String, String)>, Vec<(String, String)>), SupervisorError> {
    let base = crate::session::environment::resolve_host_environment_pairs(&cfg.environment);
    let mut host = base.clone();
    if !cfg.host_hooks.before_session.is_empty() {
        let minted = before_session_env(session_id, tool, profile, cwd)
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
/// Exports the selected target's declared `agent_config_dir` as its native agent's config-dir
/// variable, as a terminal launch does. `target` is the selected backend's own key (a wrapper's
/// name, not its base) and `native` the backend it resolves to. Returns the directory when the
/// agent is Claude, for native MCP discovery.
pub(super) fn apply_declared_config_dir(
    environment: &mut Vec<(String, String)>,
    cfg: &crate::session::Config,
    tool: &str,
    target: &str,
    native: &str,
) -> Option<std::path::PathBuf> {
    let session = &cfg.session;
    let native_of = |name: &str| crate::acp::agent_profiles::resolve(name).native_config_agent;
    // After an agent switch `tool` is unchanged but may name another backend: the tool's entry
    // applies only to a target it launches itself (same key, a built-in alias, or its base).
    let tool_runs_target = target == tool
        || native_of(target).is_some_and(|built_in| {
            native_of(tool) == Some(built_in)
                || [&session.agent_execution_as, &session.agent_detect_as]
                    .iter()
                    .any(|map| map.get(tool).map(String::as_str) == Some(native))
        });
    let key = if tool_runs_target { tool } else { target };
    let native = session
        .agent_execution_as
        .get(key)
        .map_or(native, String::as_str);
    let variable = match native_of(native)? {
        "claude" => "CLAUDE_CONFIG_DIR",
        "codex" => "CODEX_HOME",
        "opencode" => "OPENCODE_CONFIG_DIR",
        "pi" => "PI_CODING_AGENT_DIR",
        _ => return None,
    };
    let home = environment
        .iter()
        .rev()
        .find(|(name, value)| name == "HOME" && !value.is_empty())
        .map(|(_, value)| value.clone())
        .or_else(|| {
            crate::session::environment::resolve_host_environment_value(&cfg.environment, "HOME")
        })
        .map(std::path::PathBuf::from)
        .or_else(dirs::home_dir)
        .filter(|home| home.is_absolute())?;
    let dir = session.agent_config_dir_for(key, &home)?;
    if variable == "CLAUDE_CONFIG_DIR" {
        // Setting the variable moves `.claude.json` into the store, so an implicit default stays unset.
        let ambient = environment
            .iter()
            .rev()
            .find(|(name, _)| name == variable)
            .map(|(_, value)| value.clone())
            .or_else(|| std::env::var(variable).ok())
            .filter(|value| !value.is_empty())
            .map(std::path::PathBuf::from);
        if crate::session::capture::is_default_claude_store(&dir, &home)
            && !crate::session::capture::is_explicit_claude_store_route(
                &dir,
                &home,
                Some(&dir),
                ambient.as_deref(),
            )
        {
            return None;
        }
    }
    overlay_env(
        environment,
        vec![(variable.into(), dir.to_string_lossy().into_owned())],
    );
    (variable == "CLAUDE_CONFIG_DIR").then_some(dir)
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
    crate::acp::mcp_config::project_servers_to_acp(merged, session_env)
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

    #[tokio::test]
    async fn cancellation_waits_for_started_work_to_finish() {
        let cancellation = CancellationToken::new();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let token = cancellation.clone();
        let mut work = tokio::spawn(async move {
            await_resume_to_completion(&token, async {
                started_tx.send(()).unwrap();
                release_rx.await.unwrap();
            })
            .await
        });
        started_rx.await.unwrap();
        cancellation.cancel();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut work)
                .await
                .is_err()
        );
        release_tx.send(()).unwrap();
        assert!(work.await.unwrap().is_err());
    }

    #[tokio::test]
    #[serial_test::serial]
    #[cfg(unix)]
    async fn lease_cancellation_releases_a_real_stalled_handshake() -> anyhow::Result<()> {
        let _home = isolate_home();
        let project = tempfile::tempdir()?;
        let ready = project.path().join("request-received");
        let mut lifecycle = crate::acp::runner_lifecycle::LifecycleTable::new(0);
        let lease = lifecycle
            .admit("stalled-handshake", ResumeKind::SpawnFirst)
            .expect("fresh launch admission");
        let mut config = runner_config(project.path().join("unused.sock"));
        config.socket_path = None;
        config.launch_lease = Some(lease.clone());
        config.spec.command = "python3".into();
        config.spec.args = vec![
            "-c".into(),
            r#"import json, os, sys, time
request = json.loads(sys.stdin.readline())
assert request['method'] == 'initialize'
with open(sys.argv[1], 'w') as marker:
    marker.write(str(os.getpid()))
time.sleep(60)
"#
            .into(),
            ready.to_string_lossy().into_owned(),
        ];
        let spawning = tokio::spawn(AcpClient::spawn(
            config,
            AcpSessionId("stalled-handshake".into()),
        ));
        let pid = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(pid) = std::fs::read_to_string(&ready)
                    .unwrap_or_default()
                    .parse::<u32>()
                {
                    break pid;
                }
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert!(worker_registry::is_pid_alive(pid));
        lease.cancellation().cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), spawning).await??;
        assert!(result.is_err());
        assert!(!worker_registry::is_pid_alive(pid));
        Ok(())
    }

    #[tokio::test]
    async fn ordinary_spawn_is_rejected_while_restart_claim_is_held() {
        let supervisor = Supervisor::new(VecSink::new());
        let _restart = supervisor
            .try_begin_restart("s-restart")
            .await
            .expect("restart claims the session");

        let result = supervisor
            .spawn(
                spawn_request("s-restart"),
                crate::process::worker_registry::ResumeAuthority::Explicit,
            )
            .await;

        assert!(matches!(
            result,
            Err(SupervisorError::RestartInProgress(id)) if id == "s-restart"
        ));
        assert_eq!(
            supervisor.worker_state("s-restart").await,
            AcpWorkerState::Absent
        );
    }

    #[tokio::test]
    async fn cancelled_restart_claim_does_not_admit_a_replacement() {
        let supervisor = Supervisor::new(VecSink::new());
        let restart = supervisor
            .try_begin_restart("s-restart")
            .await
            .expect("restart claims the session");

        supervisor.cancel_restart_request("s-restart").await;
        let result = supervisor
            .spawn_for_restart(
                spawn_request("s-restart"),
                crate::process::worker_registry::ResumeAuthority::Explicit,
                &restart,
            )
            .await;

        assert!(matches!(result, Err(SupervisorError::SpawnCancelled(id)) if id == "s-restart"));
        assert_eq!(
            supervisor.worker_state("s-restart").await,
            AcpWorkerState::Absent
        );
    }

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
    async fn host_spawn_environment_forwards_configured_pairs() {
        let cfg = crate::session::Config {
            environment: vec![
                "PI_CODING_AGENT_DIR=/stores/pi".into(),
                "PI_ACP_PI_COMMAND=pi-wrapper".into(),
            ],
            ..Default::default()
        };
        let (base, host) =
            host_spawn_environment(&cfg, "s", "pi", String::new(), PathBuf::from("/"))
                .await
                .unwrap();
        let expected = vec![
            ("PI_CODING_AGENT_DIR".to_string(), "/stores/pi".to_string()),
            ("PI_ACP_PI_COMMAND".to_string(), "pi-wrapper".to_string()),
        ];
        assert_eq!(base, expected);
        assert_eq!(host, expected);
    }

    #[test]
    fn declared_config_dir_is_exported_under_each_agents_variable() {
        let cfg = |entries: &[(&str, &str)]| {
            let mut cfg = crate::session::Config::default();
            for (tool, dir) in entries {
                cfg.session
                    .agent_config_dir
                    .insert((*tool).into(), (*dir).into());
            }
            cfg
        };
        let run = |cfg: &crate::session::Config, tool: &str, agent: &str| {
            let mut env = vec![("CLAUDE_CONFIG_DIR".to_string(), "/hook".to_string())];
            let claude_dir = apply_declared_config_dir(&mut env, cfg, tool, agent, agent);
            (env, claude_dir)
        };
        let pair = |key: &str, value: &str| (key.to_string(), value.to_string());

        // (config, tool, agent, expected env, expected Claude dir)
        let cases = [
            // A wrapper reaches its base through `agent_detect_as`.
            (
                {
                    let mut cfg = cfg(&[("claude-c", "/acct")]);
                    cfg.session
                        .agent_detect_as
                        .insert("claude-c".into(), "claude".into());
                    cfg
                },
                "claude-c",
                "claude",
                vec![pair("CLAUDE_CONFIG_DIR", "/acct")],
                Some(PathBuf::from("/acct")),
            ),
            (
                cfg(&[("codex", "/acct")]),
                "codex",
                "codex",
                vec![
                    pair("CLAUDE_CONFIG_DIR", "/hook"),
                    pair("CODEX_HOME", "/acct"),
                ],
                None,
            ),
            (
                cfg(&[("opencode", "/acct")]),
                "opencode",
                "opencode",
                vec![
                    pair("CLAUDE_CONFIG_DIR", "/hook"),
                    pair("OPENCODE_CONFIG_DIR", "/acct"),
                ],
                None,
            ),
            (
                cfg(&[("pi", "/acct")]),
                "pi",
                "pi",
                vec![
                    pair("CLAUDE_CONFIG_DIR", "/hook"),
                    pair("PI_CODING_AGENT_DIR", "/acct"),
                ],
                None,
            ),
            // A custom tool's declared native identity selects the variable.
            (
                {
                    let mut cfg = cfg(&[("my-claude", "/acct")]);
                    cfg.session
                        .agent_execution_as
                        .insert("my-claude".into(), "claude".into());
                    cfg
                },
                "my-claude",
                "my-claude",
                vec![pair("CLAUDE_CONFIG_DIR", "/acct")],
                Some(PathBuf::from("/acct")),
            ),
            // After a switch the tool's entry is not the new backend's: use the backend's own.
            (
                cfg(&[("claude", "/acct"), ("codex", "/codex")]),
                "claude",
                "codex",
                vec![
                    pair("CLAUDE_CONFIG_DIR", "/hook"),
                    pair("CODEX_HOME", "/codex"),
                ],
                None,
            ),
            (
                cfg(&[("claude", "/acct")]),
                "claude",
                "codex",
                vec![pair("CLAUDE_CONFIG_DIR", "/hook")],
                None,
            ),
            (
                {
                    let mut cfg = cfg(&[("my-claude", "/acct")]);
                    cfg.session
                        .agent_execution_as
                        .insert("my-claude".into(), "claude".into());
                    cfg
                },
                "my-claude",
                "codex",
                vec![pair("CLAUDE_CONFIG_DIR", "/hook")],
                None,
            ),
            // No entry for the tool, and an agent with no config-dir variable.
            (
                cfg(&[("other", "/acct")]),
                "claude",
                "claude",
                vec![pair("CLAUDE_CONFIG_DIR", "/hook")],
                None,
            ),
            (
                cfg(&[("gemini", "/acct")]),
                "gemini",
                "gemini",
                vec![pair("CLAUDE_CONFIG_DIR", "/hook")],
                None,
            ),
        ];
        for (cfg, tool, agent, env, claude_dir) in cases {
            assert_eq!(run(&cfg, tool, agent), (env, claude_dir), "{tool}");
        }

        // Switching into a custom target selects the target's own key and identity.
        let into = |cfg: &crate::session::Config, tool: &str, target: &str, native: &str| {
            let mut env = vec![];
            let claude_dir = apply_declared_config_dir(&mut env, cfg, tool, target, native);
            (env, claude_dir)
        };
        let mut explicit = cfg(&[
            ("claude", "/acct"),
            ("work-codex", "/work"),
            ("codex", "/c"),
        ]);
        explicit
            .session
            .agent_execution_as
            .insert("work-codex".into(), "codex".into());
        assert_eq!(
            into(&explicit, "claude", "work-codex", "work-codex"),
            (vec![pair("CODEX_HOME", "/work")], None)
        );
        let mut inherited = cfg(&[
            ("claude", "/acct"),
            ("work-codex", "/work"),
            ("codex", "/c"),
        ]);
        inherited
            .session
            .agent_detect_as
            .insert("work-codex".into(), "codex".into());
        assert_eq!(
            into(&inherited, "claude", "work-codex", "codex"),
            (vec![pair("CODEX_HOME", "/work")], None)
        );
        // An unrelated wrapper of the tool's own native agent keeps its own key too.
        let mut sibling = cfg(&[("claude", "/acct"), ("claude-d", "/d")]);
        sibling
            .session
            .agent_detect_as
            .insert("claude-d".into(), "claude".into());
        assert_eq!(
            into(&sibling, "claude", "claude-d", "claude"),
            (
                vec![pair("CLAUDE_CONFIG_DIR", "/d")],
                Some(PathBuf::from("/d"))
            )
        );

        // The exact default store is implicit unless an ambient route names it.
        let default_store = cfg(&[("claude", "~/.claude")]);
        let mut env = vec![pair("HOME", "/home/u")];
        assert_eq!(
            apply_declared_config_dir(&mut env, &default_store, "claude", "claude", "claude"),
            None
        );
        assert_eq!(env, vec![pair("HOME", "/home/u")]);
        let mut env = vec![
            pair("HOME", "/home/u"),
            pair("CLAUDE_CONFIG_DIR", "/home/u/.claude"),
        ];
        assert_eq!(
            apply_declared_config_dir(&mut env, &default_store, "claude", "claude", "claude"),
            Some(PathBuf::from("/home/u/.claude"))
        );

        // A relative effective HOME would resolve differently per process: no export.
        let mut env = vec![pair("HOME", "account")];
        let relative_home = cfg(&[("claude", "~/.claude-work")]);
        assert_eq!(
            apply_declared_config_dir(&mut env, &relative_home, "claude", "claude", "claude"),
            None
        );
        assert_eq!(env, vec![pair("HOME", "account")]);

        // `~` expands against the worker's effective HOME, not the configured one.
        let mut cfg = cfg(&[("claude", "~/.claude-work")]);
        cfg.environment = vec!["HOME=/home/operator".into()];
        let mut env = vec![pair("HOME", "/srv/account")];
        let claude_dir = apply_declared_config_dir(&mut env, &cfg, "claude", "claude", "claude");
        assert_eq!(claude_dir, Some(PathBuf::from("/srv/account/.claude-work")));
        assert_eq!(
            env,
            vec![
                pair("HOME", "/srv/account"),
                pair("CLAUDE_CONFIG_DIR", "/srv/account/.claude-work"),
            ]
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn fresh_session_exports_declared_config_dir_and_a_recorded_store_outranks_it() {
        let (_home, temp) = isolate_home();
        let declared = temp.path().join("declared");
        let recorded = temp.path().join("recorded");
        std::fs::create_dir_all(&declared).unwrap();
        std::fs::create_dir_all(&recorded).unwrap();
        let app_dir = crate::session::get_app_dir().unwrap();
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("config.toml"),
            format!(
                "[session.agent_config_dir]\nclaude-code = \"{}\"\n",
                declared.display()
            ),
        )
        .unwrap();
        let exported = |config: &SpawnConfig| {
            config
                .host_environment
                .iter()
                .find(|(key, _)| key == "CLAUDE_CONFIG_DIR")
                .map(|(_, value)| PathBuf::from(value))
        };

        let supervisor = Supervisor::new(VecSink::new());
        let mut request = spawn_request("fresh-declared");
        let (config, _) = supervisor.spawn_config(&request, 1).await.unwrap();
        assert_eq!(exported(&config), Some(declared));

        request.claude_store_pin = Some(crate::session::capture::ClaudeStorePin {
            store: recorded.clone(),
            exported_default_store: Some(false),
        });
        let (config, _) = supervisor.spawn_config(&request, 1).await.unwrap();
        assert_eq!(exported(&config), Some(recorded));
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
        let (config, context_reset) = supervisor.spawn_config(&request, 1).await.unwrap();
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
        let (config, _) = supervisor.spawn_config(&request, 1).await.unwrap();
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
            tokio::spawn(async move {
                sup.spawn(
                    req,
                    crate::process::worker_registry::ResumeAuthority::Explicit,
                )
                .await
            })
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

    /// #4116: the handshake holds no lifecycle lock, so an archive (which a TUI takes on its
    /// input thread) commits without waiting; the post-handshake recheck retires the runner.
    #[tokio::test]
    #[serial_test::serial]
    async fn spawn_retires_a_runner_whose_row_was_archived_during_the_handshake() {
        let _home = isolate_home();
        let control = Arc::new(FakeProcessControl::default());
        control.alive(4545);
        let gate = Gate::default();
        let sup = Arc::new(
            Supervisor::new(VecSink::new())
                .with_process_control(control.clone())
                .with_launcher(gated_launcher(&gate, 4545)),
        );
        let mut inst = crate::session::Instance::new("s-archived", "/tmp");
        inst.id = "s-archived".into();
        inst.view = crate::session::View::Structured;
        let storage = crate::session::Storage::new_unwatched(&inst.source_profile).unwrap();
        storage
            .update(|rows, _| {
                *rows = vec![inst.clone()];
                Ok(())
            })
            .unwrap();
        let mut req = spawn_request("s-archived");
        req.source_profile = Some(inst.source_profile.clone());
        let spawner = {
            let sup = Arc::clone(&sup);
            tokio::spawn(async move {
                sup.spawn(
                    req,
                    crate::process::worker_registry::ResumeAuthority::Explicit,
                )
                .await
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified())
            .await
            .expect("structured launcher must reach its handshake");
        let original = sup
            .capture_retirement("s-archived")
            .unwrap()
            .identity
            .expect("the producer records proof before the handshake gate");
        assert_eq!(original, attested_test_identity(4545, original.generation));
        assert!(
            worker_registry::load_strict("s-archived")
                .unwrap()
                .is_none(),
            "producer proof must not come from a later registry record"
        );

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
                    rows[0].archive();
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
        assert_eq!(sup.worker_state("s-archived").await, AcpWorkerState::Absent);
        assert!(
            control.signals().iter().any(|(pid, _)| *pid == 4545),
            "the runner launched for the archived row must be torn down"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn cancelled_terminal_handoff_cannot_forget_a_pending_runner() {
        use crate::acp::runner_lifecycle::ProcessControl;
        let _home = isolate_home();
        let control = Arc::new(FakeProcessControl::default());
        control.alive(4646);
        let gate = Gate::default();
        let sup = Arc::new(
            Supervisor::new(VecSink::new())
                .with_process_control(control.clone())
                .with_launcher(gated_launcher(&gate, 4646)),
        );
        let mut row = crate::session::Instance::new("s-terminal-handoff", "/tmp");
        row.id = "s-terminal-handoff".into();
        row.view = crate::session::View::Structured;
        let storage = crate::session::Storage::new_unwatched(&row.source_profile).unwrap();
        storage
            .update(|rows, _| {
                *rows = vec![row.clone()];
                Ok(())
            })
            .unwrap();
        let mut request = spawn_request(&row.id);
        request.source_profile = Some(row.source_profile.clone());
        let spawner = {
            let sup = Arc::clone(&sup);
            tokio::spawn(async move {
                sup.spawn(
                    request,
                    crate::process::worker_registry::ResumeAuthority::Explicit,
                )
                .await
            })
        };
        gate.entered.notified().await;
        let original = sup
            .capture_retirement(&row.id)
            .unwrap()
            .identity
            .expect("the producer records proof before the handshake gate");
        assert_eq!(original, attested_test_identity(4646, original.generation));
        assert!(
            worker_registry::load_strict(&row.id).unwrap().is_none(),
            "producer proof must not come from a later registry record"
        );
        storage
            .update(|rows, _| {
                rows[0].view = crate::session::View::Terminal;
                rows[0].lifecycle_generation += 1;
                Ok(())
            })
            .unwrap();
        sup.shutdown(&row.id).await.unwrap();
        sup.forget_session(&row.id)
            .expect_err("a pending producer retains its matching teardown authority");
        gate.open.notify_one();
        assert!(matches!(
            spawner.await.unwrap(),
            Err(SupervisorError::SpawnCancelled(_))
        ));
        assert!(
            control.signals().contains(&(4646, "TERM")),
            "cancelled handoff must retain the matching teardown lease"
        );
        assert!(!control.is_alive(original));
        assert!(worker_registry::load(&row.id).unwrap().is_none());
        assert_eq!(sup.worker_state(&row.id).await, AcpWorkerState::Absent);
        assert_eq!(stopped_reasons(&sup.sink, &row.id), ["user_stopped"]);
        sup.retry_pending_teardowns().await;
        assert_eq!(stopped_reasons(&sup.sink, &row.id), ["user_stopped"]);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn spawn_rejects_unknown_agents_and_running_sessions() {
        let _home = isolate_home();
        let sup = Supervisor::new(VecSink::new());
        let mut req = spawn_request("s-1");
        req.agent = "no-such-agent".into();
        assert!(matches!(
            sup.spawn(
                req,
                crate::process::worker_registry::ResumeAuthority::Explicit
            )
            .await,
            Err(SupervisorError::UnknownAgent(_))
        ));

        sup.test_insert_worker("s-1").await;
        assert!(matches!(
            sup.spawn(
                spawn_request("s-1"),
                crate::process::worker_registry::ResumeAuthority::Explicit
            )
            .await,
            Err(SupervisorError::AlreadyRunning(_))
        ));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn capacity_counts_workers_detached_runners_and_pending_spawns() {
        let (_home, _tmp) = isolate_home();

        let sup = Supervisor::with_capacity(VecSink::new(), 1);
        sup.test_insert_worker("s-1").await;
        match sup
            .spawn(
                spawn_request("s-2"),
                crate::process::worker_registry::ResumeAuthority::Explicit,
            )
            .await
        {
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
        match sup
            .spawn(
                spawn_request("fresh"),
                crate::process::worker_registry::ResumeAuthority::Explicit,
            )
            .await
        {
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
                ResumeKind::Spawn,
                crate::process::worker_registry::ResumeAuthority::Automatic,
            )
            .await,
        );
        let _attach = reserve(
            sup.begin_resume(
                "s-attach",
                ResumeKind::Attach,
                crate::process::worker_registry::ResumeAuthority::Automatic,
            )
            .await,
        );
        let _b = reserve(
            sup.begin_resume(
                "s-b",
                ResumeKind::Spawn,
                crate::process::worker_registry::ResumeAuthority::Automatic,
            )
            .await,
        );
        match sup
            .begin_resume(
                "s-c",
                ResumeKind::Spawn,
                crate::process::worker_registry::ResumeAuthority::Automatic,
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
                Ok(client.with_runner_pid(4345))
            })
        });
        let control = Arc::new(FakeProcessControl::default());
        control.alive(4345);
        let sup = Supervisor::new(sink)
            .with_process_control(control)
            .with_launcher(launcher);
        sup.hydrate_seqs(store.all_session_seqs());
        sup.spawn(
            spawn_request("s-startup"),
            crate::process::worker_registry::ResumeAuthority::Explicit,
        )
        .await
        .unwrap();
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
        sup.spawn(
            spawn_request("s-startup"),
            crate::process::worker_registry::ResumeAuthority::Explicit,
        )
        .await
        .unwrap();
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

    /// Reaps the runner and aborts its handshake, including when attach fails.
    struct AttachBackgroundAgentFixture {
        store: Arc<crate::acp::event_store::EventStore>,
        rx: tokio::sync::broadcast::Receiver<crate::server::AcpBroadcastFrame>,
        supervisor: Supervisor<super::super::ChannelSink>,
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

    #[tokio::test]
    #[serial_test::serial]
    async fn cancelled_attach_retires_only_its_recorded_scope() {
        use crate::acp::control_protocol::{self, ControlBody};
        let (_home, tmp) = isolate_home();
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let id = "cancelled-scope-attach";
        let socket = tmp.path().join("attach.sock");
        let listener =
            tokio::net::UnixListener::bind(crate::process::worker::control_socket_sibling(&socket))
                .unwrap();
        let (entered, observed) = tokio::sync::oneshot::channel();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            control_protocol::write_frame(
                &mut stream,
                &ControlBody::Hello {
                    control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
                    session_id: id.into(),
                },
            )
            .await
            .unwrap();
            loop {
                if matches!(
                    control_protocol::read_frame(&mut stream).await.unwrap(),
                    Some(ControlBody::Initialize { .. })
                ) {
                    entered.send(()).unwrap();
                    break;
                }
            }
            let _ = control_protocol::read_frame(&mut stream).await;
        });
        let mut record = worker_registry::WorkerRecord::new(
            id.into(),
            pid,
            socket,
            "test-agent-acp".into(),
            "test-agent".into(),
            tmp.path().to_path_buf(),
            None,
            vec![],
            vec![],
            Some("original-native-id".into()),
            None,
        );
        record.runner_identity = crate::process::process_birth_identity(pid).unwrap();
        let unit = "aoe-original-attach.scope";
        record.scope = crate::process::worker_registry::ScopeOwnership::Owned { unit: unit.into() };
        worker_registry::save(&record).unwrap();
        let control = Arc::new(FakeProcessControl::default());
        control.alive(pid);
        control.fail_scope(unit);
        let sup = Arc::new(Supervisor::new(VecSink::new()).with_process_control(control.clone()));
        let attaching = tokio::spawn({
            let sup = sup.clone();
            let cwd = tmp.path().to_path_buf();
            async move { sup.attach(id.into(), cwd, vec![], false, None).await }
        });
        observed.await.unwrap();
        sup.shutdown(id).await.unwrap();
        assert!(attaching.await.unwrap().is_err());
        assert!(control.scope_units().iter().any(|scope| scope == unit));
        assert!(
            sup.is_owned(id).await,
            "failed original scope release retains its lease"
        );
        control.exit_scope(unit);
        sup.retry_pending_teardowns().await;
        assert!(!sup.is_owned(id).await);
        peer.abort();
        drop(child);
    }

    /// One `attach` fixture for the survivor and untrackable cases below: a
    /// fake runner behind a control socket, an unresolved
    /// `BackgroundAgentLaunched` (+`Progress`) already on disk, and the
    /// connection left by a successful `attach`. `output_file` is the launch
    /// payload's transcript path, empty for the untrackable case. `open_task`
    /// also leaves a `BackgroundTaskStarted` of that id on disk.
    async fn attach_with_orphaned_background_agent(
        session_id: &str,
        output_file: &str,
        open_task: Option<&str>,
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
        if let Some(task_id) = open_task {
            let store = tmp.path().join("captured-claude");
            let project = store.join("projects/-enc");
            std::fs::create_dir_all(&project).unwrap();
            std::fs::write(project.join("acp-sid.jsonl"), "").unwrap();
            let mut row =
                crate::session::Instance::new("resumed task", tmp.path().to_str().unwrap());
            row.id = session_id.into();
            row.source_profile = "default".into();
            row.tool = "claude".into();
            row.view = crate::session::View::Structured;
            row.agent_session_id = Some("acp-sid".into());
            row.acp_session_id = Some("acp-sid".into());
            let binding = crate::session::ConversationBinding {
                session_id: "acp-sid".into(),
                execution: Some(crate::session::ExecutionBinding {
                    agent: "claude".into(),
                    stores: vec![store],
                    configuration: Vec::new(),
                    cwd: tmp.path().to_path_buf(),
                    cwd_filesystem: "host".into(),
                    filesystem: "host".into(),
                    exported_default_store: Some(false),
                }),
                provenance: crate::session::ConversationProvenance::Observed,
                transcript_path: None,
            };
            row.agent_session_binding = Some(binding.clone());
            row.resume_intent = crate::session::ResumeIntent::Use("acp-sid".into());
            row.resume_binding = Some(binding);
            crate::session::Storage::new_unwatched("default")
                .unwrap()
                .update(|rows, _| {
                    rows.push(row);
                    Ok(())
                })
                .unwrap();
            sink.publish(
                session_id,
                3,
                &Event::BackgroundTaskStarted {
                    task_id: task_id.into(),
                    tool_call_id: Some("toolu_task".into()),
                    description: "sleep".into(),
                },
            );
        }

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
        let mut record = worker_registry::WorkerRecord::new(
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
        if open_task.is_some() {
            record.source_profile = Some("default".into());
        }
        worker_registry::save(&record).unwrap();

        let sup = Supervisor::new(sink);
        sup.hydrate_seqs(store.all_session_seqs());
        let fixture = AttachBackgroundAgentFixture {
            store,
            rx,
            supervisor: sup,
            runner_handshake,
            fake_runner,
            _tmp: tmp,
            _store_tmp,
            _home,
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            fixture.supervisor.attach(
                session_id.to_string(),
                fixture._tmp.path().to_path_buf(),
                vec![],
                false,
                None,
            ),
        )
        .await
        .expect("attach must not hang")
        .expect("attach must succeed against the fake runner");
        fixture
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
        for (seq, event) in fixture.store.replay_from(session_id, 0) {
            state.apply_event(seq, event).unwrap();
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
            None,
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

        let mut fixture =
            attach_with_orphaned_background_agent("s-attach-untrackable", "", None).await;
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

    /// A task left open when the daemon restarted may have ended meanwhile:
    /// `attach` resumes its transcript tailer, which reads the end already on
    /// disk instead of leaving the session busy for good.
    #[tokio::test]
    #[serial_test::serial]
    async fn attach_resumes_tailing_a_background_task_that_ended_during_the_restart() {
        let session_id = "s-attach-task";
        let mut fixture =
            attach_with_orphaned_background_agent(session_id, "", Some("task-1")).await;
        assert_eq!(
            fixture.store.unresolved_background_task_ids(session_id),
            ["task-1".to_string()],
            "precondition: the task is open before the tailer runs"
        );
        // The fake runner's native session is `acp-sid`; its transcript holds
        // the end of the task.
        let home = fixture._tmp.path().join("captured-claude");
        let dir = home.join("projects/-enc");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("acp-sid.jsonl"),
            r#"{"type":"queue-operation","operation":"enqueue","content":"<task-notification>\n<task-id>task-1</task-id>\n<tool-use-id>toolu_task</tool-use-id>\n<status>completed</status>\n</task-notification>"}"#
                .to_string()
                + "\n",
        )
        .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !fixture
                .store
                .unresolved_background_task_ids(session_id)
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
        .expect("the resumed tailer must finish the task");
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
            // No stored session: an allowed attach fails fast past the gate
            // instead of dialing a control socket until the runner deadline.
            None,
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

    #[tokio::test]
    #[serial_test::serial]
    async fn borrowed_attach_timeout_and_drop_preserve_the_original_turn_for_retry() {
        for drop_waiter in [false, true] {
            let (_home, tmp) = isolate_home();
            let id = "borrowed-attach-retry";
            let mut runner = HeldAttachRunner::new(id, tmp.path(), 17, None);
            let bytes = std::fs::read(worker_registry::record_path(id).unwrap()).unwrap();
            let (sink, store, mut rx, _store_tmp) = channel_sink();
            sink.publish(
                id,
                1,
                &Event::UserPromptSent {
                    text: "continue the original turn".into(),
                    attachments: vec![],
                    prompt_id: None,
                    synthesized: false,
                    origin_plugin: None,
                },
            );
            let control = Arc::new(ObservedProcessControl::default());
            let sup = Arc::new(Supervisor::new(sink).with_process_control(control.clone()));
            sup.hydrate_seqs(store.all_session_seqs());
            let attaching = tokio::spawn({
                let sup = sup.clone();
                let cwd = tmp.path().to_path_buf();
                async move { sup.attach(id.into(), cwd, vec![], true, None).await }
            });
            runner.entered().await;
            if drop_waiter {
                attaching.abort();
                assert!(attaching.await.unwrap_err().is_cancelled());
            } else {
                tokio::time::pause();
                tokio::time::advance(std::time::Duration::from_secs(3)).await;
                tokio::time::resume();
                assert!(matches!(
                    attaching.await.unwrap(),
                    Err(SupervisorError::Acp(AcpError::AttachTimedOut))
                ));
            }
            runner.disconnected().await;
            assert!(control.signals().is_empty(), "drop_waiter={drop_waiter}");
            assert!(control.scopes().is_empty(), "drop_waiter={drop_waiter}");
            assert_eq!(
                std::fs::read(worker_registry::record_path(id).unwrap()).unwrap(),
                bytes,
                "the original registry record must survive unchanged"
            );
            assert!(worker_registry::is_record_live(&runner.record));
            assert!(store.has_in_flight_turn(id));
            assert!(
                !sup.is_owned(id).await,
                "observation releases its reservation"
            );
            assert!(!store.replay_from(id, 0).iter().any(|(_, event)| matches!(
                event,
                Event::Stopped { .. } | Event::SessionContextReset { .. }
            )));

            sup.attach(id.into(), tmp.path().into(), vec![], true, None)
                .await
                .unwrap();
            assert_eq!(sup.running_identity(id), Some(runner.identity));
            assert!(control.signals().is_empty());
            runner.complete_turn();
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while store.has_in_flight_turn(id) {
                    rx.recv().await.unwrap();
                }
            })
            .await
            .unwrap();
            let events = store.replay_from(id, 0);
            assert!(events.iter().any(|(_, event)| matches!(
                event,
                Event::AcpSessionAssigned { acp_session_id }
                    if Some(acp_session_id) == runner.record.stored_acp_session_id.as_ref()
            )));
            assert!(events.iter().any(|(_, event)| matches!(
                event,
                Event::AgentMessageChunk { text, .. } if text == "original producer completed"
            )));
            let stops: Vec<_> = events
                .iter()
                .filter_map(|(_, event)| match event {
                    Event::Stopped { reason } => Some(reason.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(stops, ["prompt_complete"]);
            assert_eq!(
                crate::process::process_birth_identity(runner.identity.pid)
                    .unwrap()
                    .unwrap()
                    .fingerprint(),
                runner.identity.birth_fingerprint.unwrap()
            );
            sup.detach_all().await;
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn captured_stop_retires_original_across_attach_deadline_and_drop_not_replacement() {
        use crate::acp::runner_lifecycle::ProcessControl as _;

        for stop_before_deadline in [true, false] {
            let (_home, tmp) = isolate_home();
            let id = "borrowed-attach-stop-race";
            let mut original = HeldAttachRunner::new(id, tmp.path(), 17, None);
            let control = Arc::new(ObservedProcessControl::default());
            let sup =
                Arc::new(Supervisor::new(VecSink::new()).with_process_control(control.clone()));
            let attaching = tokio::spawn({
                let sup = sup.clone();
                let cwd = tmp.path().to_path_buf();
                async move { sup.attach(id.into(), cwd, vec![], true, None).await }
            });
            original.entered().await;
            let target = sup.capture_retirement(id).unwrap();
            let replacement = HeldAttachRunner::new(id, tmp.path(), 18, None);
            let replacement_bytes =
                std::fs::read(worker_registry::record_path(id).unwrap()).unwrap();
            let mut replacement_observer = None;
            if stop_before_deadline {
                // This claims the frozen original before waking the handshake.
                sup.shutdown(id).await.unwrap();
                tokio::time::pause();
                tokio::time::advance(std::time::Duration::from_secs(3)).await;
                tokio::time::resume();
                assert!(attaching.await.unwrap().is_err());
            } else {
                attaching.abort();
                assert!(attaching.await.unwrap_err().is_cancelled());
                // A stale automatic tick reserved its own observation after timeout.
                // Its replacement authority must never become the captured stop target.
                replacement_observer = Some(reserve(
                    sup.begin_resume(
                        id,
                        ResumeKind::Attach,
                        worker_registry::ResumeAuthority::Automatic,
                    )
                    .await,
                ));
            }
            original.disconnected().await;
            sup.shutdown_captured_and_wait(id, &target, std::time::Duration::from_secs(5))
                .await
                .unwrap();
            drop(replacement_observer);
            assert!(!control.is_alive(original.identity));
            assert!(control.is_alive(replacement.identity));
            assert!(control
                .signals()
                .iter()
                .any(|(identity, _)| *identity == original.identity));
            assert!(control
                .signals()
                .iter()
                .all(|(identity, _)| *identity == original.identity));
            assert!(control.scopes().contains(&original.record.scope));
            assert!(!control.scopes().contains(&replacement.record.scope));
            assert_eq!(
                std::fs::read(worker_registry::record_path(id).unwrap()).unwrap(),
                replacement_bytes
            );
            assert!(worker_registry::is_record_live(&replacement.record));
        }
    }
}
