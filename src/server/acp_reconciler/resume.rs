//! Resuming a single session's worker: attach to a live runner or fresh-spawn one.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::time::timeout_at;

use super::{is_resumable, query_flushed, AppState, ResumeOutcome};
use crate::acp::supervisor::{
    command_override_for_spawn, BroadcastSink, ResumeKind, ResumeReservation, SpawnRequest,
    Supervisor, SupervisorError,
};
use crate::process::worker_registry;
use crate::server::sandbox_login::LaunchOrigin;
use crate::server::session_service::SessionService;
use crate::session::Instance;

/// A structured view session that needs a worker, snapshotted at the tick so
/// resume tasks need not re-scan the instance list. Anything that a later pick
/// can change is re-read in `build_spawn_request`, not carried here.
#[derive(Clone)]
pub(super) struct ResumeTarget {
    pub(super) id: String,
    pub(super) lifecycle_generation: u64,
    pub(super) created_at: crate::time::Timestamp,
    pub(super) native_scope: Option<crate::acp::state::NativeObservationScope>,
    tool: String,
    agent_override: Option<String>,
    pub(super) project_path: String,
    stored_acp_session_id: Option<String>,
    pub(super) source_profile: String,
    pub(super) in_flight_turn: bool,
    pub(super) historical_control_known: bool,
    /// Agent-started work must not adopt the runner's cached user completion.
    pub(super) agent_turn_open: bool,
    /// Actual work or a recent background finish awaiting the agent's answer.
    pub(super) holds_live_work: bool,
    yolo_mode: bool,
    /// The resolved launch command, honored like tmux does (#1766).
    command: String,
}

impl ResumeTarget {
    pub(super) fn from_instance(inst: &Instance) -> Self {
        Self {
            id: inst.id.clone(),
            lifecycle_generation: inst.lifecycle_generation,
            created_at: inst.created_at,
            native_scope: None,
            tool: inst.tool.clone(),
            agent_override: inst.agent_name.clone(),
            project_path: inst.project_path.clone(),
            stored_acp_session_id: inst.acp_session_id.clone(),
            source_profile: inst.source_profile.clone(),
            in_flight_turn: false,
            historical_control_known: true,
            agent_turn_open: false,
            holds_live_work: false,
            yolo_mode: inst.yolo_mode,
            command: inst.command.clone(),
        }
    }

    fn spawn_kind(&self) -> ResumeKind {
        spawn_kind(self.stored_acp_session_id.is_some())
    }

    fn unavailable(
        &self,
        refusal: worker_registry::ControlAdmissionRefusal,
        origin: Option<&crate::acp::runner_lifecycle::Lease>,
    ) -> ResumeOutcome {
        ResumeOutcome::Unavailable {
            scope: match origin {
                Some(lease) => lease.native_scope().ok().flatten(),
                None => self.native_scope.clone(),
            },
            refusal,
            originating_epoch: origin.map(|lease| lease.epoch()),
        }
    }

    pub(super) async fn freeze_native_scope(
        &mut self,
        previous: Option<crate::acp::state::NativeObservationScope>,
    ) -> anyhow::Result<()> {
        let (profile, id, created_at, generation) = (
            self.source_profile.clone(),
            self.id.clone(),
            self.created_at,
            self.lifecycle_generation,
        );
        let scope = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let storage = crate::session::Storage::open_reference(&profile)?;
            let context = crate::acp::state::NativeEventContext::capture_identity(
                &id, created_at, generation, &storage,
            )?;
            let scope = if let Some(scope) = previous.filter(|scope| scope.context == context) {
                scope
            } else {
                crate::acp::state::NativeObservationScope {
                    context,
                    choice: None,
                    fence: crate::session::NativeInputFence::capture_route(
                        storage.profile(),
                        &id,
                        created_at,
                        generation,
                        false,
                        Some(storage.bound_profile_identity()?),
                    )?,
                }
            };
            let _guard = scope.fence.lock_observation()?;
            drop(_guard);
            Ok(scope)
        })
        .await??;
        self.native_scope = Some(scope);
        Ok(())
    }
}

fn spawn_kind(has_stored_id: bool) -> ResumeKind {
    if has_stored_id {
        ResumeKind::Spawn
    } else {
        ResumeKind::SpawnFirst
    }
}

/// What to do with the registry record of a session with no in-memory worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdoptDecision {
    /// No usable record: sweep and fresh-spawn.
    FreshSpawn,
    Attach,
    RespawnStaleIdle,
    /// Build-stale worker mid-turn: adopt until the turn drains (#1754).
    AdoptStaleForDrain,
    ReplaceIncompatibleRunner,
}

/// A build-stale runner still speaks the protocol, so its turn may drain; a
/// runner-generation mismatch cannot attach at all and is replaced at once.
fn adopt_decision(
    live: bool,
    build_current: bool,
    runner_current: bool,
    holds_live_work: bool,
) -> AdoptDecision {
    match (live, runner_current, build_current, holds_live_work) {
        (false, ..) => AdoptDecision::FreshSpawn,
        (true, false, ..) => AdoptDecision::ReplaceIncompatibleRunner,
        (true, true, true, _) => AdoptDecision::Attach,
        (true, true, false, true) => AdoptDecision::AdoptStaleForDrain,
        (true, true, false, false) => AdoptDecision::RespawnStaleIdle,
    }
}

/// Publishes the terminal of a turn orphaned by a replaced runner.
fn publish_orphaned_turn_stop<S: BroadcastSink>(
    supervisor: &Supervisor<S>,
    session_id: &str,
    decision: AdoptDecision,
    in_flight_turn: bool,
) {
    if !in_flight_turn {
        return;
    }
    let reason = if decision == AdoptDecision::ReplaceIncompatibleRunner {
        "runner_protocol_upgraded"
    } else {
        "orphaned_at_restart"
    };
    supervisor.synthesize_stopped_for_orphan(session_id, reason);
}

/// A `Monitor` watch or background command lives inside the agent process, so
/// a replacement process has lost it; the reminder gives the agent a turn to
/// re-arm it.
const MONITOR_RESTART_REMINDER: &str = "Note: this session's agent process restarted, so any background Monitor watch or background command that was running is no longer running. Re-arm or re-run it if it's still needed.";

/// Queues the re-arm reminder once a fresh process replaces the session's worker.
pub(super) async fn requeue_interrupted_monitor(state: &AppState, session_id: &str) {
    if state
        .acp_event_store
        .latest_active_monitor(session_id)
        .is_none()
    {
        return;
    }
    state
        .session_service
        .set_pending_initial_turn(
            session_id,
            MONITOR_RESTART_REMINDER.to_string(),
            vec![],
            crate::daemon::PromptOrigin::Aoe,
        )
        .await;
    tracing::debug!(target: "acp.supervisor", session = %session_id, "queued monitor re-arm reminder");
}

async fn admit(
    state: &AppState,
    id: &str,
    kind: ResumeKind,
    predecessor: Option<crate::acp::runner_lifecycle::RunnerIdentity>,
) -> Result<Option<crate::acp::supervisor::ResumeReservation>, SupervisorError> {
    let outcome = match predecessor {
        Some(predecessor) => {
            state
                .acp_supervisor
                .begin_governed_replacement(id, kind, predecessor)
                .await?
        }
        None => {
            state
                .acp_supervisor
                .begin_resume(
                    id,
                    kind,
                    crate::process::worker_registry::ResumeAuthority::Automatic,
                )
                .await?
        }
    };
    Ok(outcome)
}

/// Record why admission did not produce a spawn, and whether reconciliation
/// should re-arm the session and keep or refund the attempt's budget charge.
fn refused_admission(id: &str, what: &str, error: SupervisorError) -> ResumeOutcome {
    match &error {
        SupervisorError::SpawnCancelled(_)
        | SupervisorError::PurgeInProgress(_)
        | SupervisorError::TeardownPending(_) => {
            tracing::debug!(target: "acp.supervisor", session = %id, "{what} not admitted: {error}");
        }
        _ => {
            tracing::warn!(target: "acp.supervisor", session = %id, "{what} not admitted: {error}");
        }
    }
    match error {
        SupervisorError::PurgeInProgress(_) => ResumeOutcome::AdmissionDeferred,
        SupervisorError::TeardownPending(_) => ResumeOutcome::Retry,
        _ => ResumeOutcome::SpawnFinished,
    }
}

pub(super) async fn resume_one(state: Arc<AppState>, mut target: ResumeTarget) -> ResumeOutcome {
    let id = target.id.clone();
    let in_flight_turn = target.in_flight_turn;
    if target.native_scope.is_none() && target.freeze_native_scope(None).await.is_err() {
        return ResumeOutcome::Retry;
    }

    // Take the lease before any preparation so a stop landing from here on is honored.
    let probe_id = id.clone();
    let record = match tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let registry =
            worker_registry::try_registry_lock(&probe_id)?.ok_or(worker_registry::RegistryBusy)?;
        Ok(registry.control_admission())
    })
    .await
    {
        Ok(Ok(Ok(record))) => record,
        Ok(Ok(Err(refusal))) => {
            tracing::warn!(target: "acp.supervisor", session = %id, %refusal,
                "resume: retained runtime admission refused");
            return target.unavailable(refusal, None);
        }
        Ok(Err(error)) if error.is::<worker_registry::RegistryBusy>() => {
            return ResumeOutcome::RegistryBusy
        }
        Ok(Err(_)) => {
            return target.unavailable(
                worker_registry::ControlAdmissionRefusal::unconfirmed(None),
                None,
            )
        }
        Err(error) => {
            tracing::warn!(target: "acp.supervisor", session = %id, %error,
                "resume: retained runtime admission probe failed");
            return ResumeOutcome::Retry;
        }
    };
    let mut decision = record.as_ref().map_or(AdoptDecision::FreshSpawn, |r| {
        adopt_decision(
            worker_registry::is_record_live(r),
            worker_registry::is_build_current(r),
            worker_registry::is_runner_current(r),
            target.holds_live_work,
        )
    });
    if !target.historical_control_known {
        decision = match decision {
            AdoptDecision::Attach | AdoptDecision::AdoptStaleForDrain => decision,
            AdoptDecision::RespawnStaleIdle => AdoptDecision::AdoptStaleForDrain,
            _ => return ResumeOutcome::Retry,
        };
    }
    let kind = match decision {
        AdoptDecision::Attach | AdoptDecision::AdoptStaleForDrain => ResumeKind::Attach,
        _ => target.spawn_kind(),
    };
    let predecessor = record
        .as_ref()
        .filter(|_| {
            matches!(
                decision,
                AdoptDecision::ReplaceIncompatibleRunner | AdoptDecision::RespawnStaleIdle,
            )
        })
        .map(crate::acp::runner_lifecycle::RunnerIdentity::from_record);
    let mut reservation = match admit(&state, &id, kind, predecessor).await {
        Ok(Some(reservation)) => reservation,
        Ok(None) => return ResumeOutcome::SpawnFinished,
        Err(error) if error.is_registry_busy() => return ResumeOutcome::RegistryBusy,
        Err(SupervisorError::Acp(
            crate::acp::acp_client::AcpError::RetainedRuntimeUnavailable(refusal),
        )) => return target.unavailable(*refusal, None),
        Err(error @ SupervisorError::CapacityFull { .. }) => {
            return ResumeOutcome::CapacityDeferred {
                message: error.to_string(),
                source_profile: target.source_profile,
                lifecycle_generation: target.lifecycle_generation,
            }
        }
        Err(error) => return refused_admission(&id, "resume", error),
    };
    reservation.expected_lifecycle_generation = Some(target.lifecycle_generation);
    let mut originating_lease = reservation.lease().clone();
    // The snapshot may predate an archive, snooze, trash or stop.
    if resume_target_for_session(&state.session_service, &id)
        .await
        .filter(|fresh| {
            fresh.lifecycle_generation == target.lifecycle_generation
                && fresh.created_at == target.created_at
                && fresh.source_profile == target.source_profile
        })
        .is_none()
    {
        tracing::debug!(target: "acp.supervisor", session = %id, "session left the resume set after the snapshot; not resuming");
        return ResumeOutcome::Retry;
    }

    if let Some(record) = record {
        match decision {
            AdoptDecision::Attach | AdoptDecision::AdoptStaleForDrain => {
                if decision == AdoptDecision::AdoptStaleForDrain {
                    tracing::info!(
                        target: "acp.supervisor",
                        session = %id,
                        old_build = %record.build_version,
                        new_build = crate::build_info::BUILD_VERSION,
                        "adopting build-stale structured view worker to drain in-flight turn before respawn"
                    );
                }
                let attach_deadline = reservation.admission_deadline();
                let attach = attach_retained_target(&state.session_service, &target, reservation);
                match timeout_at(attach_deadline, attach).await {
                    Ok(Ok(())) => {
                        // Flagged only once attached: a failed attach respawns on the current binary.
                        if decision == AdoptDecision::AdoptStaleForDrain {
                            state.acp_supervisor.mark_build_respawn_pending(&id);
                        }
                        tracing::info!(
                            target: "acp.supervisor",
                            session = %id,
                            pid = record.pid,
                            in_flight_turn,
                            "reattached to existing structured view runner"
                        );
                        if in_flight_turn {
                            seed_in_flight_status(&state, &id).await;
                        }
                        return ResumeOutcome::Attached;
                    }
                    Ok(Err(error)) if error.is_registry_busy() => {
                        return ResumeOutcome::RegistryBusy
                    }
                    Ok(Err(SupervisorError::SpawnCancelled(_))) => {
                        return ResumeOutcome::SpawnFinished
                    }
                    Ok(Err(SupervisorError::Acp(
                        crate::acp::acp_client::AcpError::RetainedRuntimeUnavailable(refusal),
                    ))) => return target.unavailable(*refusal, Some(&originating_lease)),
                    Ok(Err(SupervisorError::Acp(
                        crate::acp::acp_client::AcpError::AttachTimedOut,
                    ))) => {
                        // An incomplete observation is not evidence against
                        // this already attested retained owner. Release only
                        // the borrowed attachment and let the existing tick retry.
                        return ResumeOutcome::Retry;
                    }
                    Ok(Err(e)) => {
                        if !target.historical_control_known {
                            return ResumeOutcome::Retry;
                        }
                        if worker_registry::retained_process_may_be_alive(&record) {
                            return target.unavailable(
                                worker_registry::ControlAdmissionRefusal::from_record(
                                    &record,
                                    worker_registry::ControlRefusalReason::OwnershipUnconfirmed,
                                ),
                                Some(&originating_lease),
                            );
                        }
                        tracing::warn!(target: "acp.supervisor", session = %id,
                            "dead runner attachment failed; following existing retirement policy: {e}");
                        if let Err(error) =
                            worker_registry::terminate_record_and_wait(record, ()).await
                        {
                            return if error.is::<worker_registry::RegistryBusy>() {
                                ResumeOutcome::RegistryBusy
                            } else {
                                ResumeOutcome::Retry
                            };
                        }
                    }
                    Err(_) => {
                        tracing::warn!(target: "acp.supervisor", session = %id,
                            "attach timed out; preserving the original runner owner");
                        return ResumeOutcome::Retry;
                    }
                }
                // The failed attach released its lease; the spawn needs one that counts toward capacity.
                reservation = match admit(&state, &id, target.spawn_kind(), None).await {
                    Ok(Some(reservation)) => reservation,
                    Ok(None) => return ResumeOutcome::SpawnFinished,
                    Err(error) if error.is_registry_busy() => return ResumeOutcome::RegistryBusy,
                    Err(SupervisorError::Acp(
                        crate::acp::acp_client::AcpError::RetainedRuntimeUnavailable(refusal),
                    )) => return target.unavailable(*refusal, None),
                    Err(error @ SupervisorError::CapacityFull { .. }) => {
                        return ResumeOutcome::CapacityDeferred {
                            message: error.to_string(),
                            source_profile: target.source_profile,
                            lifecycle_generation: target.lifecycle_generation,
                        }
                    }
                    Err(error) => return refused_admission(&id, "fallback spawn", error),
                };
                reservation.expected_lifecycle_generation = Some(target.lifecycle_generation);
                originating_lease = reservation.lease().clone();
            }
            AdoptDecision::ReplaceIncompatibleRunner => {
                tracing::info!(
                    target: "acp.supervisor",
                    session = %id,
                    old_runner_version = record.runner_version,
                    new_runner_version = worker_registry::RUNNER_VERSION,
                    "replacing incompatible structured view runner"
                );
                if let Err(error) = worker_registry::terminate_record_and_wait(record, ()).await {
                    return if error.is::<worker_registry::RegistryBusy>() {
                        ResumeOutcome::RegistryBusy
                    } else {
                        ResumeOutcome::Retry
                    };
                }
            }
            AdoptDecision::RespawnStaleIdle => {
                tracing::info!(
                    target: "acp.supervisor",
                    session = %id,
                    old_build = %record.build_version,
                    new_build = crate::build_info::BUILD_VERSION,
                    "respawning idle build-stale structured view worker on current binary"
                );
                if let Err(error) = worker_registry::terminate_record_and_wait(record, ()).await {
                    return if error.is::<worker_registry::RegistryBusy>() {
                        ResumeOutcome::RegistryBusy
                    } else {
                        ResumeOutcome::Retry
                    };
                }
            }
            // A dead record can still hold a live pid whose socket vanished.
            AdoptDecision::FreshSpawn => {
                if let Err(error) = worker_registry::terminate_record_and_wait(record, ()).await {
                    return if error.is::<worker_registry::RegistryBusy>() {
                        ResumeOutcome::RegistryBusy
                    } else {
                        ResumeOutcome::Retry
                    };
                }
            }
        }
    }

    publish_orphaned_turn_stop(
        &state.acp_supervisor,
        &id,
        decision,
        in_flight_turn || target.agent_turn_open,
    );
    requeue_interrupted_monitor(&state, &id).await;
    let req = match build_spawn_request(&state.session_service, &target, LaunchOrigin::Unattended)
        .await
    {
        Ok(req) => req,
        Err(PrepFailure::Reported) => return ResumeOutcome::SpawnFinished,
        Err(PrepFailure::Raced) => return ResumeOutcome::Retry,
    };
    let agent = req.agent.clone();
    match state.acp_supervisor.spawn_inner(req, reservation).await {
        Err(error) if error.is_registry_busy() => return ResumeOutcome::RegistryBusy,
        Err(SupervisorError::Acp(
            crate::acp::acp_client::AcpError::RetainedRuntimeUnavailable(refusal),
        )) => return target.unavailable(*refusal, Some(&originating_lease)),
        Err(e @ SupervisorError::CapacityFull { .. }) => {
            return ResumeOutcome::CapacityDeferred {
                message: e.to_string(),
                source_profile: target.source_profile,
                lifecycle_generation: target.lifecycle_generation,
            }
        }
        Err(SupervisorError::TeardownPending(_)) => return ResumeOutcome::Retry,
        // The supervisor already parked the session; a startup error would bury it.
        Err(SupervisorError::Acp(crate::acp::acp_client::AcpError::RateLimited(_))) | Ok(()) => {}
        Err(e) => report_spawn_failure(&state.session_service, &target, &agent, &e).await,
    }
    ResumeOutcome::SpawnFinished
}

async fn seed_in_flight_status(state: &AppState, id: &str) {
    let Some(projected) =
        crate::server::acp_events::folded_sidebar_status(&state.session_service, id).await
    else {
        return;
    };
    let mut instances = state.instances.write().await;
    if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
        crate::server::acp_events::apply_status_seed(inst, projected, &state.status_tx);
    }
}

/// Publishes a startup error for a real spawn failure of a still-present session.
async fn report_spawn_failure(
    service: &SessionService,
    target: &ResumeTarget,
    agent: &str,
    error: &SupervisorError,
) {
    if matches!(
        error,
        SupervisorError::AlreadyRunning(_)
            | SupervisorError::SpawnCancelled(_)
            | SupervisorError::RegistryBusy
            | SupervisorError::Acp(crate::acp::acp_client::AcpError::RegistryBusy)
    ) {
        return;
    }
    let message = format!("Failed to start structured view agent {agent:?}: {error}");
    tracing::warn!(target: "acp.supervisor", session = %target.id, agent = %agent,
        "structured view spawn failed: {message}");
    let Ok(state) = service.native_state() else {
        return;
    };
    crate::server::api::publish_startup_error_if_current(
        &state,
        &target.id,
        &target.source_profile,
        target.lifecycle_generation,
        message,
    )
    .await;
}

/// Why `build_spawn_request` produced no request.
#[derive(Debug)]
enum PrepFailure {
    /// A startup error was published; retrying would repeat it.
    Reported,
    /// The row changed under the tick; the next tick re-evaluates it.
    Raced,
}

/// Builds a fresh-spawn request; on sandbox failure publishes a startup error and returns `Err`.
/// A launch refused for the sandbox login also holds the session under `origin`.
async fn build_spawn_request(
    service: &Arc<SessionService>,
    target: &ResumeTarget,
    origin: LaunchOrigin,
) -> Result<SpawnRequest, PrepFailure> {
    let state = service.native_state().map_err(|_| PrepFailure::Raced)?;
    let exclusion = crate::acp::sandbox::LaunchExclusion::acquire(state.clone(), &target.id, false)
        .await
        .map_err(|_| PrepFailure::Raced)?;
    let supervisor = &service.acp_supervisor;

    // Re-read under the session lock, for two reasons. A worktree rename holds
    // it across the move, so a snapshotted path could be stale (#2260); and the
    // persisted selectors can be re-picked after the tick snapshotted
    // this target, which the handshake then re-asserts at the agent. Released
    // before ensure_container, which takes the same lock.
    let (
        cwd,
        seed_history_replay,
        fork_from,
        acp_mode_id,
        acp_effort,
        acp_model_config,
        agent_model,
        agent_provider,
        claude_store_pin,
        instance,
        prior_conversation,
        workspace_info,
    ) = {
        let instances = service.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == target.id) else {
            return Err(PrepFailure::Raced);
        };
        if inst.terminal_handoff.is_some()
            || !inst.launch_is_finalized()
            || inst.lifecycle_generation != target.lifecycle_generation
            || inst.source_profile != target.source_profile
        {
            return Err(PrepFailure::Raced);
        }
        (
            PathBuf::from(&inst.project_path),
            inst.import_pending == Some(true),
            inst.fork_pending.clone(),
            inst.acp_mode_id.clone(),
            inst.acp_effort.clone(),
            inst.acp_model_config.clone(),
            inst.agent_model.clone(),
            inst.agent_provider.clone(),
            inst.selected_claude_store_pin(),
            inst.clone(),
            target.stored_acp_session_id.is_none()
                && (inst.agent_session_id.is_some() || inst.pi_session_path.is_some()),
            inst.workspace_info.clone(),
        )
    };
    let config_path = instance.repo_config_path().to_path_buf();
    let agent = supervisor
        .pick_agent_for_tool(
            &target.tool,
            target.agent_override.as_deref(),
            &target.source_profile,
            &config_path,
        )
        .await;
    let sandbox_continuation = if instance.is_sandboxed()
        && seed_history_replay
        && target.stored_acp_session_id.as_deref().is_some_and(|id| {
            instance.agent_session_id.as_deref() == Some(id)
                && crate::agents::acp_transcript_cli_resumable(&target.tool, &agent)
        }) {
        crate::acp::supervisor::SandboxContinuation::ImportTerminal
    } else {
        crate::acp::supervisor::SandboxContinuation::Persisted
    };
    let (native, exclusion) = crate::server::session_store::NativeSessionStore::open_for_launch(
        state.clone(),
        &instance,
        exclusion,
    )
    .await
    .map_err(|_| PrepFailure::Raced)?;
    let (sandbox_info, _exclusion) = match crate::acp::sandbox::ensure_container_for_session(
        native.clone(),
        instance,
        exclusion,
    )
    .await
    {
        Ok(info) => info,
        Err(e) => {
            if let Some(blocked) = e.downcast_ref::<crate::session::SandboxLoginRequired>() {
                tracing::warn!(target: "acp.supervisor", session = %target.id, "resume launch held: {blocked}");
                service.sandbox_login.hold(&target.id, origin, blocked);
                supervisor.publish_login_required(&target.id, blocked);
                return Err(PrepFailure::Reported);
            }
            let message = format!("sandbox container ensure failed: {e}");
            tracing::warn!(target: "acp.supervisor", session = %target.id, "reconciler container ensure failed: {message}");
            let published = crate::server::api::publish_startup_error_with_cause_if_current(
                &state,
                &target.id,
                &target.source_profile,
                target.lifecycle_generation,
                message,
                crate::session::SandboxLoginUnsupported::cause_of(&e),
            )
            .await;
            return Err(if published {
                PrepFailure::Reported
            } else {
                PrepFailure::Raced
            });
        }
    };

    Ok(SpawnRequest {
        launch_admission: Some(crate::acp::supervisor::LaunchAdmission {
            establishment: None,
            store: native,
            generation: target.lifecycle_generation,
            namespace: Some(state.profile_namespace.clone()),
            native_scope: None,
        }),
        expected_lifecycle_generation: target.lifecycle_generation,
        session_id: target.id.clone(),
        agent,
        tool: target.tool.clone(),
        cwd,
        config_path,
        additional_dirs: vec![],
        provider_env: vec![],
        provider: agent_provider,
        model: agent_model,
        // `acp_effort` only holds a user-set effort, so presence is its provenance.
        effort_explicit: acp_effort.is_some(),
        effort: acp_effort,
        model_config: acp_model_config,
        stored_acp_session_id: target.stored_acp_session_id.clone(),
        fork_from,
        sandbox_continuation,
        sandbox_info,
        workspace_info,
        source_profile: Some(target.source_profile.clone()),
        yolo_mode: target.yolo_mode,
        acp_mode_id,
        agent_command_override: command_override_for_spawn(&target.tool, &target.command),
        seed_history_replay,
        prior_conversation,
        claude_store_pin,
    })
}

async fn attach_retained_target(
    service: &Arc<SessionService>,
    target: &ResumeTarget,
    reservation: ResumeReservation,
) -> Result<(), SupervisorError> {
    let sandbox = {
        let instances = service.instances.read().await;
        let row = instances
            .iter()
            .find(|row| {
                row.id == target.id
                    && is_resumable(row)
                    && row.source_profile == target.source_profile
                    && row.created_at == target.created_at
                    && row.lifecycle_generation == target.lifecycle_generation
            })
            .ok_or_else(|| SupervisorError::SpawnCancelled(target.id.clone()))?;
        let config_path = row.repo_config_path().to_path_buf();
        row.sandbox_info.clone().map(|info| (info, config_path))
    };
    // Attached workers resume their background tailers independently in
    // install_worker. Only an actual main turn owns a cached prompt completion.
    let in_flight_turn = super::query_control(
        &service.acp_event_store,
        &target.id,
        "retained main-turn adoption",
        |store, id| store.has_in_flight_main_turn(id),
    )
    .await?;
    let in_flight_turn = matches!(
        in_flight_turn,
        crate::acp::event_store::ControlKnowledge::Known(true)
    );
    service
        .acp_supervisor
        .attach_inner(
            target.id.clone(),
            PathBuf::from(&target.project_path),
            vec![],
            in_flight_turn,
            sandbox,
            reservation,
        )
        .await
}

pub(crate) async fn ensure_rate_limit_worker(
    state: &Arc<AppState>,
    id: &str,
    authority: worker_registry::ResumeAuthority,
    retained_only: bool,
) -> Result<(), SupervisorError> {
    if !state.acp_supervisor.is_running(id).await {
        let target = resume_target_for_session(&state.session_service, id)
            .await
            .ok_or_else(|| SupervisorError::SpawnCancelled(id.to_owned()))?;
        let probe_id = id.to_owned();
        let record =
            tokio::task::spawn_blocking(move || worker_registry::control_admission(&probe_id))
                .await
                .map_err(|_| SupervisorError::NativeStoreUnavailable)?
                .map_err(|refusal| {
                    SupervisorError::Acp(
                        crate::acp::acp_client::AcpError::RetainedRuntimeUnavailable(Box::new(
                            refusal,
                        )),
                    )
                })?;
        if record
            .as_ref()
            .is_some_and(worker_registry::retained_process_may_be_alive)
        {
            if let Some(mut reservation) = state
                .acp_supervisor
                .begin_resume(id, ResumeKind::Attach, authority)
                .await?
            {
                reservation.expected_lifecycle_generation = Some(target.lifecycle_generation);
                let deadline = reservation.admission_deadline();
                timeout_at(
                    deadline,
                    attach_retained_target(&state.session_service, &target, reservation),
                )
                .await
                .map_err(|_| {
                    SupervisorError::Acp(crate::acp::acp_client::AcpError::AttachTimedOut)
                })??;
            }
        } else {
            if retained_only {
                return Err(SupervisorError::ControlStateUnavailable);
            }
            let origin = if authority == worker_registry::ResumeAuthority::Explicit {
                crate::server::sandbox_login::LaunchOrigin::Interactive
            } else {
                crate::server::sandbox_login::LaunchOrigin::Unattended
            };
            match trigger_resume_background(&state.session_service, id, authority, origin, None)
                .await?
            {
                ResumeTrigger::NotFound => {
                    return Err(SupervisorError::UnknownSession(id.to_owned()))
                }
                ResumeTrigger::Started | ResumeTrigger::AlreadyResuming => {}
            }
        }
    }
    state.acp_supervisor.wait_until_ready(id).await
}

async fn resume_target_for_session(service: &SessionService, id: &str) -> Option<ResumeTarget> {
    let instances = service.instances.read().await;
    instances
        .iter()
        .find(|i| i.id == id && is_resumable(i))
        .map(ResumeTarget::from_instance)
}

pub(crate) enum ResumeTrigger {
    /// A detached resume holds the lease, so `wait_for_worker` blocks until live.
    Started,
    AlreadyResuming,
    NotFound,
}

/// Reserves retained attachment or a new launch for `id` in a detached task
/// that survives the originating request (#1748).
///
/// Nothing may hold `instance_lock` while awaiting worker readiness: the
/// detached spawn takes it in `build_spawn_request` (#3172, #3621).
pub(crate) async fn trigger_resume_background(
    service: &Arc<SessionService>,
    id: &str,
    authority: crate::process::worker_registry::ResumeAuthority,
    origin: LaunchOrigin,
    reservation: Option<crate::acp::supervisor::ResumeReservation>,
) -> Result<ResumeTrigger, SupervisorError> {
    trigger_resume_observing(service, id, authority, origin, reservation, None, None).await
}

/// [`trigger_resume_background`] admitting against a stop intent the caller already
/// observed (`Some`), so a newer stop is refused instead of cleared.
pub(crate) async fn trigger_resume_observing(
    service: &Arc<SessionService>,
    id: &str,
    authority: crate::process::worker_registry::ResumeAuthority,
    origin: LaunchOrigin,
    reservation: Option<crate::acp::supervisor::ResumeReservation>,
    observed_stop: Option<Option<crate::process::worker_registry::StopIntent>>,
    stop_refused: Option<tokio::sync::oneshot::Sender<SupervisorError>>,
) -> Result<ResumeTrigger, SupervisorError> {
    if service
        .instances
        .read()
        .await
        .iter()
        .any(|row| row.id == id && row.terminal_handoff.is_some())
    {
        return Ok(ResumeTrigger::NotFound);
    }
    // Unknown turn state defers the wake: replacing the worker without closing an
    // orphaned turn would leave the session Running.
    let Some(Ok(crate::acp::event_store::ControlKnowledge::Known(orphaned_turn))) = query_flushed(
        &service.acp_supervisor,
        &service.acp_event_store,
        id,
        "open turn",
        |s, id| s.has_open_turn(id),
    )
    .await
    else {
        return Err(SupervisorError::EventStoreUnavailable(id.to_string()));
    };
    let has_stored_id = service
        .instances
        .read()
        .await
        .iter()
        .any(|i| i.id == id && i.acp_session_id.is_some());
    let mut reservation = match reservation {
        Some(reservation) => {
            service
                .acp_supervisor
                .validate_reserved_resume(&reservation)?;
            reservation
        }
        None => {
            let probe_id = id.to_owned();
            let record =
                tokio::task::spawn_blocking(move || worker_registry::control_admission(&probe_id))
                    .await
                    .map_err(|_| SupervisorError::NativeStoreUnavailable)?
                    .map_err(|refusal| {
                        SupervisorError::Acp(
                            crate::acp::acp_client::AcpError::RetainedRuntimeUnavailable(Box::new(
                                refusal,
                            )),
                        )
                    })?;
            let kind = if record
                .as_ref()
                .is_some_and(worker_registry::retained_process_may_be_alive)
            {
                ResumeKind::Attach
            } else {
                spawn_kind(has_stored_id)
            };
            match service
                .acp_supervisor
                .begin_resume_observing(id, kind, authority, observed_stop)
                .await?
            {
                Some(reservation) => reservation,
                None => return Ok(ResumeTrigger::AlreadyResuming),
            }
        }
    };
    let Some(target) = resume_target_for_session(service, id).await else {
        return Ok(ResumeTrigger::NotFound);
    };
    reservation
        .expected_lifecycle_generation
        .get_or_insert(target.lifecycle_generation);
    let service = Arc::clone(service);
    crate::task_util::spawn_supervised(
        "acp.prompt_wake_resume",
        crate::task_util::PanicPolicy::Log,
        async move {
            if reservation.lease().is_borrowed_attach() {
                let deadline = reservation.admission_deadline();
                let attached = timeout_at(
                    deadline,
                    attach_retained_target(&service, &target, reservation),
                )
                .await
                .map_err(|_| SupervisorError::Acp(crate::acp::acp_client::AcpError::AttachTimedOut))
                .and_then(|result| result);
                match attached {
                    Err(error @ SupervisorError::StopSuperseded(_)) => {
                        if let Some(reply) = stop_refused {
                            let _ = reply.send(error);
                        }
                    }
                    Err(error) => {
                        tracing::warn!(target: "acp.supervisor", session = %target.id, %error,
                            "explicit input could not attach the retained actor; preserving its owner");
                    }
                    Ok(()) => {}
                }
                return;
            }
            // Close a turn a dead worker left open before a new prompt lands (#3686).
            if orphaned_turn {
                service
                    .acp_supervisor
                    .synthesize_stopped_for_orphan(&target.id, "orphaned_at_restart");
            }
            let Ok(req) = build_spawn_request(&service, &target, origin).await else {
                return;
            };
            let agent = req.agent.clone();
            match service.acp_supervisor.spawn_inner(req, reservation).await {
                // The supervisor already parked the session; a startup error would bury it,
                // and a prompt into a live limit lands here by design now that one can wake a
                // parked session. Same exemption `resume_one` makes.
                Err(SupervisorError::Acp(crate::acp::acp_client::AcpError::RateLimited(_)))
                | Ok(()) => {}
                Err(error @ SupervisorError::StopSuperseded(_)) => {
                    if let Some(reply) = stop_refused {
                        let _ = reply.send(error);
                    }
                }
                Err(e) => report_spawn_failure(&service, &target, &agent, &e).await,
            }
        },
    );
    Ok(ResumeTrigger::Started)
}

/// Respawns adopted build-stale workers once their turn drains (#1754), like
/// `aoe acp restart`: restart marker, then terminate the stale runner.
pub(super) async fn respawn_drained_stale_workers(state: &Arc<AppState>) {
    for id in state.acp_supervisor.respawn_pending_ids() {
        if state
            .instances
            .read()
            .await
            .iter()
            .any(|row| row.id == id && row.terminal_handoff.is_some())
        {
            continue;
        }
        // A failed probe counts as busy so a live turn is never killed.
        let busy = query_flushed(
            &state.acp_supervisor,
            &state.acp_event_store,
            &id,
            "draining stale worker live work",
            |s, id| s.holds_live_work(id),
        )
        .await;
        if !matches!(
            busy,
            Some(Ok(crate::acp::event_store::ControlKnowledge::Known(false)))
        ) {
            continue;
        }
        tracing::info!(target: "acp.supervisor", session = %id, reason = "build_stale", "stale structured view worker drained; respawning");
        let generation = state
            .acp_supervisor
            .running_identity(&id)
            .map(|identity| identity.generation)
            .or_else(|| {
                worker_registry::load(&id)
                    .ok()
                    .flatten()
                    .map(|r| r.generation)
            });
        // With nothing naming the runner, a marker written by the stop that removed the record stands.
        if let Some(generation) = generation {
            if let Err(error) = worker_registry::mark_restart_pending(&id, generation) {
                tracing::debug!(target: "acp.supervisor", session = %id, %error,
                    "restart publication refused; retaining the current owner");
                continue;
            }
        }
        if let Err(error) = worker_registry::terminate_and_wait(&id).await {
            tracing::warn!(target: "acp.supervisor", session = %id, %error, "runtime termination unproven; retaining restart marker");
            continue;
        }
        state.acp_supervisor.clear_respawn_pending(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_fixtures::test_state;
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    #[serial_test::serial]
    async fn build_spawn_request_reads_live_session_fields() {
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let mut moved = Instance::new("renamed", "/tmp/aoe-2260-after-rename");
        moved.id = "sess-moved".to_string();
        moved.source_profile = "default".into();
        moved.view = crate::session::View::Structured;
        moved.acp_effort = Some("high".to_string());
        let mut unpinned = Instance::new("unpinned", "/tmp/aoe-effort-respawn");
        unpinned.id = "sess-unpinned".to_string();
        unpinned.source_profile = "default".into();
        unpinned.view = crate::session::View::Structured;
        crate::server::test_support::seed_instances_on_disk_for_test(
            "default",
            vec![moved.clone(), unpinned.clone()],
        );
        let state = crate::server::test_support::build_test_app_state(vec![
            moved.clone(),
            unpinned.clone(),
        ]);

        // The snapshot predates a worktree rename (#2260).
        let mut target = ResumeTarget::from_instance(&moved);
        target.project_path = "/tmp/aoe-2260-before-rename".to_string();
        let req = build_spawn_request(&state.session_service, &target, LaunchOrigin::Unattended)
            .await
            .unwrap();
        assert_eq!(req.cwd, PathBuf::from("/tmp/aoe-2260-after-rename"));
        assert_eq!(req.effort.as_deref(), Some("high"));

        let req = build_spawn_request(
            &state.session_service,
            &ResumeTarget::from_instance(&unpinned),
            LaunchOrigin::Unattended,
        )
        .await
        .unwrap();
        assert_eq!(req.effort, None);

        // The model too: a pick landing after the tick snapshotted its targets
        // must reach the worker the tick is about to start. The handshake
        // re-asserts whatever the request carries, so a snapshot read here does
        // not merely miss the pick, it pushes the replaced value at the agent.
        state
            .instances
            .write()
            .await
            .iter_mut()
            .find(|i| i.id == "sess-moved")
            .expect("fixture")
            .agent_model = Some("claude-opus-5".to_string());
        let req = build_spawn_request(&state.session_service, &target, LaunchOrigin::Unattended)
            .await
            .unwrap();
        assert_eq!(req.model.as_deref(), Some("claude-opus-5"));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn build_spawn_request_carries_the_workspace_layout() {
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let mut ws = crate::server::test_support::workspace_instance(
            "ws-resume",
            std::path::Path::new("/tmp/aoe-ws-resume"),
        );
        ws.id = "sess-ws-resume".to_string();
        ws.source_profile = "default".into();
        ws.view = crate::session::View::Structured;
        crate::server::test_support::seed_instances_on_disk_for_test("default", vec![ws.clone()]);
        let state = crate::server::test_support::build_test_app_state(vec![ws.clone()]);

        // The target snapshot carries no workspace; the request must read the live row.
        let mut target = ResumeTarget::from_instance(&ws);
        target.project_path = "/tmp/aoe-ws-resume-stale".to_string();
        let req = build_spawn_request(&state.session_service, &target, LaunchOrigin::Unattended)
            .await
            .unwrap();
        crate::server::test_support::assert_carries_workspace(
            req.workspace_info.as_ref(),
            &ws,
            "build_spawn_request",
        );
    }

    /// #4127: a request is not an observation. Building one derives the route
    /// it will carry, but only a launch that attests one may write the marker,
    /// so the row keeps deriving it and a later alias declaration still reaches
    /// the next launch.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_resume_request_derives_the_route_without_persisting_a_guess() {
        let (home, state, project) = test_state("s-legacy-routing");
        let profile = "legacy-acp-routing";
        let store = home.path().join(".claude");
        let sid = "11111111-1111-4111-8111-111111111111";
        let mut instance = Instance::new("legacy", project.path().to_str().unwrap());
        instance.id = "s-legacy-routing".into();
        instance.view = crate::session::View::Structured;
        instance.source_profile = profile.into();
        instance.agent_session_id = Some(sid.into());
        instance.agent_session_binding = Some(crate::session::ConversationBinding {
            session_id: sid.into(),
            execution: Some(crate::session::ExecutionBinding {
                agent: "claude".into(),
                stores: vec![store],
                configuration: Vec::new(),
                cwd: project.path().to_path_buf(),
                cwd_filesystem: "host".into(),
                filesystem: "host".into(),
                exported_default_store: None,
            }),
            provenance: crate::session::ConversationProvenance::Observed,
            transcript_path: None,
        });
        let storage = crate::session::Storage::new_unwatched(profile).unwrap();
        storage
            .update(|rows, _| {
                *rows = vec![instance.clone()];
                Ok(())
            })
            .unwrap();
        state.instances.write().await[0] = instance;

        let target = {
            let instances = state.instances.read().await;
            ResumeTarget::from_instance(&instances[0])
        };
        let request =
            build_spawn_request(&state.session_service, &target, LaunchOrigin::Unattended)
                .await
                .unwrap();

        assert_eq!(
            request
                .claude_store_pin
                .as_ref()
                .unwrap()
                .exported_default_store,
            Some(false)
        );
        // The resume request carries the derived route, and neither the
        // in-memory row nor the persisted one is rewritten with a guess about
        // the configuration as it stands: a later alias declaration still
        // reaches the next launch.
        let marker = |row: &Instance| {
            row.agent_session_binding
                .as_ref()
                .unwrap()
                .execution
                .as_ref()
                .unwrap()
                .exported_default_store
        };
        assert_eq!(marker(&state.instances.read().await[0]), None);
        let persisted = storage.load().unwrap();
        assert_eq!(marker(&persisted[0]), None);
    }

    /// A live stale worker must never be classified dead, which would lose its PID.
    #[test]
    fn adopt_decision_truth_table() {
        use AdoptDecision::*;
        let cases = [
            ((false, false, false, false), FreshSpawn),
            ((false, true, true, true), FreshSpawn),
            ((true, true, true, false), Attach),
            ((true, true, true, true), Attach),
            ((true, false, true, false), RespawnStaleIdle),
            ((true, false, true, true), AdoptStaleForDrain),
            ((true, true, false, false), ReplaceIncompatibleRunner),
            ((true, true, false, true), ReplaceIncompatibleRunner),
            ((true, false, false, false), ReplaceIncompatibleRunner),
            ((true, false, false, true), ReplaceIncompatibleRunner),
        ];
        for ((live, build, runner, holds_live_work), expected) in cases {
            assert_eq!(
                adopt_decision(live, build, runner, holds_live_work),
                expected,
                "live={live} build={build} runner={runner} holds_live_work={holds_live_work}"
            );
        }
    }

    #[test]
    fn incompatible_replacement_publishes_one_specific_terminal() {
        #[derive(Default)]
        struct Sink(std::sync::Mutex<Vec<Arc<crate::acp::state::Event>>>);
        impl BroadcastSink for Sink {
            fn publish(
                &self,
                _session_id: &str,
                _seq: u64,
                data: &crate::acp::event_sender::AcpEventData,
            ) {
                self.0.lock().unwrap().push(Arc::clone(&data.event));
            }
        }

        let sink = Arc::new(Sink::default());
        let supervisor = Supervisor::new(Arc::clone(&sink));
        publish_orphaned_turn_stop(
            &supervisor,
            "session",
            AdoptDecision::ReplaceIncompatibleRunner,
            true,
        );

        let events = sink.0.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0].as_ref(),
            crate::acp::state::Event::Stopped { reason } if reason == "runner_protocol_upgraded"
        ));
    }

    /// An adopted stale runner's restart marker keeps the generation it stopped.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_drained_stale_respawn_keeps_the_restart_marker_of_its_generation() {
        let (_home, state, _project) = test_state("s-drain");
        let identity = crate::acp::runner_lifecycle::RunnerIdentity::unattested(4242, 4);
        state
            .acp_supervisor
            .test_install_attached("s-drain", identity)
            .await;
        for id in ["s-drain", "s-drain-gone"] {
            state.acp_supervisor.mark_build_respawn_pending(id);
            worker_registry::mark_restart_pending(id, 4).unwrap();
        }

        respawn_drained_stale_workers(&state).await;

        for id in ["s-drain", "s-drain-gone"] {
            assert_eq!(worker_registry::peek_restart_marker(id), Some(4), "{id}");
        }
        assert!(state.acp_supervisor.respawn_pending_ids().is_empty());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn prompt_wake_closes_an_orphaned_turn_before_resuming() {
        use crate::acp::event_store::ControlKnowledge::Known;
        use crate::acp::supervisor::test_support::managed::with_managed;
        for agent_initiated in [false, true] {
            with_managed(|scope| Box::pin(async move {
                let mut live = crate::server::test_support::managed_control_session(scope, "orphaned-real-turn", "claude", None).await;
                if agent_initiated {
                let run_id = uuid::Uuid::new_v4();
                live.native.send(serde_json::json!({"jsonrpc":"2.0","method":"_omp/autonomous_turn/start",
                    "params":{"version":1,"sessionId":live.sid,"runId":run_id,"source":"irc"}})).await;
                live.native.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{
                    "sessionId":live.sid,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"woken"}}
                }})).await;
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        if matches!(live.client.next_event().await, Some(crate::acp::Event::AgentMessageChunk { text, .. }) if text == "woken") { break; }
                    }
                }).await.expect("the actual Native's autonomous turn reaches its durable projection");
                assert_eq!(live.state.acp_event_store.has_open_turn(live.id()).unwrap(), Known(true),
                    "the negotiated original start is durably admitted before losing its observer");
                } else {
                    let _request = live.start_prompt("keep going").await;
                }
                let id = live.id().to_owned();
                let owner = live.state.acp_supervisor.current_native_availability(&id)
                    .await.unwrap().owner.unwrap();
                let paused = live.client.pause_drain().await;
                let guard = crate::process::RunnerProcessGuard::capture(owner.pid, owner.birth_fingerprint).unwrap();
                guard.signal(libc::SIGKILL).expect("retire only this fixture's captured private Native owner");
                tokio::time::timeout(Duration::from_secs(10), async {
                    while guard.executing().unwrap() { tokio::task::yield_now().await; }
                }).await.expect("the original captured owner is conclusively dead");
                live.state.acp_supervisor.test_remove_worker(&id).await;
                drop(paused);
                live.client.actual().shutdown().await.unwrap();
                tokio::time::timeout(Duration::from_secs(10), async {
                    while live.client.next_event().await.is_some() {}
                }).await.expect("the old SDK and its ordered drain have actually retired");
                assert_eq!(live.state.acp_event_store.has_open_turn(&id).unwrap(), Known(true));
                let trigger = trigger_resume_background(&live.state.session_service, &id,
                    crate::process::worker_registry::ResumeAuthority::Explicit, crate::server::sandbox_login::LaunchOrigin::Interactive, None).await.unwrap();
                assert!(matches!(trigger, ResumeTrigger::Started));
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let events = live.state.acp_event_store.replay_from(&id, 0).unwrap();
                        if events.iter().any(|stored| matches!(stored.event.as_ref(),
                            crate::acp::Event::Stopped { reason } if reason == "orphaned_at_restart")) { break; }
                        tokio::task::yield_now().await;
                    }
                }).await.expect("the admitted wake closes the genuine orphan before replacement");
                assert_eq!(live.state.acp_event_store.has_open_turn(&id).unwrap(), Known(false));
            })).await;
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn prompt_wake_defers_while_the_open_turn_is_unknown() {
        use crate::acp::event_store::ControlKnowledge::Known;
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| Box::pin(async move {
                let mut live = crate::server::test_support::managed_control_session(scope, "unknown-orphaned-real-turn", "claude", None).await;
                let run_id = uuid::Uuid::new_v4();
                live.native.send(serde_json::json!({"jsonrpc":"2.0","method":"_omp/autonomous_turn/start",
                    "params":{"version":1,"sessionId":live.sid,"runId":run_id,"source":"irc"}})).await;
                live.native.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{
                    "sessionId":live.sid,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"woken"}}
                }})).await;
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        if matches!(live.client.next_event().await, Some(crate::acp::Event::AgentMessageChunk { text, .. }) if text == "woken") { break; }
                    }
                }).await.expect("the actual Native's autonomous turn reaches its durable projection");
                assert_eq!(live.state.acp_event_store.has_open_turn(live.id()).unwrap(), Known(true),
                    "the original's real lifecycle, not content, establishes the open precondition");
                let id = live.id().to_owned();
                let owner = live.state.acp_supervisor.current_native_availability(&id)
                    .await.unwrap().owner.unwrap();
                let paused = live.client.pause_drain().await;
                let guard = crate::process::RunnerProcessGuard::capture(owner.pid, owner.birth_fingerprint).unwrap();
                guard.signal(libc::SIGKILL).expect("retire only this fixture's captured private Native owner");
                tokio::time::timeout(Duration::from_secs(10), async {
                    while guard.executing().unwrap() { tokio::task::yield_now().await; }
                }).await.expect("the original captured owner is conclusively dead");
                live.state.acp_supervisor.test_remove_worker(&id).await;
                drop(paused);
                live.client.actual().shutdown().await.unwrap();
                tokio::time::timeout(Duration::from_secs(10), async {
                    while live.client.next_event().await.is_some() {}
                }).await.expect("the old SDK and its ordered drain have actually retired");
                assert_eq!(live.state.acp_event_store.has_open_turn(&id).unwrap(), Known(true));
                let authority = crate::process::worker_registry::ResumeAuthority::Explicit;
                live.state.acp_event_store.test_set_unavailable(true);
                let deferred = trigger_resume_background(&live.state.session_service, &id, authority, crate::server::sandbox_login::LaunchOrigin::Interactive, None).await;
                assert!(matches!(deferred, Err(SupervisorError::EventStoreUnavailable(_))));
                assert!(!live.state.acp_supervisor.is_running(&id).await, "nothing spawned");
                live.state.acp_event_store.test_set_unavailable(false);
                let trigger = trigger_resume_background(&live.state.session_service, &id, authority, crate::server::sandbox_login::LaunchOrigin::Interactive, None).await.unwrap();
                assert!(matches!(trigger, ResumeTrigger::Started));
                tokio::time::timeout(Duration::from_secs(10), async {
                    while live.state.acp_event_store.has_open_turn(&id).unwrap() != Known(false) {
                        tokio::task::yield_now().await;
                    }
                }).await.expect("recovery closes the actual orphan rather than losing its history");
        })).await;
    }

    /// A session that left the live set after the snapshot is never spawned.
    #[tokio::test]
    #[serial_test::serial]
    async fn resume_one_rechecks_eligibility_under_the_lease() {
        let id = "s-archived-late";
        let (_home, state, _project) = test_state(id);
        let target = ResumeTarget::from_instance(&state.instances.read().await[0]);
        state.instances.write().await[0].archive();

        let outcome = resume_one(Arc::clone(&state), target).await;
        assert!(matches!(outcome, ResumeOutcome::Retry));
        assert_eq!(
            state.acp_supervisor.worker_state(id).await,
            crate::daemon::AcpWorkerState::Absent
        );
        assert!(state
            .acp_event_store
            .replay_from(id, 0)
            .expect("read retained events")
            .is_empty());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn requeue_interrupted_monitor_queues_only_over_an_armed_monitor() {
        // (armed, description): `None` is how a background command arms.
        for (armed, description) in [
            (true, Some("watch for X".to_string())),
            (true, None),
            (false, None),
        ] {
            let id = "sess-monitor";
            let (_home, state, _project) = test_state(id);
            if armed {
                let event = crate::acp::state::Event::MonitorArmed { description };
                state
                    .acp_event_store
                    .record(
                        id,
                        1,
                        &event,
                        &crate::acp::event_sender::EventMetadata::history(),
                    )
                    .unwrap();
            }

            requeue_interrupted_monitor(&state, id).await;

            let synthesized = state.instances.read().await[0]
                .pending_initial_turn
                .as_ref()
                .map(|t| t.synthesized);
            assert_eq!(synthesized, armed.then_some(true), "armed={armed}");
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn reconciler_attach_timeout_rearms_same_runner_without_orphaning_its_turn() {
        use super::super::test_fixtures::Tick;
        use crate::acp::event_store::ControlKnowledge::Known;
        use crate::acp::supervisor::test_support::managed::with_managed;
        use crate::acp::Event;
        struct SuspendedOwnedRunner(crate::process::RunnerProcessGuard);
        impl Drop for SuspendedOwnedRunner {
            fn drop(&mut self) {
                self.0
                    .signal(libc::SIGCONT)
                    .expect("resume original captured runner");
            }
        }
        with_managed(|scope| {
            Box::pin(async move {
                let mut live = crate::server::test_support::managed_control_session(
                    scope,
                    "reconciler-attach-timeout",
                    "claude",
                    None,
                )
                .await;
                let id = live.id().to_owned();
                let pending = live.start_prompt("original turn remains busy").await;
                let original = live
                    .state
                    .acp_supervisor
                    .current_native_availability(&id)
                    .await
                    .unwrap();
                let owner = original.owner.unwrap();
                let record = worker_registry::load(&id).unwrap().unwrap();
                let methods = live.native.requests();
                live.client = scope
                    .reconcile_after(live.client, |state, id| async move {
                        let guard = crate::process::RunnerProcessGuard::capture(
                            owner.pid,
                            owner.birth_fingerprint,
                        )
                        .unwrap();
                        guard
                            .signal(libc::SIGSTOP)
                            .expect("suspend only the original captured runner");
                        let paused = SuspendedOwnedRunner(guard);
                        let mut tick = Tick::default();
                        tokio::time::timeout(Duration::from_secs(10), tick.run(&state))
                            .await
                            .expect("actual attach handshake returns its bounded timeout");
                        assert!(!tick.attempted.contains(&id));
                        assert_eq!(
                            state.acp_event_store.has_in_flight_turn(&id).unwrap(),
                            Known(true)
                        );
                        let retained = worker_registry::load(&id).unwrap().unwrap();
                        assert_eq!(
                            crate::acp::runner_lifecycle::RunnerIdentity::from_record(&retained),
                            owner
                        );
                        assert_eq!(retained.scope, record.scope);
                        assert_eq!(retained.stored_acp_session_id, record.stored_acp_session_id);
                        assert!(worker_registry::is_record_live(&retained));
                        let events = state.acp_event_store.replay_from(&id, 0).unwrap();
                        assert!(!events.iter().any(|stored| matches!(
                            stored.event.as_ref(),
                            Event::Stopped { .. }
                                | Event::AgentStartupError { .. }
                                | Event::AcpConnectionUnavailable { .. }
                        )));
                        drop(paused);
                        tokio::time::timeout(Duration::from_secs(10), async {
                            loop {
                                tick.run(&state).await;
                                if state
                                    .acp_supervisor
                                    .current_native_availability(&id)
                                    .await
                                    .is_some()
                                {
                                    break;
                                }
                                tokio::time::sleep(Duration::from_millis(20)).await;
                            }
                        })
                        .await
                        .expect("a later tick installs the same borrowed Native actor");
                        let retained = worker_registry::load(&id).unwrap().unwrap();
                        assert_eq!(
                            crate::acp::runner_lifecycle::RunnerIdentity::from_record(&retained),
                            owner
                        );
                    })
                    .await;
                live.client.ready().await;
                live.state = live.client.app_state();
                assert_eq!(
                    live.state
                        .acp_supervisor
                        .current_native_availability(&id)
                        .await
                        .unwrap()
                        .owner,
                    Some(owner)
                );
                assert_eq!(live.native.requests(), methods);
                live.finish_prompt(&pending, None).await;
                let reasons = live.state.acp_event_store.replay_from(&id, 0).unwrap();
                let reasons = reasons
                    .iter()
                    .filter_map(|stored| match stored.event.as_ref() {
                        Event::Stopped { reason } => Some(reason.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                assert_eq!(reasons, ["prompt_complete"]);
                assert_eq!(
                    live.state.acp_event_store.has_in_flight_turn(&id).unwrap(),
                    Known(false)
                );
            })
        })
        .await;
    }
    #[tokio::test]
    #[serial_test::serial]
    async fn a_retained_prompt_completion_does_not_close_an_agent_turn_on_reattach() {
        use super::super::test_fixtures::Tick;
        use crate::acp::event_store::ControlKnowledge::Known;
        use crate::acp::supervisor::test_support::managed::with_managed;
        use crate::acp::Event;
        with_managed(|scope| Box::pin(async move {
            let mut live = crate::server::test_support::managed_control_session(scope, "retained-agent-work", "claude", None).await;
            let id = live.id().to_owned();
            let request = live.start_prompt("go").await;
            live.finish_prompt(&request, None).await;
            let run_id = uuid::Uuid::new_v4();
            live.native.send(serde_json::json!({"jsonrpc":"2.0","method":"_omp/autonomous_turn/start",
                "params":{"version":1,"sessionId":live.sid,"runId":run_id,"source":"irc"}})).await;
            live.native.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{
                "sessionId":live.sid,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"woken"}}
            }})).await;
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if matches!(live.client.next_event().await, Some(Event::AgentMessageChunk { text, .. }) if text == "woken") { break; }
                }
            }).await.expect("the original Native opens autonomous work after its prompt completes");
            assert_eq!(live.state.acp_event_store.holds_live_work(&id).unwrap(), Known(true));
            let admitted = live.state.acp_event_store.replay_from(&id, 0).unwrap();
            let start = admitted.iter().find(|stored| stored.metadata.native_autonomous.as_ref()
                .is_some_and(|autonomous| autonomous.identity.run_id == run_id)).expect("durable original start");
            assert!(matches!(start.event.as_ref(), Event::ThinkingStarted));
            assert!(start.metadata.operation.is_none() && start.metadata.native_prompt_id.is_none());
            assert_eq!(start.metadata.scope, crate::acp::event_sender::EventScope::HistoryOnly);
            let owner = live.client.actual().connected_runner();
            let methods = live.native.requests();
            live.client = scope.reconcile_after(live.client, |state, id| async move {
                let mut tick = Tick::default();
                tick.run(&state).await;
                assert!(state.acp_supervisor.current_lease(&id).is_some());
            }).await;
            live.state = live.client.app_state();
            assert_eq!(live.client.actual().connected_runner(), owner);
            assert_eq!(live.native.requests(), methods);
            live.native.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{
                "sessionId":live.sid,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"after the retained completion"}}
            }})).await;
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if matches!(live.client.next_event().await, Some(Event::AgentMessageChunk { text, .. }) if text == "after the retained completion") { break; }
                }
            }).await.expect("the same actual stream progresses beyond its retained prompt completion");
            let stops = live.state.acp_event_store.replay_from(&id, 0).unwrap().into_iter()
                .filter(|stored| matches!(stored.event.as_ref(), Event::Stopped { .. })).count();
            assert_eq!(stops, 1, "the retained completion cannot publish another turn's Stopped");
            assert_eq!(live.state.acp_event_store.holds_live_work(&id).unwrap(), Known(true));
            live.native.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{
                "sessionId":live.sid,"update":{"sessionUpdate":"usage_update","used":10,"size":100,
                    "cost":{"amount":0.1,"currency":"USD"}}
            }})).await;
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if matches!(live.client.next_event().await, Some(Event::UsageUpdated { .. })) { break; }
                }
            }).await.expect("the actual accounting frame reaches the durable stream");
            assert_eq!(live.state.acp_event_store.holds_live_work(&id).unwrap(), Known(true),
                "accounting cannot settle the independently admitted original");
            assert_eq!(live.state.acp_event_store.replay_from(&id, 0).unwrap().iter()
                .filter(|stored| matches!(stored.event.as_ref(), Event::Stopped { .. })).count(), 1);
            live.native.send(serde_json::json!({"jsonrpc":"2.0","method":"_omp/autonomous_turn/end",
                "params":{"version":1,"sessionId":live.sid,"runId":run_id,"source":"irc","stopReason":"end_turn"}})).await;
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    if matches!(live.client.next_event().await, Some(Event::Stopped { reason }) if reason == "agent_idle") { break; }
                }
            }).await.expect("only this autonomous original's own typed terminal releases its live-work hold");
            assert_eq!(live.state.acp_event_store.holds_live_work(&id).unwrap(), Known(false));
            let completed = live.state.acp_event_store.replay_from(&id, 0).unwrap();
            assert!(completed.windows(2).all(|pair| pair[0].seq.checked_add(1) == Some(pair[1].seq)),
                "replayed committed phases must not consume unused journal sequence candidates");
            assert_eq!(completed.iter().filter(|stored| matches!(stored.event.as_ref(), Event::Stopped { .. })).count(), 2);
            let terminal = completed.iter().find(|stored| stored.metadata.native_autonomous.as_ref()
                .is_some_and(|autonomous| autonomous.identity.run_id == run_id
                    && autonomous.phase == crate::acp::control_protocol::NativeAutonomousPhase::Ended))
                .expect("the original's durable correlated terminal");
            assert!(terminal.metadata.operation.is_none() && terminal.metadata.native_prompt_id.is_none());
        })).await;
    }
}
