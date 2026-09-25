//! Resuming a single session's worker: attach to a live runner or fresh-spawn one.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::timeout;

use super::{is_resumable, query_store, AppState, ResumeOutcome};
use crate::acp::supervisor::{
    AgentCommandOverride, BroadcastSink, ResumeKind, ResumeReservationOutcome, SpawnRequest,
    Supervisor, SupervisorError,
};
use crate::process::worker_registry;
use crate::server::session_service::SessionService;
use crate::session::Instance;

/// A structured view session that needs a worker, snapshotted at the tick so
/// resume tasks need not re-scan the instance list. Anything that a later pick
/// can change is re-read in `build_spawn_request`, not carried here.
#[derive(Clone)]
pub(super) struct ResumeTarget {
    pub(super) id: String,
    tool: String,
    agent_override: Option<String>,
    pub(super) project_path: String,
    stored_acp_session_id: Option<String>,
    source_profile: String,
    pub(super) in_flight_turn: bool,
    yolo_mode: bool,
    /// The resolved launch command, honored like tmux does (#1766).
    command: String,
}

impl ResumeTarget {
    pub(super) fn from_instance(inst: &Instance) -> Self {
        Self {
            id: inst.id.clone(),
            tool: inst.tool.clone(),
            agent_override: inst.agent_name.clone(),
            project_path: inst.project_path.clone(),
            stored_acp_session_id: inst.acp_session_id.clone(),
            source_profile: inst.source_profile.clone(),
            in_flight_turn: false,
            yolo_mode: inst.yolo_mode,
            command: inst.command.clone(),
        }
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
    in_flight_turn: bool,
) -> AdoptDecision {
    match (live, runner_current, build_current, in_flight_turn) {
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

/// A `Monitor` watch lives inside the agent process, so a replacement process
/// has lost it; the reminder gives the agent a turn to re-arm it.
const MONITOR_RESTART_REMINDER: &str = "Note: this session's agent process restarted, so any background Monitor watch that was armed is no longer running. Re-arm it if it's still needed.";

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
        .set_pending_initial_turn(session_id, MONITOR_RESTART_REMINDER.to_string(), vec![])
        .await;
}

async fn admit(
    state: &AppState,
    id: &str,
    kind: ResumeKind,
) -> Result<crate::acp::supervisor::ResumeReservation, ResumeOutcome> {
    match state.acp_supervisor.begin_resume(id, kind).await {
        Ok(ResumeReservationOutcome::Reserved(r)) => Ok(r),
        Ok(ResumeReservationOutcome::AlreadyPresent) => Err(ResumeOutcome::SpawnFinished),
        Err(e @ SupervisorError::CapacityFull { .. }) => Err(ResumeOutcome::CapacityDeferred {
            message: e.to_string(),
        }),
        Err(e) => {
            tracing::debug!(target: "acp.supervisor", session = %id, "resume not admitted: {e}");
            Err(ResumeOutcome::SpawnFinished)
        }
    }
}

pub(super) async fn resume_one(state: Arc<AppState>, target: ResumeTarget) -> ResumeOutcome {
    let id = target.id.clone();
    let in_flight_turn = target.in_flight_turn;

    // Take the lease before any preparation so a stop landing from here on is honored.
    let record = worker_registry::load(&id).ok().flatten();
    let decision = record.as_ref().map_or(AdoptDecision::FreshSpawn, |r| {
        adopt_decision(
            worker_registry::is_record_live(r),
            worker_registry::is_build_current(r),
            worker_registry::is_runner_current(r),
            in_flight_turn,
        )
    });
    let kind = match decision {
        AdoptDecision::Attach | AdoptDecision::AdoptStaleForDrain => ResumeKind::Attach,
        _ => ResumeKind::Spawn,
    };
    let mut reservation = match admit(&state, &id, kind).await {
        Ok(r) => r,
        Err(outcome) => return outcome,
    };
    // The snapshot may predate an archive, snooze, trash or stop.
    if resume_target_for_session(&state.session_service, &id)
        .await
        .is_none()
    {
        tracing::debug!(target: "acp.supervisor", session = %id, "session left the resume set after the snapshot; not resuming");
        return ResumeOutcome::SpawnFinished;
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
                let sandbox = {
                    let instances = state.instances.read().await;
                    instances
                        .iter()
                        .find(|i| i.id == id)
                        .and_then(|i| i.sandbox_info.clone())
                };
                let attach = state.acp_supervisor.attach_inner(
                    id.clone(),
                    PathBuf::from(&target.project_path),
                    vec![],
                    in_flight_turn,
                    sandbox,
                    reservation,
                );
                match timeout(Duration::from_secs(3), attach).await {
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
                    Ok(Err(SupervisorError::SpawnCancelled(_))) => {
                        return ResumeOutcome::SpawnFinished
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(target: "acp.supervisor", session = %id, "attach failed; terminating the worker and falling back to fresh spawn: {e}");
                        worker_registry::terminate_and_wait(&id).await;
                    }
                    Err(_) => {
                        tracing::warn!(target: "acp.supervisor", session = %id, "attach timed out after 3s; terminating the worker and falling back to fresh spawn");
                        worker_registry::terminate_and_wait(&id).await;
                        return ResumeOutcome::RetryAfterAttachTimeout;
                    }
                }
                // The failed attach released its lease; the spawn needs one that counts toward capacity.
                reservation = match admit(&state, &id, ResumeKind::Spawn).await {
                    Ok(r) => r,
                    Err(outcome) => return outcome,
                };
            }
            AdoptDecision::ReplaceIncompatibleRunner => {
                tracing::info!(
                    target: "acp.supervisor",
                    session = %id,
                    old_runner_version = record.runner_version,
                    new_runner_version = worker_registry::RUNNER_VERSION,
                    "replacing incompatible structured view runner"
                );
                worker_registry::terminate_and_wait(&id).await;
            }
            AdoptDecision::RespawnStaleIdle => {
                tracing::info!(
                    target: "acp.supervisor",
                    session = %id,
                    old_build = %record.build_version,
                    new_build = crate::build_info::BUILD_VERSION,
                    "respawning idle build-stale structured view worker on current binary"
                );
                worker_registry::terminate_and_wait(&id).await;
            }
            // A dead record can still hold a live pid whose socket vanished.
            AdoptDecision::FreshSpawn => worker_registry::terminate_and_wait(&id).await,
        }
    }

    publish_orphaned_turn_stop(&state.acp_supervisor, &id, decision, in_flight_turn);
    requeue_interrupted_monitor(&state, &id).await;
    let Ok(req) = build_spawn_request(&state.session_service, &target).await else {
        return ResumeOutcome::SpawnFinished;
    };
    let agent = req.agent.clone();
    match state.acp_supervisor.spawn_inner(req, reservation).await {
        Err(e @ SupervisorError::CapacityFull { .. }) => {
            return ResumeOutcome::CapacityDeferred {
                message: e.to_string(),
            }
        }
        // The supervisor already parked the session; a startup error would bury it.
        Err(SupervisorError::Acp(crate::acp::acp_client::AcpError::RateLimited(_))) | Ok(()) => {}
        Err(e) => report_spawn_failure(&state.session_service, &id, &agent, &e).await,
    }
    ResumeOutcome::SpawnFinished
}

async fn seed_in_flight_status(state: &AppState, id: &str) {
    // Cold control-state fold at reattach time, same caveat as
    // `seed_acp_statuses` (#4001).
    let Some(intent) = state
        .acp_event_store
        .latest_seed_status_event(id)
        .and_then(|event| crate::server::derive_acp_status(&event, false, false))
    else {
        return;
    };
    let mut instances = state.instances.write().await;
    if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
        crate::server::apply_status_intent(inst, Some(intent), &state.status_tx);
    }
}

/// Publishes a startup error for a real spawn failure of a still-present session.
async fn report_spawn_failure(
    service: &SessionService,
    id: &str,
    agent: &str,
    e: &SupervisorError,
) {
    if matches!(
        e,
        SupervisorError::AlreadyRunning(_) | SupervisorError::SpawnCancelled(_)
    ) || !service.instances.read().await.iter().any(|i| i.id == id)
    {
        return;
    }
    let message = format!("Failed to start structured view agent {agent:?}: {e}");
    tracing::warn!(target: "acp.supervisor", session = %id, agent = %agent, "structured view spawn failed: {message}");
    service.acp_supervisor.publish_startup_error(id, message);
}

/// Builds a fresh-spawn request; on sandbox failure publishes a startup error and returns `Err`.
async fn build_spawn_request(
    service: &Arc<SessionService>,
    target: &ResumeTarget,
) -> Result<SpawnRequest, ()> {
    let supervisor = &service.acp_supervisor;
    let inst_lock = service.instance_lock(&target.id).await;
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
        agent_model,
        claude_store_pin,
    ) = {
        let _guard = inst_lock.lock().await;
        let instances = service.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == target.id) else {
            return Err(());
        };
        (
            PathBuf::from(&inst.project_path),
            inst.import_pending == Some(true),
            inst.fork_pending.clone(),
            inst.acp_mode_id.clone(),
            inst.acp_effort.clone(),
            inst.agent_model.clone(),
            inst.selected_claude_conversation()
                .and_then(|(_, execution)| crate::session::capture::ClaudeStorePin::of(execution)),
        )
    };
    let agent = supervisor
        .pick_agent_for_tool(
            &target.tool,
            target.agent_override.as_deref(),
            &target.source_profile,
            &cwd,
        )
        .await;
    let sandbox_info = match crate::acp::sandbox::ensure_container_for_session(
        &service.instances,
        &inst_lock,
        &target.id,
        false,
    )
    .await
    {
        Ok(info) => info,
        Err(e) => {
            let message = format!("sandbox container ensure failed: {e}");
            tracing::warn!(target: "acp.supervisor", session = %target.id, "reconciler container ensure failed: {message}");
            supervisor.publish_startup_error(&target.id, message);
            return Err(());
        }
    };

    Ok(SpawnRequest {
        session_id: target.id.clone(),
        agent,
        tool: target.tool.clone(),
        cwd,
        additional_dirs: vec![],
        provider_env: vec![],
        model: agent_model,
        // `acp_effort` only holds a user-set effort, so presence is its provenance.
        effort_explicit: acp_effort.is_some(),
        effort: acp_effort,
        stored_acp_session_id: target.stored_acp_session_id.clone(),
        fork_from,
        sandbox_continuation: crate::acp::supervisor::SandboxContinuation::Persisted,
        sandbox_info,
        source_profile: Some(target.source_profile.clone()),
        yolo_mode: target.yolo_mode,
        acp_mode_id,
        agent_command_override: command_override_for_spawn(&target.tool, &target.command),
        seed_history_replay,
        claude_store_pin,
    })
}

/// Command override from a persisted launch command; `None` keeps the registry default.
pub(crate) fn command_override_for_spawn(
    tool: &str,
    command: &str,
) -> Option<AgentCommandOverride> {
    let command = command.trim();
    (!command.is_empty()).then(|| AgentCommandOverride {
        logical_tool: tool.to_string(),
        command: command.to_string(),
    })
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

/// Reserves a resume slot for `id`, then spawns the worker in a detached task
/// that survives the originating request (#1748).
///
/// Nothing may hold `instance_lock` while awaiting worker readiness: the
/// detached spawn takes it in `build_spawn_request` (#3172, #3621).
pub(crate) async fn trigger_resume_background(
    service: &Arc<SessionService>,
    id: &str,
) -> Result<ResumeTrigger, SupervisorError> {
    service.acp_supervisor.forget_stale_cancel(id);
    let reservation = match service
        .acp_supervisor
        .begin_resume(id, ResumeKind::Spawn)
        .await?
    {
        ResumeReservationOutcome::Reserved(r) => r,
        ResumeReservationOutcome::AlreadyPresent => return Ok(ResumeTrigger::AlreadyResuming),
    };
    let Some(target) = resume_target_for_session(service, id).await else {
        return Ok(ResumeTrigger::NotFound);
    };
    let service = Arc::clone(service);
    crate::task_util::spawn_supervised(
        "acp.prompt_wake_resume",
        crate::task_util::PanicPolicy::Log,
        async move {
            // Close a turn a dead worker left open before a new prompt lands (#3686).
            let in_flight = query_store(
                &service.acp_event_store,
                &target.id,
                "in-flight turn",
                |s, id| s.has_in_flight_turn(id),
            )
            .await
            .unwrap_or(false);
            if in_flight {
                service
                    .acp_supervisor
                    .synthesize_stopped_for_orphan(&target.id, "orphaned_at_restart");
            }
            let Ok(req) = build_spawn_request(&service, &target).await else {
                return;
            };
            let agent = req.agent.clone();
            match service.acp_supervisor.spawn_inner(req, reservation).await {
                // The supervisor already parked the session; a startup error would bury it,
                // and a prompt into a live limit lands here by design now that one can wake a
                // parked session. Same exemption `resume_one` makes.
                Err(SupervisorError::Acp(crate::acp::acp_client::AcpError::RateLimited(_)))
                | Ok(()) => {}
                Err(e) => report_spawn_failure(&service, &target.id, &agent, &e).await,
            }
        },
    );
    Ok(ResumeTrigger::Started)
}

/// Respawns adopted build-stale workers once their turn drains (#1754), like
/// `aoe acp restart`: restart marker, then terminate the stale runner.
pub(super) async fn respawn_drained_stale_workers(state: &Arc<AppState>) {
    for id in state.acp_supervisor.respawn_pending_ids() {
        // A failed probe counts as busy so a live turn is never killed.
        let in_flight = query_store(
            &state.acp_event_store,
            &id,
            "draining stale worker in-flight",
            |s, id| s.has_in_flight_turn(id),
        )
        .await
        .unwrap_or(true);
        if in_flight {
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
            worker_registry::mark_restart_pending(&id, generation);
        }
        worker_registry::terminate_and_wait(&id).await;
        state.acp_supervisor.clear_respawn_pending(&id);
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_fixtures::{structured_instance, test_state};
    use super::*;

    #[tokio::test]
    async fn build_spawn_request_reads_live_session_fields() {
        let mut moved = Instance::new("renamed", "/tmp/aoe-2260-after-rename");
        moved.id = "sess-moved".to_string();
        moved.view = crate::session::View::Structured;
        moved.acp_effort = Some("high".to_string());
        let mut unpinned = Instance::new("unpinned", "/tmp/aoe-effort-respawn");
        unpinned.id = "sess-unpinned".to_string();
        unpinned.view = crate::session::View::Structured;
        let state = crate::server::test_support::build_test_app_state(vec![
            moved.clone(),
            unpinned.clone(),
        ]);

        // The snapshot predates a worktree rename (#2260).
        let mut target = ResumeTarget::from_instance(&moved);
        target.project_path = "/tmp/aoe-2260-before-rename".to_string();
        let req = build_spawn_request(&state.session_service, &target)
            .await
            .unwrap();
        assert_eq!(req.cwd, PathBuf::from("/tmp/aoe-2260-after-rename"));
        assert_eq!(req.effort.as_deref(), Some("high"));

        let req = build_spawn_request(
            &state.session_service,
            &ResumeTarget::from_instance(&unpinned),
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
        let req = build_spawn_request(&state.session_service, &target)
            .await
            .unwrap();
        assert_eq!(req.model.as_deref(), Some("claude-opus-5"));
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
        for ((live, build, runner, in_flight), expected) in cases {
            assert_eq!(
                adopt_decision(live, build, runner, in_flight),
                expected,
                "live={live} build={build} runner={runner} in_flight={in_flight}"
            );
        }
    }

    #[test]
    fn incompatible_replacement_publishes_one_specific_terminal() {
        #[derive(Default)]
        struct Sink(std::sync::Mutex<Vec<crate::acp::state::Event>>);
        impl BroadcastSink for Sink {
            fn publish(&self, _session_id: &str, _seq: u64, event: &crate::acp::state::Event) {
                self.0.lock().unwrap().push(event.clone());
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
            &events[0],
            crate::acp::state::Event::Stopped { reason } if reason == "runner_protocol_upgraded"
        ));
    }

    /// An adopted stale runner's restart marker keeps the generation it stopped.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_drained_stale_respawn_keeps_the_restart_marker_of_its_generation() {
        let (_home, state, _project) = test_state("s-drain");
        let identity = crate::acp::runner_lifecycle::RunnerIdentity {
            pid: 4242,
            generation: 4,
        };
        state
            .acp_supervisor
            .test_install_attached("s-drain", identity)
            .await;
        for id in ["s-drain", "s-drain-gone"] {
            state.acp_supervisor.mark_build_respawn_pending(id);
            worker_registry::mark_restart_pending(id, 4);
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
        let id = "sess-3686-orphan";
        let (_home, state, _project) = test_state(id);
        let prompt = crate::acp::Event::UserPromptSent {
            text: "keep going".into(),
            attachments: Vec::new(),
            prompt_id: None,
            synthesized: false,
        };
        state.acp_event_store.record(id, 1, &prompt).unwrap();
        state.acp_supervisor.hydrate_seqs([(id.to_string(), 1)]);

        let trigger = trigger_resume_background(&state.session_service, id)
            .await
            .expect("resume is admitted");
        assert!(matches!(trigger, ResumeTrigger::Started));

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !state.acp_event_store.replay_from(id, 0).into_iter().any(
            |(_, e)| matches!(e, crate::acp::Event::Stopped { reason } if reason == "orphaned_at_restart"),
        ) {
            assert!(tokio::time::Instant::now() < deadline, "orphaned turn was not closed");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!state.acp_event_store.has_in_flight_turn(id));
    }

    /// A session that left the live set after the snapshot is never spawned.
    #[tokio::test]
    #[serial_test::serial]
    async fn resume_one_rechecks_eligibility_under_the_lease() {
        let id = "s-archived-late";
        let (_home, state, project) = test_state(id);
        let target = ResumeTarget::from_instance(&structured_instance(
            id,
            &project.path().to_string_lossy(),
        ));
        state.instances.write().await[0].archive();

        let outcome = resume_one(Arc::clone(&state), target).await;
        assert!(matches!(outcome, ResumeOutcome::SpawnFinished));
        assert_eq!(
            state.acp_supervisor.worker_state(id).await,
            crate::daemon::AcpWorkerState::Absent
        );
        assert!(state.acp_event_store.replay_from(id, 0).is_empty());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn requeue_interrupted_monitor_queues_only_over_an_armed_monitor() {
        for armed in [true, false] {
            let id = "sess-monitor";
            let (_home, state, _project) = test_state(id);
            if armed {
                let event = crate::acp::state::Event::MonitorArmed {
                    description: Some("watch for X".to_string()),
                };
                state.acp_event_store.record(id, 1, &event).unwrap();
            }

            requeue_interrupted_monitor(&state, id).await;

            let synthesized = state.instances.read().await[0]
                .pending_initial_turn
                .as_ref()
                .map(|t| t.synthesized);
            assert_eq!(synthesized, armed.then_some(true), "armed={armed}");
        }
    }
}
