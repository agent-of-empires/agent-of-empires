//! Worker spawn, shutdown, and agent switching.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::acp::event_store::ControlKnowledge::{self, Indeterminate, Known};
use crate::acp::protocol::{
    SwitchAgentRequest, SwitchAgentResponse, SwitchProviderRequest, SwitchProviderResponse,
};
use crate::server::acp_reconciler::{resume_rate_limited, QuotaResume};
use crate::server::api::find_instance;

use super::*;

#[derive(Debug, Deserialize)]
pub struct SpawnAcpRequest {
    /// Falls back to `Supervisor::pick_agent_for_tool`.
    pub agent: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub recovery: Option<crate::acp::establishment::EstablishmentRecovery>,
    /// Extra dirs the agent may use through fs/*; the worktree is always allowed.
    #[serde(default)]
    pub additional_dirs: Vec<PathBuf>,
    /// Filtered against the agent's allowlist.
    #[serde(default)]
    pub provider_env: Vec<EnvPair>,
}

#[derive(Debug, Deserialize)]
pub struct EnvPair {
    pub key: String,
    pub value: String,
}

#[derive(Debug, Serialize)]
pub struct SpawnAcpResponse {
    pub session_id: String,
    pub agent: String,
    pub status: &'static str,
}

fn not_structured_response() -> Response {
    super::super::api_error(
        StatusCode::CONFLICT,
        "not_structured",
        "Switch the session to structured view before starting an ACP worker",
    )
}

/// The resume-at instant a manual resume reports, from the session's durable
/// rate-limit park. A cap park has no schedule, so it uses the fallback.
fn rate_limit_resume_marker_resets_at(
    park: Option<&crate::acp::event_store::RateLimitPark>,
    fallback_resets_at: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let park = park?;
    if park.cap_reached {
        return Some(fallback_resets_at);
    }
    Some(
        park.info
            .as_ref()
            .and_then(|info| info.resets_at)
            .unwrap_or(fallback_resets_at),
    )
}

async fn rate_limit_resume_probe(
    state: &AppState,
    id: &str,
) -> Result<ControlKnowledge<Option<DateTime<Utc>>>, SupervisorError> {
    let store = Arc::clone(&state.acp_event_store);
    let id_for_probe = id.to_string();
    let park = tokio::task::spawn_blocking(move || store.rate_limit_park(&id_for_probe))
        .await
        .map_err(|_| SupervisorError::NativeStoreUnavailable)?
        .map_err(|_| SupervisorError::NativeStoreUnavailable)?;
    Ok(match park {
        Known(park) => Known(rate_limit_resume_marker_resets_at(
            park.as_ref(),
            Utc::now(),
        )),
        Indeterminate => Indeterminate,
    })
}

/// The memory check runs before the handler's awaits, during which a peer such as
/// `aoe session archive` or `aoe rm --purge` can shelve or remove the stored row, so
/// recheck it right before spawning.
async fn refuse_if_stored_row_shelved(
    state: &AppState,
    instance: &crate::session::Instance,
) -> Option<Response> {
    match crate::server::api::load_persisted_instance(state, &instance.source_profile, &instance.id)
        .await
    {
        // A purge removes the row while the cache may still hold it.
        Ok(None) => Some(session_not_found()),
        Ok(Some(stored)) => stored
            .ensure_startable()
            .err()
            .map(crate::server::api::start_blocked_response),
        Err(resp) => Some(resp),
    }
}

pub async fn spawn_acp(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Result<Json<SpawnAcpRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    if let Some(resp) = cityhall_block(&state) {
        return resp;
    }
    let Json(req) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };
    if req.recovery.is_some() {
        return restart_acp(State(state), Path(id), Ok(Json(req)))
            .await
            .into_response();
    }
    let spawn_request = match state.acp_supervisor.begin_spawn_request(&id) {
        Ok(request) => request,
        Err(error) => return supervisor_error_response("spawn cancelled", &error),
    };
    // A stop must be able to cancel a launch waiting on either exclusion guard.
    let mut exclusion = tokio::select! {
        biased;
        _ = spawn_request.cancellation().cancelled() => {
            return supervisor_error_response(
                "spawn cancelled",
                &SupervisorError::SpawnCancelled(id.clone()),
            );
        }
        exclusion = crate::acp::sandbox::LaunchExclusion::acquire(state.clone(), &id, true) => {
            match exclusion {
                Ok(exclusion) => exclusion,
                Err(_) => return session_not_found(),
            }
        }
    };
    let Some(instance) = find_instance(&state, &id).await else {
        return session_not_found();
    };
    if spawn_request.cancellation().is_cancelled() {
        return supervisor_error_response(
            "spawn cancelled",
            &SupervisorError::SpawnCancelled(id.clone()),
        );
    }
    if !instance.is_structured() {
        return not_structured_response();
    }
    if instance.terminal_handoff.is_some() {
        return (StatusCode::CONFLICT, "terminal handoff is pending").into_response();
    }
    if let Err(blocked) = instance.ensure_startable() {
        return crate::server::api::start_blocked_response(blocked);
    }
    if !instance.launch_is_finalized() {
        return (StatusCode::CONFLICT, Json(serde_json::json!({
            "error": "creation_not_finalized", "message": "The structured session launch is not finalized",
        }))).into_response();
    }
    if let Some(response) = refuse_if_stored_row_shelved(&state, &instance).await {
        return response;
    }
    if let Err(refusal) = instance.require_resolved_acp_selection() {
        return supervisor_error_response("spawn refused", &refusal.into());
    }
    let quota = match rate_limit_resume_probe(&state, &id).await {
        Ok(quota) => quota,
        Err(error) => return supervisor_error_response("quota history probe failed", &error),
    };
    if matches!(quota, Indeterminate) {
        let agent = pick_agent(&state, &instance, None).await;
        if req
            .agent
            .as_ref()
            .is_some_and(|requested| requested != &agent)
            || req.model.is_some()
            || req.recovery.is_some()
            || !req.additional_dirs.is_empty()
            || !req.provider_env.is_empty()
        {
            return supervisor_error_response(
                "only read-only reattachment is available with unconfirmed quota history",
                &SupervisorError::ControlStateUnavailable,
            );
        }
        drop(exclusion);
        if let Err(error) = crate::server::acp_reconciler::ensure_rate_limit_worker(
            &state,
            &id,
            crate::process::worker_registry::ResumeAuthority::Explicit,
            true,
        )
        .await
        {
            return supervisor_error_response("retained attachment failed", &error);
        }
        return Json(SpawnAcpResponse {
            session_id: id,
            agent,
            status: "running",
        })
        .into_response();
    }
    if matches!(quota, Known(Some(_))) {
        let explicit = req.agent.as_deref().or(instance.agent_name.as_deref());
        let agent = pick_agent(&state, &instance, explicit).await;
        let submission = exclusion
            .take_submission()
            .expect("launch owns prompt submission");
        drop(exclusion);
        if let Err(error) = resume_rate_limited(&state, &id, submission, QuotaResume::Manual).await
        {
            return supervisor_error_response("quota continuation failed", &error);
        }
        return Json(SpawnAcpResponse {
            session_id: id,
            agent,
            status: "running",
        })
        .into_response();
    }
    if req.agent.is_none()
        && req.model.is_none()
        && req.additional_dirs.is_empty()
        && req.provider_env.is_empty()
        && state.acp_supervisor.is_running(&id).await
    {
        match current_is_noop(&state, &instance, None).await {
            Ok(true) => {
                return Json(SpawnAcpResponse {
                    session_id: id,
                    agent: pick_agent(&state, &instance, instance.agent_name.as_deref()).await,
                    status: "running",
                })
                .into_response();
            }
            Ok(false) => {}
            Err(response) => return response,
        }
    }

    let (instance, resume_reservation) =
        match crate::server::api::sessions::resume_structured_stop(&state, instance, None).await {
            Ok(instance) => instance,
            Err(response) => return response,
        };
    let explicit = req.agent.clone().or_else(|| instance.agent_name.clone());
    let agent = pick_agent(&state, &instance, explicit.as_deref()).await;
    let (native, exclusion) = match launch_store_for(&state, &instance, exclusion).await {
        Ok(store) => store,
        Err(response) => return response,
    };
    let mut reservation = if let Some(reservation) = resume_reservation {
        reservation
    } else {
        match state
            .acp_supervisor
            .reserve_spawn(
                &id,
                if instance.acp_session_id.is_some() {
                    crate::acp::supervisor::ResumeKind::Spawn
                } else {
                    crate::acp::supervisor::ResumeKind::SpawnFirst
                },
                crate::process::worker_registry::ResumeAuthority::Explicit,
            )
            .await
        {
            Ok(reservation) => reservation,
            Err(error) => return supervisor_error_response("spawn failed", &error),
        }
    };
    let mut admission = crate::acp::supervisor::LaunchAdmission {
        establishment: reservation.establishment.clone(),
        store: native.clone(),
        generation: instance.lifecycle_generation,
        namespace: Some(state.profile_namespace.clone()),
        native_scope: None,
    };
    let instance = match exclusion
        .prepare_native_establishment(&state.acp_supervisor, &mut admission, &mut reservation)
        .await
    {
        Ok(row) => row,
        Err(error) => return supervisor_error_response("spawn admission failed", &error),
    };
    let (sandbox_info, exclusion) = match crate::acp::sandbox::ensure_container_for_session(
        native.clone(),
        instance.clone(),
        exclusion,
    )
    .await
    {
        Ok(result) => result,
        Err(e) => return launch_error_response("sandbox container ensure", &e),
    };
    if spawn_request.cancellation().is_cancelled() {
        return supervisor_error_response(
            "spawn cancelled",
            &SupervisorError::SpawnCancelled(id.clone()),
        );
    }

    // Explicit resume has settled and consumed only the observed native stop.

    let request = SpawnRequest {
        launch_admission: Some(admission),
        additional_dirs: req.additional_dirs,
        provider_env: req
            .provider_env
            .into_iter()
            .map(|p| (p.key, p.value))
            .collect(),
        model: req.model.or_else(|| instance.agent_model.clone()),
        ..spawn_request_for(&instance, agent.clone(), sandbox_info)
    };
    if let Some(resp) = refuse_if_stored_row_shelved(&state, &instance).await {
        return resp;
    }
    if spawn_request.cancellation().is_cancelled() {
        drop(reservation);
        return supervisor_error_response(
            "spawn cancelled",
            &SupervisorError::SpawnCancelled(id.clone()),
        );
    }
    drop(exclusion);
    if let Err(e) = state.acp_supervisor.spawn_inner(request, reservation).await {
        return supervisor_error_response("spawn failed", &e);
    }
    Json(SpawnAcpResponse {
        session_id: id,
        agent,
        status: "running",
    })
    .into_response()
}

/// Stop and start the structured-view worker as one operation. Unlike a plain
/// spawn, this endpoint also cancels a launch that is stuck in
/// `session/new`/`session/load` and proves the old worker is gone before it
/// admits the replacement.
pub async fn restart_acp(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Result<Json<SpawnAcpRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    if let Some(resp) = cityhall_block(&state) {
        return resp;
    }
    let Json(req) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };
    let work = state.runtime.work.clone();
    match work
        .run("server.acp.restart", async move {
            restart_acp_inner(state, id, req, None).await
        })
        .await
    {
        Ok(response) => response,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

/// The public restart route shares this worker admission, not another HTTP handler.
pub(crate) async fn restart_session_recovery(
    state: Arc<AppState>,
    id: String,
    recovery: crate::session::SessionRecoveryRequest,
) -> Response {
    let request = SpawnAcpRequest {
        agent: None,
        model: None,
        recovery: None,
        additional_dirs: Vec::new(),
        provider_env: Vec::new(),
    };
    let work = state.runtime.work.clone();
    let request_state = state.clone();
    let request_id = id.clone();
    // Keep the restart state machine out of by-value moves through the work registry and task spawn.
    let response = match work
        .run(
            "server.session.recovery",
            Box::pin(restart_acp_inner(
                request_state,
                request_id,
                request,
                Some(recovery),
            )),
        )
        .await
    {
        Ok(response) => response,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    if !response.status().is_success() {
        return response;
    }
    let row = state
        .instances
        .read()
        .await
        .iter()
        .find(|row| row.id == id)
        .cloned();
    let Some(row) = row else {
        return session_not_found();
    };
    crate::server::runtime::session_mutation_response(
        &state,
        &id,
        Some(crate::daemon::RestartOutcome {
            lifecycle_generation: row.lifecycle_generation,
            profile: row.source_profile,
            target: None,
        }),
    )
    .await
}

async fn restart_acp_inner(
    state: Arc<AppState>,
    id: String,
    req: SpawnAcpRequest,
    session_recovery: Option<crate::session::SessionRecoveryRequest>,
) -> Response {
    let restart = match state
        .acp_supervisor
        .try_begin_restart_with_reason(&id)
        .await
    {
        Ok(restart) => restart,
        Err(crate::acp::supervisor::RestartClaimConflict::Switch) => {
            return (
                StatusCode::CONFLICT,
                "switch_in_progress: an agent switch is already in progress for this session",
            )
                .into_response();
        }
        Err(crate::acp::supervisor::RestartClaimConflict::Restart) => {
            return (
                StatusCode::CONFLICT,
                "restart_in_progress: a restart is already in progress for this session",
            )
                .into_response();
        }
    };
    let Some(preflight_instance) = find_instance(&state, &id).await else {
        return session_not_found();
    };
    if !preflight_instance.is_structured() {
        return not_structured_response();
    }
    if preflight_instance.terminal_handoff.is_some() {
        return (StatusCode::CONFLICT, "terminal handoff is pending").into_response();
    }
    if let Err(blocked) = preflight_instance.ensure_startable() {
        return crate::server::api::start_blocked_response(blocked);
    }
    if !preflight_instance.launch_is_finalized() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "creation_not_finalized",
                "message": "The structured session launch is not finalized",
            })),
        )
            .into_response();
    }
    if let Some(resp) = refuse_if_stored_row_shelved(&state, &preflight_instance).await {
        return resp;
    }
    let preflight_explicit = req
        .agent
        .clone()
        .or_else(|| preflight_instance.agent_name.clone());
    let preflight_agent =
        pick_agent(&state, &preflight_instance, preflight_explicit.as_deref()).await;
    if let Err(response) =
        validate_restart_agent(&state, &preflight_instance, &preflight_agent).await
    {
        return response;
    }
    if let Some(recovery) = req.recovery.as_ref() {
        match current_is_noop(&state, &preflight_instance, Some(recovery)).await {
            Ok(true) => {
                return Json(SpawnAcpResponse {
                    session_id: id,
                    agent: preflight_agent,
                    status: "running",
                })
                .into_response()
            }
            Ok(false) => {}
            Err(response) => return response,
        }
    } else if session_recovery.is_none() {
        if let Err(refusal) = preflight_instance.require_resolved_acp_selection() {
            return supervisor_error_response("restart refused", &refusal.into());
        }
    }

    let was_resuming =
        state.acp_supervisor.worker_state(&id).await == crate::daemon::AcpWorkerState::Resuming;
    if session_recovery.is_some() && was_resuming {
        return super::super::api_error(
            StatusCode::CONFLICT,
            "recovery_unavailable",
            "Native establishment is still in progress; observe its original requests",
        );
    }
    let stop_request = if session_recovery.is_none() {
        Some(
            state
                .acp_supervisor
                .begin_stop_request(&id, "user_stopped", None, true)
                .await,
        )
    } else {
        // Shared recovery admits its frozen selection before any cancellation.
        // Captured teardown below uses the existing non-cancelling retirement barrier.
        None
    };
    let mut establishment = None;
    // A running worker must finish its current submission before restart
    // tears it down. Hold only this barrier here; the full launch exclusion
    // also owns the instance lock and would block an external stop.
    let submission = if was_resuming {
        None
    } else {
        let Some(submission) = state
            .session_service
            .prompt_submission_for_session(&id)
            .await
        else {
            return session_not_found();
        };
        Some(submission)
    };
    // The stop request shares the installation gate, so a launch either gets
    // cancelled here or is observed as running and stopped after the barrier.
    if was_resuming {
        match retire_restart_worker(
            &state,
            &preflight_instance,
            &restart,
            req.recovery.clone(),
            session_recovery.clone(),
        )
        .await
        {
            Ok(attempt) => establishment = Some(attempt),
            Err(response) => return response,
        }
    }

    if restart.is_cancelled() {
        return supervisor_error_response(
            "restart cancelled by a stop request",
            &crate::acp::supervisor::SupervisorError::SpawnCancelled(id.clone()),
        );
    }
    let Some(instance) = find_instance(&state, &id).await else {
        return session_not_found();
    };
    if !instance.is_structured() {
        return not_structured_response();
    }
    if instance.terminal_handoff.is_some() {
        return (StatusCode::CONFLICT, "terminal handoff is pending").into_response();
    }
    if let Err(blocked) = instance.ensure_startable() {
        return crate::server::api::start_blocked_response(blocked);
    }
    if !instance.launch_is_finalized() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "creation_not_finalized",
                "message": "The structured session launch is not finalized",
            })),
        )
            .into_response();
    }
    let explicit = req.agent.clone().or_else(|| instance.agent_name.clone());
    let agent = pick_agent(&state, &instance, explicit.as_deref()).await;
    if instance.source_profile != preflight_instance.source_profile
        || instance.project_path != preflight_instance.project_path
        || agent != preflight_agent
    {
        return (
            StatusCode::CONFLICT,
            "session changed during restart preflight; retry the restart",
        )
            .into_response();
    }

    if let Some(resp) = refuse_if_stored_row_shelved(&state, &instance).await {
        return resp;
    }
    if restart.is_cancelled() {
        return supervisor_error_response(
            "restart cancelled by a stop request",
            &crate::acp::supervisor::SupervisorError::SpawnCancelled(id.clone()),
        );
    }
    // Running workers are stopped only after the submission barrier is held.
    if !was_resuming {
        match retire_restart_worker(
            &state,
            &instance,
            &restart,
            req.recovery.clone(),
            session_recovery.clone(),
        )
        .await
        {
            Ok(attempt) => establishment = Some(attempt),
            Err(response) => return response,
        }
    }
    if restart.is_cancelled() {
        return supervisor_error_response(
            "restart cancelled by a stop request",
            &crate::acp::supervisor::SupervisorError::SpawnCancelled(id.clone()),
        );
    }
    if state.acp_supervisor.worker_state(&id).await != crate::daemon::AcpWorkerState::Absent {
        return (
            StatusCode::CONFLICT,
            "worker_not_stopped: the previous worker has not finished stopping; retry the restart shortly",
        )
            .into_response();
    }
    drop(stop_request);
    drop(submission);

    // Acquire the launch exclusion only after the old worker has stopped. It
    // owns the submission mutex, so taking it before the stop barrier would
    // prevent an external stop from completing while restart waits here.
    let exclusion = tokio::select! {
        biased;
        _ = restart.cancellation().cancelled() => {
            return supervisor_error_response(
                "restart cancelled by a stop request",
                &crate::acp::supervisor::SupervisorError::SpawnCancelled(id.clone()),
            );
        }
        exclusion = crate::acp::sandbox::LaunchExclusion::acquire(state.clone(), &id, true) => {
            match exclusion {
                Ok(exclusion) => exclusion,
                Err(_) => return session_not_found(),
            }
        }
    };
    let Some(instance) = find_instance(&state, &id).await else {
        return session_not_found();
    };
    if !instance.is_structured() {
        return not_structured_response();
    }
    if instance.terminal_handoff.is_some() {
        return (StatusCode::CONFLICT, "terminal handoff is pending").into_response();
    }
    if let Err(blocked) = instance.ensure_startable() {
        return crate::server::api::start_blocked_response(blocked);
    }
    if !instance.launch_is_finalized() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "creation_not_finalized",
                "message": "The structured session launch is not finalized",
            })),
        )
            .into_response();
    }
    let explicit = req.agent.clone().or_else(|| instance.agent_name.clone());
    let agent = pick_agent(&state, &instance, explicit.as_deref()).await;
    if instance.source_profile != preflight_instance.source_profile
        || instance.project_path != preflight_instance.project_path
        || agent != preflight_agent
    {
        return (
            StatusCode::CONFLICT,
            "session changed during restart preflight; retry the restart",
        )
            .into_response();
    }
    let (instance, resume_reservation) =
        match crate::server::api::sessions::resume_structured_stop_with_restart(
            &state,
            instance,
            None,
            Some(&restart),
            establishment.clone(),
        )
        .await
        {
            Ok(prepared) => prepared,
            Err(response) => return response,
        };
    let establishment = resume_reservation
        .as_ref()
        .and_then(|reservation| reservation.establishment.clone())
        .or(establishment);
    if restart.is_cancelled() {
        return supervisor_error_response(
            "restart cancelled by a stop request",
            &crate::acp::supervisor::SupervisorError::SpawnCancelled(id.clone()),
        );
    }
    let (native, exclusion) = match launch_store_for(&state, &instance, exclusion).await {
        Ok(store) => store,
        Err(response) => return response,
    };
    let mut reservation = match resume_reservation {
        Some(reservation) => reservation,
        None => match state
            .acp_supervisor
            .reserve_spawn_with_restart(
                &id,
                if instance.acp_session_id.is_some() {
                    crate::acp::supervisor::ResumeKind::Spawn
                } else {
                    crate::acp::supervisor::ResumeKind::SpawnFirst
                },
                crate::process::worker_registry::ResumeAuthority::Explicit,
                Some(&restart),
            )
            .await
        {
            Ok(reservation) => reservation,
            Err(error) => return supervisor_error_response("restart admission failed", &error),
        },
    };
    let mut admission = crate::acp::supervisor::LaunchAdmission {
        establishment,
        store: native.clone(),
        generation: instance.lifecycle_generation,
        namespace: Some(state.profile_namespace.clone()),
        native_scope: None,
    };
    let instance = match exclusion
        .prepare_native_establishment(&state.acp_supervisor, &mut admission, &mut reservation)
        .await
    {
        Ok(row) => row,
        Err(error) => return supervisor_error_response("restart admission failed", &error),
    };
    let (sandbox_info, exclusion) = match crate::acp::sandbox::ensure_container_for_session(
        native.clone(),
        instance.clone(),
        exclusion,
    )
    .await
    {
        Ok(info) => info,
        Err(error) => return launch_error_response("sandbox container ensure", &error),
    };
    if restart.is_cancelled() {
        return supervisor_error_response(
            "restart cancelled by a stop request",
            &crate::acp::supervisor::SupervisorError::SpawnCancelled(id.clone()),
        );
    }
    let request = SpawnRequest {
        launch_admission: Some(admission),
        additional_dirs: req.additional_dirs,
        provider_env: req
            .provider_env
            .into_iter()
            .map(|p| (p.key, p.value))
            .collect(),
        model: req.model.or_else(|| instance.agent_model.clone()),
        ..spawn_request_for(&instance, agent.clone(), sandbox_info)
    };
    if let Some(resp) = refuse_if_stored_row_shelved(&state, &instance).await {
        return resp;
    }
    if restart.is_cancelled() {
        return supervisor_error_response(
            "restart cancelled by a stop request",
            &crate::acp::supervisor::SupervisorError::SpawnCancelled(id.clone()),
        );
    }
    drop(exclusion);
    let launched = state.acp_supervisor.spawn_inner(request, reservation).await;
    match launched {
        Ok(()) => Json(SpawnAcpResponse {
            session_id: id,
            agent,
            status: "running",
        })
        .into_response(),
        Err(e) => supervisor_error_response("restart spawn failed", &e),
    }
}

async fn current_is_noop(
    state: &Arc<AppState>,
    expected: &crate::session::Instance,
    recovery: Option<&crate::acp::establishment::EstablishmentRecovery>,
) -> Result<bool, Response> {
    use crate::acp::establishment::{
        AcpEstablishment, EstablishmentRecoveryAction, EstablishmentRefusal,
    };
    let mut current = state
        .acp_supervisor
        .current_native_availability(&expected.id)
        .await;
    let worker = state.acp_supervisor.worker_state(&expected.id).await;
    let owner = state.clone();
    let profile = expected.source_profile.clone();
    let id = expected.id.clone();
    let expected = expected.clone();
    let recovery = recovery.cloned();
    tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
        use crate::session::SessionStore as _;
        let native = crate::server::session_store::NativeSessionStore::open(
            owner,
            &profile,
            Some(id.clone()),
        )?;
        let _lifecycle = native.storage().acquire_instance_lifecycle_lock(&id)?;
        let row = native
            .load()?
            .into_iter()
            .find(|row| row.id == id)
            .ok_or(crate::session::SessionGone)?;
        anyhow::ensure!(
            crate::acp::state::NativeEventContext::capture(&expected, native.storage())?
                == crate::acp::state::NativeEventContext::capture(&row, native.storage())?
                && crate::acp::state::NativeBindingState::capture(&expected).matches(&row)
                && expected.acp_establishment == row.acp_establishment,
            EstablishmentRefusal::Superseded
        );
        if let Some(recovery) = &recovery {
            anyhow::ensure!(
                recovery.expected.matches(&row, native.storage())?,
                EstablishmentRefusal::Superseded
            );
        }
        if recovery
            .as_ref()
            .is_some_and(|recovery| recovery.action != EstablishmentRecoveryAction::Same)
            || !matches!(
                row.acp_establishment,
                Some(AcpEstablishment::Applied { .. })
            )
            || worker == crate::daemon::AcpWorkerState::Absent
        {
            return Ok(false);
        }
        let current = current.as_mut().ok_or(EstablishmentRefusal::Unconfirmed)?;
        let selected = current
            .scope
            .choice
            .as_ref()
            .is_some_and(|choice| choice.matches(&row));
        anyhow::ensure!(
            current.scope.context
                == crate::acp::state::NativeEventContext::capture(&row, native.storage())?
                && current.refresh(&id, selected).unwrap_or(false),
            EstablishmentRefusal::Unconfirmed
        );
        if current.unavailable.is_some() {
            return Ok(false);
        }
        let applied = current
            .client
            .establishment_projection()
            .and_then(|projection| projection.applied())
            .ok_or(EstablishmentRefusal::Unconfirmed)?;
        anyhow::ensure!(
            current.ready && applied.matches(&row, native.storage())?,
            EstablishmentRefusal::Unconfirmed
        );
        Ok(true)
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result)
    .map_err(|error| restart_retirement_error("conversation recovery refused", &error))
}

async fn retire_restart_worker(
    state: &Arc<AppState>,
    expected: &crate::session::Instance,
    restart: &crate::acp::supervisor::RestartRequestGuard,
    recovery: Option<crate::acp::establishment::EstablishmentRecovery>,
    session_recovery: Option<crate::session::SessionRecoveryRequest>,
) -> Result<Arc<crate::acp::establishment::EstablishmentAttempt>, Response> {
    if restart.is_cancelled() {
        return Err(supervisor_error_response(
            "restart cancelled",
            &SupervisorError::SpawnCancelled(expected.id.clone()),
        ));
    }
    let folded = if session_recovery.is_some() {
        Some(
            state
                .session_service
                .fold_control_history(&expected.id)
                .await
                .map_err(|error| restart_retirement_error("recovery observation failed", &error))?,
        )
    } else {
        None
    };
    let current = if session_recovery.is_some() {
        state
            .acp_supervisor
            .current_native_availability(&expected.id)
            .await
    } else {
        None
    };
    let owner = state.clone();
    let id = expected.id.clone();
    let prepare_id = id.clone();
    let profile = expected.source_profile.clone();
    let created_at = expected.created_at;
    let generation = expected.lifecycle_generation;
    let project_path = expected.project_path.clone();
    let tool = expected.tool.clone();
    let agent_name = expected.agent_name.clone();
    let agent_provider = expected.agent_provider.clone();
    let expected_choice = crate::acp::state::NativeBindingState::capture(expected);
    let expected_authority = expected.acp_establishment.clone();
    let prepared = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        use crate::session::SessionStore as _;
        let native = crate::server::session_store::NativeSessionStore::open(
            owner.clone(),
            &profile,
            Some(prepare_id.clone()),
        )?;
        let _lifecycle = native
            .storage()
            .acquire_instance_lifecycle_lock(&prepare_id)?;
        let row = native
            .load()?
            .into_iter()
            .find(|row| row.id == prepare_id)
            .ok_or(crate::session::SessionGone)?;
        row.ensure_startable()?;
        anyhow::ensure!(
            row.is_structured()
                && row.terminal_handoff.is_none()
                && row.created_at == created_at
                && row.lifecycle_generation == generation
                && row.source_profile == profile
                && row.project_path == project_path
                && row.tool == tool
                && row.agent_name == agent_name
                && row.agent_provider == agent_provider
                && expected_choice.matches(&row)
                && row.acp_establishment == expected_authority,
            crate::session::LifecycleReservationError::Superseded
        );
        let recovery = if let Some(request) = &session_recovery {
            use crate::acp::establishment::{
                EstablishmentExpected, EstablishmentRecovery, EstablishmentRecoveryAction,
                EstablishmentRefusal,
            };
            anyhow::ensure!(
                request.expected
                    == crate::session::projection::recovery_selection(&row, native.storage())?,
                EstablishmentRefusal::Superseded
            );
            let context = crate::acp::state::NativeEventContext::capture(&row, native.storage())?;
            let mut current = current;
            let current_valid = if let Some(current) = current.as_mut() {
                let selected = current
                    .scope
                    .choice
                    .as_ref()
                    .is_some_and(|choice| choice.matches(&row));
                current.scope.context == context
                    && current.refresh(&row.id, selected).unwrap_or(false)
                    && current.qualify_owner(&row.id).unwrap_or(false)
            } else {
                false
            };
            let folded = folded.as_ref().expect("shared recovery observation");
            let folded = owner
                .acp_control_cache
                .get_or_hydrate(&row.id, || Ok((folded.clone(), folded.last_seq)))?;
            let historical = folded.native_availability;
            let historical_current = historical
                .is_some_and(|(_, subject)| subject.unavailability_still_applies(&row.id));
            let availability = crate::acp::state::NativeAvailabilityProjection::resolve(
                historical,
                Some(context),
                current.as_ref().filter(|_| current_valid),
                historical_current,
                crate::process::worker_registry::control_admission(&row.id).err(),
            );
            let projected = crate::session::projection::current_recovery(
                &row,
                native.storage(),
                Some(&folded),
                availability,
                true,
            )?
            .ok_or(EstablishmentRefusal::Unconfirmed)?;
            let (admitted, action) = match request.action {
                crate::session::SessionRecoveryAction::Same => (
                    projected.actions.restart_same,
                    EstablishmentRecoveryAction::Same,
                ),
                crate::session::SessionRecoveryAction::Fresh => {
                    (projected.actions.fresh, EstablishmentRecoveryAction::Fresh)
                }
            };
            anyhow::ensure!(admitted, EstablishmentRefusal::Unconfirmed);
            Some(EstablishmentRecovery {
                expected: EstablishmentExpected::capture(&row, native.storage())?,
                action,
            })
        } else {
            recovery
        };
        let target = owner.acp_supervisor.capture_retirement(&row.id)?;
        if let Some(lease) = &target.lease {
            if let Some(scope) = lease
                .native_scope()
                .map_err(|_| anyhow::anyhow!("native retirement source is unavailable"))?
            {
                anyhow::ensure!(
                    scope.context
                        == crate::acp::state::NativeEventContext::capture(&row, native.storage())?,
                    crate::session::LifecycleReservationError::Superseded
                );
            } else {
                anyhow::ensure!(
                    target.identity.is_none(),
                    crate::session::LifecycleReservationError::Superseded
                );
            }
        }
        let mut binding = crate::server::acp_events::BindingRetirement::prepare_captured(
            &owner,
            target,
            &row,
            native.storage(),
        )?;
        let mut attempt = None;
        let mut retired_input = Vec::new();
        let store: &dyn crate::session::SessionStore = &native;
        store.update(|rows, _| {
            let row = rows
                .iter_mut()
                .find(|row| row.id == prepare_id)
                .ok_or(crate::session::SessionGone)?;
            if let Some(request) = &session_recovery {
                anyhow::ensure!(
                    request.expected
                        == crate::session::projection::recovery_selection(row, native.storage())?,
                    crate::acp::establishment::EstablishmentRefusal::Superseded
                );
            }
            attempt = Some(row.arm_acp_establishment(native.storage(), recovery.as_ref())?);
            if session_recovery.as_ref().is_some_and(|request| {
                request.action == crate::session::SessionRecoveryAction::Fresh
            }) {
                // Fresh is explicit abandonment, after selection and original retirement custody admission.
                // Never carry a blocked or possibly accepted AoE input into the replacement.
                row.pending_initial_turn = None;
                retired_input = std::mem::take(&mut row.queued_prompts);
            }
            Ok(())
        })?;
        for prompt in retired_input {
            owner
                .acp_event_store
                .delete_pending_attachments_for_ref(&prepare_id, &prompt.id);
        }
        let attempt = attempt
            .ok_or_else(|| anyhow::anyhow!("native establishment mutation was not applied"))?;
        let row = native
            .load()?
            .into_iter()
            .find(|row| row.id == prepare_id)
            .ok_or(crate::session::SessionGone)?;
        binding.promote(&row, native.storage())?;
        owner
            .acp_supervisor
            .register_delegated_binding_retirement(&binding.target, binding.checkpoint.clone())?;
        Ok((native, binding, Arc::new(attempt)))
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result);
    let (native, binding, attempt) = prepared.map_err(|error| {
        restart_retirement_error("restart native retirement preparation", &error)
    })?;
    state
        .acp_supervisor
        .shutdown_and_wait_for_restart(
            &id,
            &binding.target,
            std::time::Duration::from_secs(5),
            restart,
        )
        .await
        .map_err(|error| supervisor_error_response("restart shutdown failed", &error))?;
    let owner = state.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        use crate::session::SessionStore as _;
        let _lifecycle = native.storage().acquire_instance_lifecycle_lock(&id)?;
        let delegated = || {
            binding
                .target
                .lease
                .as_ref()
                .is_none_or(|lease| lease.retirement().binding_checkpoint_delegated())
        };
        anyhow::ensure!(delegated(), SupervisorError::TeardownPending(id.clone()));
        let row = native
            .load()?
            .into_iter()
            .find(|row| row.id == id)
            .ok_or(crate::session::SessionGone)?;
        anyhow::ensure!(
            row.terminal_handoff.is_none(),
            crate::session::LifecycleReservationError::Superseded
        );
        binding.commit_under_native_lock(&native)?;
        native
            .load()?
            .into_iter()
            .find(|row| row.id == id)
            .ok_or(crate::session::SessionGone)?;
        anyhow::ensure!(delegated(), SupervisorError::TeardownPending(id.clone()));
        owner
            .acp_supervisor
            .acknowledge_retirement(&binding.target)?;
        Ok(())
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result)
    .map_err(|error| restart_retirement_error("restart native retirement checkpoint", &error))?;
    Ok(attempt)
}

fn restart_retirement_error(context: &str, error: &anyhow::Error) -> Response {
    if let Some(error) = error.downcast_ref::<SupervisorError>() {
        return supervisor_error_response(context, error);
    }
    if let Some(refusal) = error.downcast_ref::<crate::acp::establishment::EstablishmentRefusal>() {
        return supervisor_error_response(context, &(*refusal).into());
    }
    if error.is::<crate::session::SessionCommitApplied>() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("{context}: applied/unconfirmed: {error}"),
        )
            .into_response();
    }
    launch_error_response(context, error)
}

async fn validate_restart_agent(
    state: &AppState,
    instance: &crate::session::Instance,
    agent: &str,
) -> Result<(), Response> {
    if !state
        .acp_supervisor
        .agent_is_valid_switch_target(
            agent,
            &instance.source_profile,
            std::path::Path::new(&instance.project_path),
        )
        .await
    {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("unknown structured view agent: {agent}"),
        )
            .into_response());
    }
    if !super::super::agent_policy().await.allows(agent) {
        return Err((
            StatusCode::FORBIDDEN,
            SupervisorError::AgentNotAllowed(agent.to_string()).to_string(),
        )
            .into_response());
    }
    Ok(())
}

pub async fn shutdown_acp(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    if let Some(resp) = cityhall_block(&state) {
        return resp;
    }
    // Runtime-owned work retains stop admission and retirement after HTTP cancellation.
    let work = state.runtime.work.clone();
    match work
        .run("server.acp.user_stop", async move {
            let Some(cached) = find_instance(&state, &id).await else {
                return session_not_found();
            };
            let persisted = match crate::server::api::load_persisted_instance(
                &state,
                &cached.source_profile,
                &id,
            )
            .await
            {
                Ok(Some(persisted)) => persisted,
                Ok(None) => return session_not_found(),
                Err(response) => return response,
            };
            if !persisted.is_structured() {
                return not_structured_response();
            }
            let _namespace = state.profile_namespace.read().await;
            if *state.canonical_health.read().await != crate::daemon::RuntimeHealth::Healthy {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            let stop_request = state
                .acp_supervisor
                .begin_stop_request(&id, "user_stopped", None, false)
                .await;
            let cancelled_target = stop_request.cancelled_target();
            state.acp_supervisor.cancel_restart_request(&id).await;
            let Some(_submission) = state
                .session_service
                .prompt_submission_for_session(&id)
                .await
            else {
                return if cancelled_target {
                    StatusCode::NO_CONTENT.into_response()
                } else {
                    session_not_found()
                };
            };
            let lock = state.instance_lock(&id).await;
            let _guard = lock.lock().await;
            let Some(instance) = find_instance(&state, &id).await else {
                return if cancelled_target {
                    StatusCode::NO_CONTENT.into_response()
                } else {
                    session_not_found()
                };
            };
            if !instance.is_structured() {
                return not_structured_response();
            }
            match crate::server::api::sessions::stop_structured_with_commit(
                &state,
                instance.source_profile,
                &id,
            )
            .await
            {
                Ok(()) => {
                    tracing::info!(
                        target: "session.stop",
                        session = %id,
                        view = "structured",
                        "session stopped"
                    );
                    StatusCode::NO_CONTENT.into_response()
                }
                Err(response) if cancelled_target && response.status() == StatusCode::NOT_FOUND => {
                    StatusCode::NO_CONTENT.into_response()
                }
                Err(response) => response,
            }
        })
        .await
    {
        Ok(response) => response,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

/// One `GET /api/acp/agents` entry; `name` is a valid switch-agent target.
#[derive(Debug, Serialize)]
pub struct AcpAgentInfo {
    pub name: String,
    pub description: String,
    pub command: String,
    #[serde(skip_serializing_if = "crate::agents::AgentLifecycle::is_active")]
    pub lifecycle: crate::agents::AgentLifecycle,
}

/// Built-in ACP registry entries the operator policy permits.
pub async fn list_acp_agents(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let registry = state.acp_supervisor.registry_snapshot().await;
    let policy = super::super::agent_policy().await;
    Json(acp_agent_entries(&registry, &policy)).into_response()
}

fn acp_agent_entries(
    registry: &crate::acp::AgentRegistry,
    policy: &crate::acp::agent_policy::AgentPolicy,
) -> Vec<AcpAgentInfo> {
    let mut entries: Vec<AcpAgentInfo> = registry
        .list()
        .into_iter()
        .filter(|(name, _)| policy.allows(name))
        .map(|(name, spec)| AcpAgentInfo {
            name: name.clone(),
            description: spec.description.clone(),
            command: spec.command.clone(),
            lifecycle: crate::agents::registry_lifecycle(name),
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}

/// `GET /api/acp/option-catalog`: config options each agent last advertised.
pub async fn get_option_catalog() -> impl IntoResponse {
    let catalog = tokio::task::spawn_blocking(crate::acp::option_catalog::load)
        .await
        .unwrap_or_default();
    Json(catalog).into_response()
}

/// Validate a switch target before the current worker is torn down, returning
/// the agent the session is switching away from.
async fn check_switch_target(
    state: &AppState,
    instance: &crate::session::Instance,
    target: &str,
) -> Result<String, Response> {
    if !state
        .acp_supervisor
        .agent_is_valid_switch_target(
            target,
            &instance.source_profile,
            std::path::Path::new(&instance.project_path),
        )
        .await
    {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("unknown structured view agent: {target}"),
        )
            .into_response());
    }
    if !super::super::agent_policy().await.allows(target) {
        return Err((
            StatusCode::FORBIDDEN,
            SupervisorError::AgentNotAllowed(target.to_string()).to_string(),
        )
            .into_response());
    }
    let from_agent = pick_agent(state, instance, instance.agent_name.as_deref()).await;
    if from_agent == target {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("session is already using {target}"),
        )
            .into_response());
    }
    Ok(from_agent)
}

async fn complete_agent_switch(
    state: &Arc<AppState>,
    store: Arc<crate::server::session_store::NativeSessionStore>,
    id: &str,
    target: &str,
    model: Option<&str>,
    launch_epoch: u64,
) -> anyhow::Result<crate::acp::sandbox::LaunchExclusion> {
    let exclusion = crate::acp::sandbox::LaunchExclusion::acquire(state.clone(), id, true).await?;
    anyhow::ensure!(
        state
            .acp_supervisor
            .is_current_launch(id, launch_epoch)
            .await,
        crate::session::LifecycleReservationError::Superseded
    );
    persist_agent_switch(store, id, target, model, exclusion).await
}

async fn persist_agent_switch(
    store: Arc<crate::server::session_store::NativeSessionStore>,
    id: &str,
    target: &str,
    model: Option<&str>,
    exclusion: crate::acp::sandbox::LaunchExclusion,
) -> anyhow::Result<crate::acp::sandbox::LaunchExclusion> {
    use crate::session::SessionStore;
    let id = id.to_owned();
    let target = target.to_owned();
    let model = model.map(str::to_owned);
    tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        store.commit(&mut |rows, _| {
            let instance = rows
                .iter_mut()
                .find(|row| row.id == id)
                .ok_or(crate::session::SessionGone)?;
            if instance.agent_name.as_deref() != Some(target.as_str()) {
                instance.acp_session_id = None;
                instance.import_pending = None;
            }
            instance.agent_name = Some(target.clone());
            instance.acp_effort = None;
            instance.agent_model = model.clone();
            instance.terminal_rebind_pending = true;
            Ok(())
        })?;
        Ok(exclusion)
    })
    .await?
}

/// An explicit backend handoff consents to a new destination conversation,
/// never to resuming the previous backend's native session under another agent.
async fn prepare_agent_switch(
    store: Arc<crate::server::session_store::NativeSessionStore>,
    expected: crate::session::Instance,
    target: String,
    model: Option<String>,
) -> anyhow::Result<Arc<crate::acp::establishment::EstablishmentAttempt>> {
    use crate::acp::establishment::{
        EstablishmentExpected, EstablishmentRecovery, EstablishmentRecoveryAction,
    };
    use crate::session::SessionStore;
    tokio::task::spawn_blocking(move || {
        let _lifecycle = store
            .storage()
            .acquire_instance_lifecycle_lock(&expected.id)?;
        let source = crate::acp::state::NativeBindingState::capture(&expected);
        let context = crate::acp::state::NativeEventContext::capture(&expected, store.storage())?;
        let mut prepared = None;
        store.commit(&mut |rows, _| {
            let row = rows
                .iter_mut()
                .find(|row| row.id == expected.id)
                .ok_or(crate::session::SessionGone)?;
            row.ensure_startable()?;
            anyhow::ensure!(
                row.is_structured()
                    && row.launch_is_finalized()
                    && row.terminal_handoff.is_none()
                    && source.matches(row)
                    && row.acp_establishment == expected.acp_establishment
                    && context
                        == crate::acp::state::NativeEventContext::capture(row, store.storage())?,
                crate::session::LifecycleReservationError::Superseded
            );
            row.agent_name = Some(target.clone());
            row.agent_model = model.clone();
            row.acp_effort = None;
            row.acp_load_session_capable = None;
            row.terminal_rebind_pending = true;
            let recovery = EstablishmentRecovery {
                expected: EstablishmentExpected::capture(row, store.storage())?,
                action: EstablishmentRecoveryAction::Fresh,
            };
            prepared = Some(row.arm_acp_establishment(store.storage(), Some(&recovery))?);
            Ok(())
        })?;
        prepared
            .map(Arc::new)
            .ok_or_else(|| anyhow::anyhow!("backend switch establishment was not committed"))
    })
    .await?
}

/// Move a structured session to another ACP backend, keeping the transcript.
/// `before_seq` lets the client's context primer exclude the handoff event.
pub async fn switch_acp_agent(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Result<Json<SwitchAgentRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    if let Some(resp) = cityhall_block(&state) {
        return resp;
    }
    let Json(req) = match req {
        Ok(req) => req,
        Err(error) => return error.into_response(),
    };
    let target = req.target.trim().to_string();
    if target.is_empty() {
        return (StatusCode::BAD_REQUEST, "target is required").into_response();
    }
    let _switch_request = match state.acp_supervisor.begin_switch_request(&id) {
        Ok(request) => request,
        Err(error) => return supervisor_error_response("agent switch failed", &error),
    };
    let exclusion =
        match crate::acp::sandbox::LaunchExclusion::acquire(state.clone(), &id, true).await {
            Ok(exclusion) => exclusion,
            Err(_) => return session_not_found(),
        };
    // Custom agents are profile-specific, so the instance is needed to validate.
    let Some(instance) = find_instance(&state, &id).await else {
        return session_not_found();
    };
    if instance.terminal_handoff.is_some() {
        return (StatusCode::CONFLICT, "terminal handoff is pending").into_response();
    }
    if let Err(blocked) = instance.ensure_startable() {
        return crate::server::api::start_blocked_response(blocked);
    }
    if let Err(refusal) = instance.require_resolved_acp_selection() {
        return supervisor_error_response("agent switch refused", &refusal.into());
    }
    let from_agent = match check_switch_target(&state, &instance, &target).await {
        Ok(agent) => agent,
        Err(resp) => return resp,
    };
    let (instance, resume_reservation) =
        match crate::server::api::sessions::resume_structured_stop_deferred(
            &state,
            instance,
            Some(crate::acp::supervisor::ResumeKind::SpawnFirst),
        )
        .await
        {
            Ok(instance) => instance,
            Err(response) => return response,
        };
    let (native, exclusion) = match launch_store_for(&state, &instance, exclusion).await {
        Ok(store) => store,
        Err(response) => return response,
    };
    let before_seq = state.acp_event_store.highest_seq(&id);

    if resume_reservation.is_none() {
        if let Err(e) = async {
            let target = state.acp_supervisor.capture_retirement(&id)?;
            crate::server::acp_events::retire_native_worker(
                &state,
                &id,
                target,
                std::time::Duration::from_secs(5),
            )
            .await
        }
        .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("shutdown failed before agent switch: {e}"),
            )
                .into_response();
        }
    }
    let mut reservation = if let Some(reservation) = resume_reservation {
        reservation
    } else {
        match state
            .acp_supervisor
            .reserve_spawn(
                &id,
                crate::acp::supervisor::ResumeKind::SpawnFirst,
                crate::process::worker_registry::ResumeAuthority::Explicit,
            )
            .await
        {
            Ok(reservation) => reservation,
            Err(e) => return supervisor_error_response("spawn failed", &e),
        }
    };
    let Some(instance) = find_instance(&state, &id).await.filter(|row| {
        row.created_at == instance.created_at
            && row.lifecycle_generation == instance.lifecycle_generation
            && row.source_profile == instance.source_profile
            && row.agent_name == instance.agent_name
            && row.agent_provider == instance.agent_provider
            && row.project_path == instance.project_path
            && crate::acp::state::NativeBindingState::capture(&instance).matches(row)
            && row.acp_establishment == instance.acp_establishment
    }) else {
        return supervisor_error_response(
            "agent switch source changed",
            &SupervisorError::Superseded(id),
        );
    };
    let model = req.model.clone();
    let attempt = match prepare_agent_switch(
        native.clone(),
        instance.clone(),
        target.clone(),
        model.clone(),
    )
    .await
    {
        Ok(attempt) => attempt,
        Err(error) => return restart_retirement_error("agent switch destination", &error),
    };
    let mut admission = crate::acp::supervisor::LaunchAdmission {
        establishment: Some(attempt),
        store: native.clone(),
        generation: instance.lifecycle_generation,
        namespace: Some(state.profile_namespace.clone()),
        native_scope: None,
    };
    let instance = match exclusion
        .prepare_native_establishment(&state.acp_supervisor, &mut admission, &mut reservation)
        .await
    {
        Ok(row) => row,
        Err(error) => return supervisor_error_response("agent switch admission failed", &error),
    };
    let (sandbox_info, exclusion) = match crate::acp::sandbox::ensure_container_for_session(
        native.clone(),
        instance.clone(),
        exclusion,
    )
    .await
    {
        Ok(info) => info,
        Err(error) => return launch_error_response("sandbox container ensure", &error),
    };
    let request = SpawnRequest {
        launch_admission: Some(admission),
        sandbox_continuation: crate::acp::supervisor::SandboxContinuation::Fresh,
        ..spawn_request_for(&instance, target.clone(), sandbox_info)
    };
    if let Some(response) = refuse_if_stored_row_shelved(&state, &instance).await {
        return response;
    }
    let launch_lease = reservation.lease().clone();
    let launch_epoch = launch_lease.epoch();
    drop(exclusion);
    if let Err(e) = state.acp_supervisor.spawn_inner(request, reservation).await {
        return supervisor_error_response("spawn failed", &e);
    }
    let _exclusion =
        match complete_agent_switch(&state, native, &id, &target, model.as_deref(), launch_epoch)
            .await
        {
            Ok(exclusion) => exclusion,
            Err(error) if is_store_write_failure(&error) => {
                return persist_failure_response(
                    &state,
                    &id,
                    &launch_lease,
                    "agent switch",
                    &error,
                )
                .await;
            }
            Err(error) => return launch_error_response("agent switch completion", &error),
        };
    state
        .telemetry_structured
        .agent_switches
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let reason = req
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .unwrap_or("manual")
        .to_string();
    tracing::info!(
        target: "acp.switch",
        session = %id,
        from = %from_agent,
        to = %target,
        reason = %reason,
        "agent switched"
    );
    let switch_seq =
        state
            .acp_supervisor
            .publish_agent_switched(&id, from_agent, target.clone(), reason);

    Json(SwitchAgentResponse {
        session_id: id,
        agent: target,
        before_seq,
        switch_seq,
        status: "running".to_string(),
    })
    .into_response()
}

/// The adapter's always-present model entry, which the CLI resolves to the
/// running provider's own default.
const PROVIDER_DEFAULT_MODEL: &str = "default";

/// Pin the provider default explicitly so a loaded transcript cannot restore
/// a model id that belongs to the previous provider.
async fn persist_provider_switch(
    store: Arc<crate::server::session_store::NativeSessionStore>,
    expected: &crate::session::Instance,
    provider: &str,
    exclusion: crate::acp::sandbox::LaunchExclusion,
) -> anyhow::Result<(
    crate::acp::sandbox::LaunchExclusion,
    Arc<crate::acp::establishment::EstablishmentAttempt>,
)> {
    use crate::session::SessionStore;
    let expected = expected.clone();
    let provider = provider.to_owned();
    tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let _lifecycle = store
            .storage()
            .acquire_instance_lifecycle_lock(&expected.id)?;
        let source = crate::acp::state::NativeBindingState::capture(&expected);
        let mut prepared = None;
        store.commit(&mut |rows, _| {
            let row = rows
                .iter_mut()
                .find(|row| row.id == expected.id)
                .ok_or(crate::session::LifecycleReservationError::Superseded)?;
            anyhow::ensure!(
                row.lifecycle_generation == expected.lifecycle_generation
                    && row.title == expected.title
                    && row.agent_provider == expected.agent_provider
                    && source.matches(row)
                    && row.acp_establishment == expected.acp_establishment
                    && row.is_structured()
                    && row.launch_is_finalized()
                    && row.terminal_handoff.is_none(),
                crate::session::LifecycleReservationError::Superseded
            );
            row.ensure_startable()?;
            row.agent_provider = Some(provider.clone());
            row.agent_model = Some(PROVIDER_DEFAULT_MODEL.to_owned());
            prepared = Some(row.arm_acp_establishment(store.storage(), None)?);
            Ok(())
        })?;
        let attempt = prepared
            .ok_or_else(|| anyhow::anyhow!("provider switch establishment was not committed"))?;
        Ok((exclusion, Arc::new(attempt)))
    })
    .await?
}

/// Re-route a structured session to another LLM provider, keeping the
/// transcript: the worker stops between turns and the respawn resumes the
/// stored ACP session.
///
/// Unlike an agent switch the pick is persisted before the respawn, because
/// both the sandbox container reconcile and the spawn request read it from the
/// row. A failed spawn therefore leaves the pick in place, which is what lets
/// the user see the error and switch back rather than silently landing on the
/// old provider.
pub async fn switch_acp_provider(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Result<Json<SwitchProviderRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    if let Some(resp) = cityhall_block(&state) {
        return resp;
    }
    let Json(req) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };
    let provider = req.provider.trim().to_string();
    if !crate::session::environment::AGENT_PROVIDERS.contains(&provider.as_str()) {
        return super::super::api_error(
            StatusCode::BAD_REQUEST,
            "unknown_provider",
            format!(
                "unknown provider {provider:?}; expected one of {}",
                crate::session::environment::AGENT_PROVIDERS.join(", ")
            ),
        );
    }
    let _switch_request = match state.acp_supervisor.begin_switch_request(&id) {
        Ok(request) => request,
        Err(error) => return supervisor_error_response("provider switch failed", &error),
    };
    let exclusion =
        match crate::acp::sandbox::LaunchExclusion::acquire(state.clone(), &id, true).await {
            Ok(exclusion) => exclusion,
            Err(_) => return session_not_found(),
        };
    // Held from the first read through the respawn, as `spawn_acp` takes it:
    // between the shutdown and the persisted pick the reconciler would
    // otherwise resume the worker off the old row, and win.

    let Some(instance) = find_instance(&state, &id).await else {
        return session_not_found();
    };
    if !instance.is_structured() {
        return not_structured_response();
    }
    if instance.terminal_handoff.is_some() {
        return (StatusCode::CONFLICT, "terminal handoff is pending").into_response();
    }
    if let Err(blocked) = instance.ensure_startable() {
        return crate::server::api::start_blocked_response(blocked);
    }
    if !instance.launch_is_finalized() {
        return super::super::api_error(
            StatusCode::CONFLICT,
            "creation_not_finalized",
            "The structured session launch is not finalized",
        );
    }
    let agent = pick_agent(&state, &instance, instance.agent_name.as_deref()).await;
    // The routing flags are Claude-specific; no other adapter reads them.
    if !matches!(agent.as_str(), "claude" | "claude-code") {
        return super::super::api_error(
            StatusCode::CONFLICT,
            "provider_switch_unsupported",
            format!("provider switching is Claude-only; this session runs {agent}"),
        );
    }
    if instance.agent_provider.as_deref() == Some(provider.as_str()) {
        return super::super::api_error(
            StatusCode::BAD_REQUEST,
            "provider_unchanged",
            format!("session is already pinned to {provider}"),
        );
    }
    if let Err(error) = instance.require_resolved_acp_selection() {
        return supervisor_error_response("provider switch refused", &error.into());
    }
    // The submission guard keeps new prompts out but does not wait for the
    // running one, and the shutdown below aborts it.
    let control = match state.session_service.fold_control_state(&id).await {
        Ok(control) => control,
        Err(_) => return super::prompt::control_state_unavailable(),
    };
    if control.turn_active || control.has_active_background_agent() {
        return super::super::api_error(
            StatusCode::CONFLICT,
            "turn_active",
            "the session is mid-turn; switch providers once it finishes",
        );
    }
    let (instance, resume_reservation) =
        match crate::server::api::sessions::resume_structured_stop_deferred(&state, instance, None)
            .await
        {
            Ok(instance) => instance,
            Err(response) => return response,
        };
    let (native, exclusion) = match launch_store_for(&state, &instance, exclusion).await {
        Ok(store) => store,
        Err(response) => return response,
    };

    if resume_reservation.is_none() {
        if let Err(e) = async {
            let target = state.acp_supervisor.capture_retirement(&id)?;
            crate::server::acp_events::retire_native_worker(
                &state,
                &id,
                target,
                std::time::Duration::from_secs(5),
            )
            .await
        }
        .await
        {
            if matches!(
                e.downcast_ref::<SupervisorError>(),
                Some(SupervisorError::TeardownPending(_))
            ) {
                return super::super::api_error(
                    StatusCode::CONFLICT,
                    "worker_not_stopped",
                    "the previous worker has not finished stopping; retry the switch shortly",
                );
            }
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("shutdown failed before provider switch: {e}"),
            )
                .into_response();
        }
    }
    // No provider mutation until the previous runner is proven absent.
    if resume_reservation.is_none()
        && state.acp_supervisor.worker_state(&id).await != crate::daemon::AcpWorkerState::Absent
    {
        return super::super::api_error(
            StatusCode::CONFLICT,
            "worker_not_stopped",
            "the previous worker has not finished stopping; retry the switch shortly",
        );
    }

    let model_cleared = instance
        .agent_model
        .as_deref()
        .is_some_and(|model| model != PROVIDER_DEFAULT_MODEL);
    let (exclusion, attempt) =
        match persist_provider_switch(native.clone(), &instance, &provider, exclusion).await {
            Ok(prepared) => prepared,
            Err(error) => {
                if is_store_write_failure(&error) {
                    state.acp_supervisor.request_respawn(&id);
                }
                return launch_error_response("persisting the provider switch", &error);
            }
        };
    drop(native);

    let Some(updated) = find_instance(&state, &id).await.filter(|row| {
        row.lifecycle_generation == instance.lifecycle_generation
            && row.source_profile == instance.source_profile
    }) else {
        return session_not_found();
    };
    let (native, exclusion) = match launch_store_for(&state, &updated, exclusion).await {
        Ok(store) => store,
        Err(response) => return response,
    };
    let mut reservation = if let Some(reservation) = resume_reservation {
        reservation
    } else {
        match state
            .acp_supervisor
            .reserve_spawn(
                &id,
                if updated.acp_session_id.is_some() {
                    crate::acp::supervisor::ResumeKind::Spawn
                } else {
                    crate::acp::supervisor::ResumeKind::SpawnFirst
                },
                crate::process::worker_registry::ResumeAuthority::Explicit,
            )
            .await
        {
            Ok(reservation) => reservation,
            Err(e) => return supervisor_error_response("spawn failed after provider switch", &e),
        }
    };
    let mut admission = crate::acp::supervisor::LaunchAdmission {
        establishment: Some(attempt),
        store: native.clone(),
        generation: updated.lifecycle_generation,
        namespace: Some(state.profile_namespace.clone()),
        native_scope: None,
    };
    let updated = match exclusion
        .prepare_native_establishment(&state.acp_supervisor, &mut admission, &mut reservation)
        .await
    {
        Ok(row) => row,
        Err(error) => return supervisor_error_response("provider switch admission failed", &error),
    };
    let (sandbox_info, exclusion) =
        match crate::acp::sandbox::ensure_container_for_session(native, updated.clone(), exclusion)
            .await
        {
            Ok(info) => info,
            Err(e) => return launch_error_response("sandbox container ensure", &e),
        };
    let request = SpawnRequest {
        launch_admission: Some(admission),
        ..spawn_request_for(&updated, agent, sandbox_info)
    };
    if let Some(resp) = refuse_if_stored_row_shelved(&state, &updated).await {
        return resp;
    }
    drop(exclusion);
    if let Err(e) = state.acp_supervisor.spawn_inner(request, reservation).await {
        return supervisor_error_response("spawn failed after provider switch", &e);
    }
    tracing::info!(
        target: "acp.switch",
        session = %id,
        provider = %provider,
        model_cleared,
        "provider switched"
    );

    Json(SwitchProviderResponse {
        session_id: id,
        provider,
        model_cleared,
        status: "running".to_string(),
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::agent_policy::AgentPolicy;
    use crate::acp::state::RateLimitInfo;
    use crate::session::SessionStore as _;

    async fn durable_stop_state(row: crate::session::Instance) -> Arc<AppState> {
        crate::server::test_support::seed_instances_on_disk_for_test("default", vec![row.clone()]);
        let state = crate::server::test_support::build_test_app_state(vec![row]);
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        state
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn spawn_hook_can_reenter_rename_and_settles_after_cancellation() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::{mpsc, Notify};

        let _home = crate::session::test_support::isolate_app_dir();
        let project = tempfile::tempdir().unwrap();
        let mut row = crate::session::Instance::new("Before", project.path().to_str().unwrap());
        row.source_profile = "default".into();
        row.view = crate::session::View::Structured;
        row.resume_intent = crate::session::ResumeIntent::Use("retained-reentrant-hook".into());
        let id = row.id.clone();
        crate::server::test_support::seed_instances_on_disk_for_test("default", vec![row.clone()]);
        let calls = Arc::new(AtomicUsize::new(0));
        let launcher = {
            let calls = Arc::clone(&calls);
            Arc::new(move |_, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {
                    Err(crate::acp::acp_client::AcpError::Spawn(
                        "unexpected launch".into(),
                    ))
                }) as _
            })
        };
        let state =
            crate::server::test_support::build_test_app_state_with_launcher(vec![row], launcher);
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (renamed_tx, mut renamed_rx) = mpsc::unbounded_channel();
        let release = Arc::new(Notify::new());
        let app = axum::Router::new().route(
            "/hook",
            axum::routing::get({
                let state = Arc::clone(&state);
                let id = id.clone();
                let release = Arc::clone(&release);
                move || {
                    let state = Arc::clone(&state);
                    let id = id.clone();
                    let release = Arc::clone(&release);
                    let renamed_tx = renamed_tx.clone();
                    async move {
                        let response = crate::server::api::sessions::rename_session(
                            State(state),
                            Path(id),
                            Ok(Json(crate::daemon::RenameSessionBody {
                                title: Some("After".into()),
                                group: None,
                                profile: None,
                                rename_branch: false,
                            })),
                        )
                        .await
                        .into_response();
                        renamed_tx.send(response.status()).unwrap();
                        release.notified().await;
                        StatusCode::OK
                    }
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let config = crate::session::get_app_dir().unwrap().join("config.toml");
        std::fs::write(
            config,
            format!("[host_hooks]\nbefore_session = \"curl --max-time 10 -fsS http://{address}/hook\"\n"),
        )
        .unwrap();
        let mut spawning = {
            let state = Arc::clone(&state);
            let id = id.clone();
            tokio::spawn(async move {
                spawn_acp(
                    State(state),
                    Path(id),
                    Ok(Json(SpawnAcpRequest {
                        recovery: None,
                        agent: None,
                        model: None,
                        additional_dirs: Vec::new(),
                        provider_env: Vec::new(),
                    })),
                )
                .await
                .into_response()
            })
        };
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), renamed_rx.recv())
            .await
            .expect("hook's rename must finish without waiting for launch exclusion")
            .unwrap();
        assert_eq!(status, StatusCode::OK);
        let stop = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            state
                .acp_supervisor
                .begin_stop_request(&id, "user_stopped", None, false),
        )
        .await
        .expect("stop request must acquire its fence while the hook is active");
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut spawning)
                .await
                .is_err(),
            "cancellation must not settle while the hook remains blocked"
        );
        release.notify_one();
        let response = tokio::time::timeout(std::time::Duration::from_secs(5), spawning)
            .await
            .expect("cancelled launch must settle")
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            state.acp_supervisor.worker_state(&id).await,
            crate::daemon::AcpWorkerState::Absent
        );
        drop(stop);
        server.abort();
    }

    async fn assert_reconstructed_stop_does_not_resume(row: crate::session::Instance) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let starts = Arc::new(AtomicUsize::new(0));
        let calls = starts.clone();
        let state = crate::server::test_support::build_test_app_state_with_launcher(
            vec![row.clone()],
            Arc::new(move |_, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {
                    Err(crate::acp::acp_client::AcpError::Spawn(
                        "unexpected automatic launch".into(),
                    ))
                })
            }),
        );
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        crate::server::acp_reconciler::reconcile_acp_workers(
            &state,
            &mut std::collections::HashSet::new(),
            &mut crate::server::acp_reconciler::ReapCadence::default(),
            &mut std::collections::HashMap::new(),
            &mut std::collections::HashSet::new(),
            &mut std::collections::HashMap::new(),
        )
        .await;
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        assert!(!state.acp_supervisor.is_owned(&row.id).await);
    }

    #[cfg(unix)]
    async fn live_stop_runner(row: &crate::session::Instance) -> tokio::process::Child {
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let record = crate::process::worker_registry::WorkerRecord::new(
            row.id.clone(),
            child.id().unwrap(),
            crate::process::worker_registry::socket_path_for(&row.id).unwrap(),
            "sleep".into(),
            "claude".into(),
            PathBuf::from(&row.project_path),
            None,
            Vec::new(),
            Vec::new(),
            None,
            Some(row.source_profile.clone()),
        )
        .with_generation(71);
        assert!(record.runner_identity.is_some());
        crate::process::worker_registry::save(&record).unwrap();
        child
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn rest_user_stop_retires_active_and_already_stopped_native_rows_durably() {
        use crate::session::{Instance, Status, Storage, View};
        for status in [Status::Running, Status::Stopped] {
            let _home = crate::session::test_support::isolate_app_dir();
            let project = tempfile::tempdir().unwrap();
            let mut row = Instance::new("durable DELETE", project.path().to_str().unwrap());
            row.source_profile = "default".into();
            row.view = View::Structured;
            row.status = status;
            row.resume_intent = crate::session::ResumeIntent::Use("retained-durable-stop".into());
            let state = durable_stop_state(row.clone()).await;
            let mut runner = live_stop_runner(&row).await;
            let record = crate::process::worker_registry::load_strict(&row.id)
                .unwrap()
                .unwrap();
            state
                .acp_supervisor
                .test_install_attached(
                    &row.id,
                    crate::acp::runner_lifecycle::RunnerIdentity::from_record(&record),
                )
                .await;
            let response = shutdown_acp(State(state.clone()), Path(row.id.clone()))
                .await
                .into_response();
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
            let exit = tokio::time::timeout(std::time::Duration::from_secs(5), runner.wait())
                .await
                .unwrap()
                .unwrap();
            assert!(!exit.success());
            assert!(crate::process::worker_registry::load(&row.id)
                .unwrap()
                .is_none());
            let stopped = Storage::open_unwatched("default")
                .unwrap()
                .load()
                .unwrap()
                .remove(0);
            assert_eq!(stopped.status, Status::Stopped);
            assert!(stopped.is_idle_dormant());
            assert!(stopped.lifecycle_generation > row.lifecycle_generation);
            assert!(stopped.lifecycle_reservation.is_none());
            assert_reconstructed_stop_does_not_resume(stopped.clone()).await;

            let (resumed, reservation) =
                crate::server::api::sessions::resume_structured_stop(&state, stopped, None)
                    .await
                    .unwrap_or_else(|response| panic!("resume failed: {}", response.status()));
            assert_eq!(resumed.status, Status::Idle);
            assert!(!resumed.is_idle_dormant());
            let admission = crate::acp::supervisor::LaunchAdmission {
                establishment: reservation
                    .as_ref()
                    .and_then(|reservation| reservation.establishment.clone()),
                store: Arc::new(Storage::open_unwatched("default").unwrap()),
                generation: resumed.lifecycle_generation,
                namespace: None,
                native_scope: None,
            };
            let mut child = crate::acp::supervisor::with_durable_launch(
                Some(admission),
                row.id.clone(),
                || {
                    std::process::Command::new("true")
                        .spawn()
                        .map_err(|error| crate::acp::acp_client::AcpError::Spawn(error.to_string()))
                },
            )
            .await
            .unwrap();
            assert!(child.wait().unwrap().success());
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn late_assigned_frame_preserves_acknowledged_stop_and_selected_store() {
        use crate::acp::runner_lifecycle::ProcessControl as _;
        use crate::acp::supervisor::test_support::managed::with_managed;
        use crate::server::test_support as support;
        use crate::session::{Status, Storage};
        use std::time::Duration;
        for retire in [true, false] {
            with_managed(|scope| Box::pin(async move {
                let mut publications = scope.state.acp_events_tx.subscribe();
                let session = support::managed_control_session(scope, "late-assignment-stop").await;
                let state = session.state.clone();
                let id = session.id().to_owned();
                let original = session.client.lease.spawned_runner().unwrap();
                let frame = tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let frame = publications.recv().await.unwrap();
                        if frame.session_id == id && matches!(frame.event.as_ref(),
                            crate::acp::Event::AcpSessionAssigned { connection_ready: true, native_subject: Some(_), .. })
                        {
                            break frame;
                        }
                    }
                }).await.expect("retain the actual original's certified assignment frame");
                let before = find_instance(&state, &id).await.unwrap();
                let binding = before.agent_session_binding.clone().expect("actual observed store binding");
                let gate = crate::server::acp_events::pause_assignment_listener_for_test(&id, frame.seq);
                let baseline = state.acp_events_tx.receiver_count();
                let listener_state = state.clone();
                let listener = scope.spawn_helper(async move {
                    crate::server::acp_events::acp_event_listener(listener_state).await;
                });
                tokio::time::timeout(Duration::from_secs(5), async {
                    while state.acp_events_tx.receiver_count() <= baseline {
                        tokio::task::yield_now().await;
                    }
                }).await.expect("the real listener subscribes before original frame delivery");
                state.acp_events_tx.send(frame).unwrap();
                tokio::time::timeout(Duration::from_secs(5), gate.arrived).await.unwrap().unwrap();
                let storage = Storage::open_unwatched("test").unwrap();
                let expected_status = if retire {
                    let response = shutdown_acp(State(state.clone()), Path(id.clone())).await.into_response();
                    assert_eq!(response.status(), StatusCode::NO_CONTENT);
                    assert!(!crate::acp::runner_lifecycle::SystemProcessControl::default().is_alive(original));
                    Status::Stopped
                } else {
                    let _lifecycle = storage.acquire_instance_lifecycle_lock(&id).unwrap();
                    storage.update(|rows, _| {
                        rows.iter_mut().find(|row| row.id == id).unwrap().mark_idle_dormant();
                        Ok(())
                    }).unwrap();
                    state.instances.write().await.iter_mut().find(|row| row.id == id).unwrap().mark_idle_dormant();
                    support::refresh_canonical_metadata_for_test(&state).await;
                    assert!(crate::acp::runner_lifecycle::SystemProcessControl::default().is_alive(original));
                    before.status
                };
                let stopped = storage.load().unwrap().into_iter().find(|row| row.id == id).unwrap();
                assert_eq!(stopped.status, expected_status);
                assert!(stopped.is_idle_dormant());
                gate.release.send(()).unwrap();
                tokio::time::timeout(Duration::from_secs(5), gate.applied).await.unwrap().unwrap();
                listener.abort();
                for row in [
                    find_instance(&state, &id).await.unwrap(),
                    storage.load().unwrap().into_iter().find(|row| row.id == id).unwrap(),
                ] {
                    assert_eq!(row.status, expected_status, "Original assignment is not wake authority");
                    assert!(row.is_idle_dormant(), "late Native assignment must not consume the durable park");
                    assert_eq!(row.lifecycle_generation, stopped.lifecycle_generation);
                    assert_eq!(row.acp_session_id.as_deref(), Some(session.sid.as_str()));
                    assert_eq!(row.agent_session_binding.as_ref(), Some(&binding));
                    assert_eq!(row.agent_session_binding.as_ref().unwrap().execution.as_ref().unwrap().exported_default_store,
                        binding.execution.as_ref().unwrap().exported_default_store);
                }
                let sid = session.sid.clone();
                let (config, native) = scope.native_peer(
                    PathBuf::from(&before.project_path), "claude", "claude",
                    move |request| Some(match request["method"].as_str().unwrap() {
                        "initialize" => serde_json::json!({"jsonrpc":"2.0","id":request["id"],
                            "result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}}),
                        "session/load" => {
                            assert_eq!(request["params"]["sessionId"], sid);
                            serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":{}})
                        },
                        other => panic!("manual resume must retain the actual selected Native: {other}"),
                    }),
                );
                let replacement = scope.resume_stopped(&id, config).await;
                replacement.ready().await;
                assert_ne!(replacement.lease.epoch(), session.client.lease.epoch());
                assert_ne!(replacement.lease.spawned_runner(), Some(original));
                assert_eq!(native.requests().iter().filter(|method| *method == "session/load").count(), 1);
                assert!(!native.requests().iter().any(|method| method == "session/new"));
                let resumed = find_instance(&state, &id).await.unwrap();
                assert!(!resumed.is_idle_dormant());
                assert!(resumed.lifecycle_generation > stopped.lifecycle_generation);
                assert_eq!(resumed.acp_session_id.as_deref(), Some(session.sid.as_str()));
                assert_eq!(resumed.agent_session_binding.as_ref(), Some(&binding));
            })).await;
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn rest_user_stop_fences_prebirth_launch_before_http_cancellation() {
        use crate::session::{Instance, LifecycleOperation, Status, Storage, View};
        for cancel_http in [false, true] {
            let _home = crate::session::test_support::isolate_app_dir();
            let project = tempfile::tempdir().unwrap();
            let mut row = Instance::new("prebirth DELETE", project.path().to_str().unwrap());
            row.source_profile = "default".into();
            row.view = View::Structured;
            row.status = Status::Running;
            row.resume_intent = crate::session::ResumeIntent::Use("retained-prebirth-stop".into());
            let old_generation = row
                .try_acquire_lifecycle_reservation(
                    LifecycleOperation::Launch,
                    Instance::LIFECYCLE_RESERVATION_TTL,
                    Utc::now(),
                )
                .unwrap();
            let state = durable_stop_state(row.clone()).await;
            let reservation = state
                .acp_supervisor
                .reserve_spawn(
                    &row.id,
                    crate::acp::supervisor::ResumeKind::SpawnFirst,
                    crate::process::worker_registry::ResumeAuthority::Automatic,
                )
                .await
                .unwrap();
            let mut changes = state.status_tx.subscribe();
            let request_state = state.clone();
            let id = row.id.clone();
            let request = tokio::spawn(async move {
                shutdown_acp(State(request_state), Path(id))
                    .await
                    .into_response()
            });
            let change = tokio::time::timeout(std::time::Duration::from_secs(5), changes.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(change.instance_id, row.id);
            assert_eq!(change.new, Status::Stopped);
            let fenced = Storage::open_unwatched("default")
                .unwrap()
                .load()
                .unwrap()
                .remove(0);
            assert!(fenced.lifecycle_generation > old_generation);
            assert!(fenced.lifecycle_reservation_is_owned(
                LifecycleOperation::Stop,
                fenced.lifecycle_generation
            ));
            let request = if cancel_http {
                request.abort();
                assert!(request.await.unwrap_err().is_cancelled());
                None
            } else {
                Some(request)
            };

            let admission = crate::acp::supervisor::LaunchAdmission {
                establishment: None,
                store: Arc::new(Storage::open_unwatched("default").unwrap()),
                generation: old_generation,
                namespace: None,
                native_scope: None,
            };
            let late_birth = crate::acp::supervisor::with_durable_launch(
                Some(admission),
                row.id.clone(),
                || {
                    std::process::Command::new("true")
                        .spawn()
                        .map_err(|error| crate::acp::acp_client::AcpError::Spawn(error.to_string()))
                },
            )
            .await;
            assert!(matches!(
                late_birth,
                Err(SupervisorError::SpawnCancelled(_))
            ));
            drop(reservation);
            state.acp_supervisor.retry_pending_teardowns().await;
            if let Some(request) = request {
                assert_eq!(request.await.unwrap().status(), StatusCode::NO_CONTENT);
            }
            state.runtime.work.drain().await;
            let stopped = Storage::open_unwatched("default")
                .unwrap()
                .load()
                .unwrap()
                .remove(0);
            assert_eq!(stopped.status, Status::Stopped);
            assert!(stopped.is_idle_dormant());
            assert!(stopped.lifecycle_reservation.is_none());
            assert_reconstructed_stop_does_not_resume(stopped).await;
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn restart_commits_original_binding_before_releasing_its_admission() {
        use crate::acp::runner_lifecycle::ProcessControl as _;
        use crate::acp::supervisor::test_support::managed::with_managed;
        use crate::acp::Event;
        use crate::session::Storage;
        with_managed(|scope| Box::pin(async move {
                let project = scope.tempdir();
                let native_store = project.path().join("claude-native-store");
                std::fs::create_dir(&native_store).unwrap();
                scope.save_environment(&["CLAUDE_CONFIG_DIR"]);
                std::env::set_var("CLAUDE_CONFIG_DIR", &native_store);
                let (config, native) = scope.native_peer(project.path().into(), "claude", "claude", |request| {
                    let id = request.get("id")?;
                    Some(match request["method"].as_str().unwrap() {
                        "initialize" => serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":1,"agentCapabilities":{}}}),
                        "session/new" => serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":"original-native-restart"}}),
                        other => panic!("retirement cannot issue another native session operation: {other}"),
                    })
                });
                let mut client = scope.spawn(config, "restart-original-binding").await.unwrap();
                client.ready().await;
                let id = client.id.clone();
                let state = scope.state.clone();
                tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    loop {
                        let event = client.next_event().await.unwrap();
                        if matches!(&event, Event::AcpSessionAssigned {
                            acp_session_id, connection_ready: true, native_subject: Some(_),
                        } if acp_session_id == "original-native-restart") { break; }
                        assert!(!matches!(event, Event::AgentStartupError { .. }));
                    }
                }).await.unwrap();
                let original = state.acp_supervisor.current_native_availability(&id).await.unwrap();
                let owner = original.owner.expect("the real original owner");
                assert_eq!(Some(owner), client.lease.spawned_runner());
                assert_ne!(owner.pid, std::process::id());
                assert_eq!(state.instances.read().await.iter().find(|row| row.id == id).unwrap().acp_session_id.as_deref(),
                    Some("original-native-restart"),
                    "the certified original binding is published by its establishment CAS");
                let storage = Storage::open_unwatched("test").unwrap();
                let row = storage.load().unwrap().into_iter().find(|row| row.id == id).unwrap();
                assert!(matches!(row.acp_establishment, Some(crate::acp::establishment::AcpEstablishment::Applied { .. })),
                    "retirement starts from a real certified original, not a mock Ready");
                let snapshot = || state.acp_event_store.replay_from(&id, 0).expect("read retained events").into_iter()
                    .map(|stored| (stored.seq, stored.metadata.scope as i64,
                        serde_json::to_value(&stored.metadata).unwrap(), stored.event.native_subject(),
                        serde_json::to_value(stored.event.as_ref()).unwrap()))
                    .collect::<Vec<_>>();
                let history = snapshot();
                let restart = state.acp_supervisor.try_begin_restart_with_reason(&id).await.unwrap();
                // This is an ordinary retained Applied restart, not a Fresh or
                // unresolved-attempt recovery request.
                if let Err(response) = retire_restart_worker(&state, &row, &restart, None, None).await {
                    let status = response.status();
                    let body = axum::body::to_bytes(response.into_body(), 4096).await.unwrap();
                    panic!("restart retirement failed: {status}: {}", String::from_utf8_lossy(&body));
                }
                assert!(!crate::acp::runner_lifecycle::SystemProcessControl::default().is_alive(owner));
                let memory = state.instances.read().await;
                assert_eq!(memory.iter().find(|row| row.id == id).unwrap().acp_session_id.as_deref(),
                    Some("original-native-restart"));
                drop(memory);
                let stored = storage.load().unwrap().into_iter().find(|row| row.id == id).unwrap();
                assert_eq!(stored.acp_session_id.as_deref(), Some("original-native-restart"));
                assert!(snapshot().starts_with(&history));
                let reservation = state.acp_supervisor.reserve_spawn_with_restart(
                    &id, crate::acp::supervisor::ResumeKind::Spawn,
                    crate::process::worker_registry::ResumeAuthority::Explicit, Some(&restart),
                ).await.expect("the foreground checkpoint ACK releases actual replacement admission");
                assert!(state.acp_supervisor.current_lease(&id) == Some(reservation.lease().clone()));
                assert!(reservation.lease() != &original.lease);
                assert!(!restart.is_cancelled());
                assert_eq!(native.requests().iter().filter(|method| *method == "session/new").count(), 1);
                drop(reservation);
            })).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn explicit_resume_retirement_preserves_its_request_until_an_external_stop() {
        use crate::session::{Instance, Status, Storage, View};
        use futures_util::FutureExt as _;

        let _home = crate::session::test_support::isolate_app_dir();
        let project = tempfile::tempdir().unwrap();
        let mut row = Instance::new("stop then resume", project.path().to_str().unwrap());
        row.source_profile = "default".into();
        row.view = View::Structured;
        row.resume_intent = crate::session::ResumeIntent::Use("retained-explicit-resume".into());
        let state = durable_stop_state(row.clone()).await;
        assert_eq!(
            shutdown_acp(State(state.clone()), Path(row.id.clone()))
                .await
                .into_response()
                .status(),
            StatusCode::NO_CONTENT
        );
        let stopped = Storage::open_unwatched("default")
            .unwrap()
            .load()
            .unwrap()
            .remove(0);
        assert_eq!(stopped.status, Status::Stopped);

        // The HTTP spawn registers before acquiring exclusion and retiring
        // the old stopped instance. That retirement is part of this request.
        let request = state.acp_supervisor.begin_spawn_request(&row.id).unwrap();
        let restart = state
            .acp_supervisor
            .try_begin_restart_with_reason(&row.id)
            .await
            .unwrap();
        let exclusion = crate::acp::sandbox::LaunchExclusion::acquire(state.clone(), &row.id, true)
            .await
            .unwrap();
        let (resumed, reservation) =
            crate::server::api::sessions::resume_structured_stop_with_restart(
                &state,
                stopped,
                None,
                Some(&restart),
                None,
            )
            .await
            .unwrap_or_else(|response| panic!("resume failed: {}", response.status()));
        let reservation = reservation.expect("explicit stopped resume reserves once");
        let lease = reservation.lease().clone();
        let mut reservation = Some(reservation);
        let mut stopping = None;
        drop(exclusion);
        let checked = std::panic::AssertUnwindSafe(async {
            assert!(state.acp_supervisor.current_lease(&row.id) == Some(lease.clone()));
            assert_eq!(resumed.status, Status::Idle);
            let mut changes = state.status_tx.subscribe();
            assert!(!request.cancellation().is_cancelled());
            assert!(!restart.is_cancelled());

            // Keep the producer until Stop's exact native capture has committed.
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
            let owner = state.clone();
            let id = row.id.clone();
            stopping = Some(tokio::spawn(async move {
                shutdown_acp(State(owner), Path(id)).await.into_response()
            }));
            tokio::time::timeout_at(deadline, lease.cancellation().cancelled())
                .await
                .expect("external Stop cancels the original resume reservation");
            let change = tokio::time::timeout_at(deadline, changes.recv())
                .await
                .expect("external Stop commits its native capture before producer completion")
                .expect("external Stop publishes its committed status");
            assert_eq!(change.instance_id, row.id);
            assert_eq!(change.effective_profile, resumed.source_profile);
            assert_eq!(change.old, Status::Idle);
            assert_eq!(change.new, Status::Stopped);
            assert!(reservation.as_ref().unwrap().cancellation().is_cancelled());
            drop(reservation.take());
        })
        .catch_unwind()
        .await;

        // Complete owned cleanup before assertions can unwind the isolated HOME.
        drop(reservation.take());
        state.acp_supervisor.retry_pending_teardowns().await;
        let stopped = match stopping.take() {
            Some(task) => Some(task.await),
            None => None,
        };
        state.runtime.work.drain().await;
        state.shutdown.cancel();
        if let Err(panic) = checked {
            std::panic::resume_unwind(panic);
        }
        assert_eq!(
            stopped
                .expect("the external Stop was started")
                .expect("the external Stop task completed")
                .status(),
            StatusCode::NO_CONTENT
        );
        assert!(state.acp_supervisor.current_lease(&row.id).is_none());
        assert!(request.cancellation().is_cancelled());
        assert!(restart.is_cancelled());
        assert!(lease.cancellation().is_cancelled());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn explicit_resume_cannot_consume_a_newer_native_stop() {
        use crate::session::{Instance, Status, Storage, View};
        let _home = crate::session::test_support::isolate_app_dir();
        let project = tempfile::tempdir().unwrap();
        let mut row = Instance::new("newer stop", project.path().to_str().unwrap());
        row.source_profile = "default".into();
        row.view = View::Structured;
        row.status = Status::Running;
        let state = durable_stop_state(row.clone()).await;
        assert_eq!(
            shutdown_acp(State(state.clone()), Path(row.id.clone()))
                .await
                .into_response()
                .status(),
            StatusCode::NO_CONTENT
        );
        let observed = Storage::open_unwatched("default")
            .unwrap()
            .load()
            .unwrap()
            .remove(0);
        assert_eq!(
            shutdown_acp(State(state.clone()), Path(row.id.clone()))
                .await
                .into_response()
                .status(),
            StatusCode::NO_CONTENT
        );
        let newer = Storage::open_unwatched("default")
            .unwrap()
            .load()
            .unwrap()
            .remove(0);
        assert!(newer.lifecycle_generation > observed.lifecycle_generation);
        let response = crate::server::api::sessions::resume_structured_stop(&state, observed, None)
            .await
            .expect_err("stale explicit resume must retain newer stop");
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let retained = Storage::open_unwatched("default")
            .unwrap()
            .load()
            .unwrap()
            .remove(0);
        assert_eq!(retained.lifecycle_generation, newer.lifecycle_generation);
        assert_eq!(retained.status, Status::Stopped);
        assert!(retained.is_idle_dormant());
        let stop_path = crate::process::worker_registry::workers_dir()
            .unwrap()
            .join(format!("{}.stop", row.id));
        std::fs::write(&stop_path, b"undecodable stop intent").unwrap();
        let response = crate::server::api::sessions::resume_structured_stop(&state, newer, None)
            .await
            .expect_err("undecodable intent cannot authorize resume");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            std::fs::read(stop_path).unwrap(),
            b"undecodable stop intent"
        );
        let retained = Storage::open_unwatched("default")
            .unwrap()
            .load()
            .unwrap()
            .remove(0);
        assert_eq!(retained.status, Status::Stopped);
        assert!(retained.is_idle_dormant());
    }

    async fn assert_capacity_refusal_preserves_native_stop(prompt: bool) {
        use crate::process::worker_registry;
        use crate::session::{Instance, Status, Storage, View};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let _home = crate::session::test_support::isolate_app_dir();
        let project = tempfile::tempdir().unwrap();
        let mut row = Instance::new("stopped at capacity", project.path().to_str().unwrap());
        row.source_profile = "default".into();
        row.view = View::Structured;
        row.tool = "claude".into();
        row.agent_name = Some("claude".into());
        row.acp_session_id = Some("retained-native-conversation".into());
        row.status = Status::Stopped;
        row.lifecycle_generation = 19;
        row.mark_idle_dormant();
        crate::server::test_support::seed_instances_on_disk_for_test("default", vec![row.clone()]);
        let births = Arc::new(AtomicUsize::new(0));
        let attempted = births.clone();
        let state = crate::server::test_support::build_test_app_state_with_launcher(
            vec![row.clone()],
            Arc::new(move |_, _| {
                attempted.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {
                    Err(crate::acp::acp_client::AcpError::Spawn(
                        "capacity refusal must precede construction".into(),
                    ))
                })
            }),
        );
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        state
            .acp_supervisor
            .test_insert_worker("occupied-slot")
            .await;
        assert_eq!(
            state.acp_supervisor.worker_state("occupied-slot").await,
            crate::daemon::AcpWorkerState::Running
        );
        let record = worker_registry::WorkerRecord::new(
            row.id.clone(),
            std::process::id(),
            worker_registry::socket_path_for(&row.id).unwrap(),
            "fixture".into(),
            "claude".into(),
            project.path().into(),
            None,
            Vec::new(),
            Vec::new(),
            row.acp_session_id.clone(),
            Some("default".into()),
        )
        .with_generation(71);
        worker_registry::save(&record).unwrap();
        let intent = worker_registry::request_stop(&record).unwrap();
        worker_registry::delete(&row.id).unwrap();
        assert!(worker_registry::load_strict(&row.id).unwrap().is_none());
        assert_eq!(
            worker_registry::stop_intent(&row.id).unwrap(),
            Some(intent.clone())
        );
        let before = Storage::open_unwatched("default")
            .unwrap()
            .load()
            .unwrap()
            .remove(0);
        let response = if prompt {
            crate::server::api::acp_prompt(
                State(state.clone()),
                Path(row.id.clone()),
                Ok(Json(
                    serde_json::from_value(serde_json::json!({
                        "text":"explicit wake at capacity", "prompt_id":"capacity-prompt",
                    }))
                    .unwrap(),
                )),
            )
            .await
            .into_response()
        } else {
            spawn_acp(
                State(state.clone()),
                Path(row.id.clone()),
                Ok(Json(
                    serde_json::from_value(serde_json::json!({
                        "agent":"claude", "additional_dirs":[], "provider_env":[],
                    }))
                    .unwrap(),
                )),
            )
            .await
            .into_response()
        };
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(births.load(Ordering::SeqCst), 0);
        let after = Storage::open_unwatched("default")
            .unwrap()
            .load()
            .unwrap()
            .remove(0);
        let memory = find_instance(&state, &row.id).await.unwrap();
        assert_eq!(
            worker_registry::stop_intent(&row.id).unwrap(),
            Some(intent),
            "capacity refusal cannot consume the explicit stop token"
        );
        for retained in [&after, &memory] {
            assert_eq!(retained.status, Status::Stopped);
            assert!(retained.is_idle_dormant());
            assert_eq!(retained.lifecycle_generation, before.lifecycle_generation);
            assert_eq!(retained.lifecycle_reservation, before.lifecycle_reservation);
            assert_eq!(retained.acp_session_id, before.acp_session_id);
        }
        assert!(worker_registry::load_strict(&row.id).unwrap().is_none());
        assert_eq!(
            state.acp_supervisor.worker_state(&row.id).await,
            crate::daemon::AcpWorkerState::Absent
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn capacity_refusal_preserves_http_spawn_stop_fence_and_native_generation() {
        assert_capacity_refusal_preserves_native_stop(false).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn capacity_refusal_preserves_http_prompt_stop_fence_and_native_generation() {
        assert_capacity_refusal_preserves_native_stop(true).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn rest_stop_rejects_wrong_view_missing_and_uncommitted_targets() {
        use crate::session::{Instance, Status, Storage};
        let _home = crate::session::test_support::isolate_app_dir();
        let project = tempfile::tempdir().unwrap();
        let mut row = Instance::new("terminal DELETE", project.path().to_str().unwrap());
        row.source_profile = "default".into();
        row.status = Status::Running;
        let state = durable_stop_state(row.clone()).await;
        assert_eq!(
            shutdown_acp(State(state.clone()), Path(row.id.clone()))
                .await
                .into_response()
                .status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            shutdown_acp(State(state.clone()), Path("missing-stop-target".into()))
                .await
                .into_response()
                .status(),
            StatusCode::NOT_FOUND
        );
        let retained = Storage::open_unwatched("default")
            .unwrap()
            .load()
            .unwrap()
            .remove(0);
        assert_eq!(retained.status, Status::Running);
        assert_eq!(retained.lifecycle_generation, row.lifecycle_generation);
        assert!(!retained.is_idle_dormant());
        state.instances.write().await[0].view = crate::session::View::Structured;
        let reservation = state
            .acp_supervisor
            .reserve_spawn(
                &row.id,
                crate::acp::supervisor::ResumeKind::SpawnFirst,
                crate::process::worker_registry::ResumeAuthority::Automatic,
            )
            .await
            .unwrap();
        Storage::open_unwatched("default")
            .unwrap()
            .update(|rows, _| {
                rows.clear();
                Ok(())
            })
            .unwrap();
        assert_eq!(
            shutdown_acp(State(state.clone()), Path(row.id.clone()))
                .await
                .into_response()
                .status(),
            StatusCode::NOT_FOUND
        );
        assert!(!state
            .acp_supervisor
            .test_cancel_requested(reservation.lease()));
        drop(reservation);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn reconstructed_internal_stopped_history_resumes_but_native_user_stop_does_not() {
        use crate::session::{Instance, Status, Storage, View};
        use std::sync::atomic::{AtomicUsize, Ordering};
        for native_status in [Status::Running, Status::Idle, Status::Stopped] {
            let _home = crate::session::test_support::isolate_app_dir();
            let project = tempfile::tempdir().unwrap();
            let mut row = Instance::new("reconstructed shutdown", project.path().to_str().unwrap());
            row.source_profile = "default".into();
            row.view = View::Structured;
            row.status = native_status;
            row.lifecycle_generation = 9;
            row.tool = "claude".into();
            row.agent_name = Some("claude".into());
            row.resume_intent =
                crate::session::ResumeIntent::Use("retained-reconstructed-stop".into());
            if native_status == Status::Stopped {
                row.mark_idle_dormant();
            }
            crate::server::test_support::seed_instances_on_disk_for_test(
                "default",
                vec![row.clone()],
            );
            let births = Arc::new(AtomicUsize::new(0));
            let calls = births.clone();
            let state = crate::server::test_support::build_test_app_state_with_launcher(
                vec![row.clone()],
                Arc::new(move |_, _| {
                    let calls = calls.clone();
                    Box::pin(async move {
                        assert!(tokio::process::Command::new("true")
                            .status()
                            .await
                            .unwrap()
                            .success());
                        calls.fetch_add(1, Ordering::SeqCst);
                        Err(crate::acp::acp_client::AcpError::Spawn(
                            "controlled post-admission launch failure".into(),
                        ))
                    })
                }),
            );
            crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
            state
                .acp_event_store
                .record(
                    &row.id,
                    1,
                    &crate::acp::Event::Stopped {
                        reason: "user_stopped".into(),
                    },
                    &crate::acp::event_sender::EventMetadata::history(),
                )
                .unwrap();
            crate::server::acp_events::seed_acp_statuses(state.clone()).await;
            assert_eq!(
                Storage::open_unwatched("default").unwrap().load().unwrap()[0].status,
                native_status
            );
            crate::server::acp_reconciler::reconcile_acp_workers(
                &state,
                &mut std::collections::HashSet::new(),
                &mut crate::server::acp_reconciler::ReapCadence::default(),
                &mut std::collections::HashMap::new(),
                &mut std::collections::HashSet::new(),
                &mut std::collections::HashMap::new(),
            )
            .await;
            assert_eq!(
                births.load(Ordering::SeqCst),
                usize::from(native_status != Status::Stopped),
                "{native_status:?}"
            );
            if native_status == Status::Stopped {
                let admission = crate::acp::supervisor::LaunchAdmission {
                    establishment: None,
                    store: Arc::new(Storage::open_unwatched("default").unwrap()),
                    generation: row.lifecycle_generation,
                    namespace: None,
                    native_scope: None,
                };
                let rejected = crate::acp::supervisor::with_durable_launch(
                    Some(admission),
                    row.id.clone(),
                    || {
                        std::process::Command::new("true").spawn().map_err(|error| {
                            crate::acp::acp_client::AcpError::Spawn(error.to_string())
                        })
                    },
                )
                .await;
                assert!(matches!(rejected, Err(SupervisorError::SpawnCancelled(_))));
                let retained = Storage::open_unwatched("default")
                    .unwrap()
                    .load()
                    .unwrap()
                    .remove(0);
                assert_eq!(retained.status, Status::Stopped);
                assert!(retained.is_idle_dormant());
                assert_eq!(retained.lifecycle_generation, row.lifecycle_generation);
            }
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn explicit_backend_and_provider_switches_resume_native_stop_before_birth() {
        use crate::session::{Instance, LifecycleOperation, Status, Storage, View};
        use std::sync::atomic::{AtomicUsize, Ordering};
        for case in [
            "backend",
            "provider",
            "invalid_backend",
            "invalid_provider",
            "unselected_backend",
            "unselected_provider",
        ] {
            let _home = crate::session::test_support::isolate_app_dir();
            let project = tempfile::tempdir().unwrap();
            let mut row = Instance::new(case, project.path().to_str().unwrap());
            row.source_profile = "default".into();
            row.view = View::Structured;
            row.status = Status::Stopped;
            row.tool = "claude".into();
            row.agent_name = Some("claude".into());
            row.agent_provider = Some("anthropic".into());
            if !case.starts_with("unselected") {
                crate::server::test_support::select_retained_for_test(&mut row);
            }
            row.mark_idle_dormant();
            let observed = row
                .try_acquire_lifecycle_reservation(
                    LifecycleOperation::Stop,
                    Instance::LIFECYCLE_RESERVATION_TTL,
                    Utc::now(),
                )
                .unwrap();
            crate::server::test_support::seed_instances_on_disk_for_test(
                "default",
                vec![row.clone()],
            );
            let births = Arc::new(AtomicUsize::new(0));
            let calls = births.clone();
            let state = crate::server::test_support::build_test_app_state_with_launcher(
                vec![row.clone()],
                Arc::new(move |_, _| {
                    let calls = calls.clone();
                    Box::pin(async move {
                        let status = tokio::process::Command::new("true").status().await.unwrap();
                        assert!(status.success());
                        calls.fetch_add(1, Ordering::SeqCst);
                        Err(crate::acp::acp_client::AcpError::Spawn(
                            "controlled post-admission launch failure".into(),
                        ))
                    })
                }),
            );
            crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
            let response = if case.contains("backend") {
                switch_acp_agent(
                    State(state.clone()),
                    Path(row.id.clone()),
                    Ok(Json(SwitchAgentRequest {
                        target: if !case.starts_with("invalid") {
                            "codex".into()
                        } else {
                            "not-a-registered-agent".into()
                        },
                        model: None,
                        reason: None,
                    })),
                )
                .await
                .into_response()
            } else {
                let provider = if !case.starts_with("invalid") {
                    crate::session::environment::AGENT_PROVIDERS
                        .iter()
                        .find(|provider| **provider != "anthropic")
                        .unwrap()
                        .to_string()
                } else {
                    "not-a-provider".into()
                };
                switch_acp_provider(
                    State(state.clone()),
                    Path(row.id.clone()),
                    Ok(Json(SwitchProviderRequest { provider })),
                )
                .await
                .into_response()
            };
            assert!(!response.status().is_success());
            let stored = Storage::open_unwatched("default")
                .unwrap()
                .load()
                .unwrap()
                .remove(0);
            if case.starts_with("invalid") || case.starts_with("unselected") {
                assert_eq!(births.load(Ordering::SeqCst), 0);
                assert_eq!(stored.lifecycle_generation, observed);
                assert_eq!(stored.status, Status::Stopped);
                assert!(stored.is_idle_dormant());
                assert_eq!(
                    serde_json::to_value(&stored).unwrap(),
                    serde_json::to_value(&row).unwrap(),
                    "{case}: refusal preserves the exact stopped source and selection"
                );
            } else {
                assert_eq!(
                    births.load(Ordering::SeqCst),
                    1,
                    "{case}: {}",
                    response.status()
                );
                assert!(stored.lifecycle_generation > observed);
                assert_ne!(stored.status, Status::Stopped);
                assert!(!stored.is_idle_dormant());
            }
        }
    }

    #[tokio::test]
    async fn a_concurrent_restart_request_returns_conflict() {
        let state = crate::server::test_support::build_test_app_state(Vec::new());
        let id = "restart-in-flight".to_string();
        let first = state
            .acp_supervisor
            .try_begin_restart_with_reason(&id)
            .await
            .expect("the first restart claims the session");

        let response = restart_acp(
            State(Arc::clone(&state)),
            Path(id.clone()),
            Ok(Json(SpawnAcpRequest {
                recovery: None,
                agent: None,
                model: None,
                additional_dirs: Vec::new(),
                provider_env: Vec::new(),
            })),
        )
        .await
        .into_response();

        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).starts_with("restart_in_progress:"));

        drop(first);
        assert!(state
            .acp_supervisor
            .try_begin_restart_with_reason(&id)
            .await
            .is_ok());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn restart_claim_rejects_spawn_and_switches_without_mutating_session() {
        use crate::session::{Instance, Storage, View};

        let _home = crate::session::test_support::isolate_app_dir();
        let project = tempfile::tempdir().unwrap();
        let mut row = Instance::new("restart claim", project.path().to_str().unwrap());
        row.source_profile = "default".into();
        row.view = View::Structured;
        row.agent_name = Some("claude".into());
        row.agent_provider = Some("anthropic".into());
        let id = row.id.clone();
        crate::server::test_support::seed_instances_on_disk_for_test("default", vec![row.clone()]);
        let state = crate::server::test_support::build_test_app_state(vec![row]);
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let claim = state
            .acp_supervisor
            .try_begin_restart_with_reason(&id)
            .await
            .unwrap();
        let provider = crate::session::environment::AGENT_PROVIDERS
            .iter()
            .find(|provider| **provider != "anthropic")
            .unwrap()
            .to_string();
        let spawn = spawn_acp(
            State(state.clone()),
            Path(id.clone()),
            Ok(Json(SpawnAcpRequest {
                recovery: None,
                agent: None,
                model: None,
                additional_dirs: Vec::new(),
                provider_env: Vec::new(),
            })),
        )
        .await
        .into_response();
        let switch = switch_acp_provider(
            State(state.clone()),
            Path(id.clone()),
            Ok(Json(SwitchProviderRequest { provider })),
        )
        .await
        .into_response();
        let agent = switch_acp_agent(
            State(state.clone()),
            Path(id.clone()),
            Ok(Json(SwitchAgentRequest {
                target: "codex".into(),
                model: None,
                reason: None,
            })),
        )
        .await
        .into_response();
        assert_eq!(spawn.status(), StatusCode::CONFLICT);
        assert_eq!(switch.status(), StatusCode::CONFLICT);
        assert_eq!(agent.status(), StatusCode::CONFLICT);
        let stored = Storage::open_unwatched("default")
            .unwrap()
            .load()
            .unwrap()
            .remove(0);
        assert_eq!(stored.agent_provider.as_deref(), Some("anthropic"));
        assert_eq!(stored.agent_name.as_deref(), Some("claude"));
        assert_eq!(
            state.acp_supervisor.worker_state(&id).await,
            crate::daemon::AcpWorkerState::Absent
        );
        drop(claim);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn restart_cancels_a_spawn_while_it_holds_submission_and_instance_locks() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let _home = crate::session::test_support::isolate_app_dir();
        let mut instance = crate::session::Instance::new("restart", "/tmp/aoe-restart-api");
        instance.view = crate::session::View::Structured;
        instance.source_profile = "default".into();
        instance.resume_intent =
            crate::session::ResumeIntent::Use("retained-restart-cancel".into());
        let id = instance.id.clone();
        crate::server::test_support::seed_instances_on_disk_for_test(
            "default",
            vec![instance.clone()],
        );

        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let launcher: crate::acp::supervisor::Launcher = {
            let calls = Arc::clone(&calls);
            Arc::new(move |config, _session_id| {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                let started = started_tx.clone();
                Box::pin(async move {
                    let lease = config
                        .launch_lease
                        .expect("supervisor retains launch lease");
                    started.send(call).expect("test receiver remains open");
                    lease.cancellation().cancelled().await;
                    Err(crate::acp::acp_client::AcpError::Spawn(
                        "launch cancelled before runner birth".into(),
                    ))
                })
            })
        };
        let state = crate::server::test_support::build_test_app_state_with_launcher(
            vec![instance],
            launcher,
        );
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let first = {
            let state = Arc::clone(&state);
            let id = id.clone();
            tokio::spawn(async move {
                spawn_acp(
                    State(state),
                    Path(id),
                    Ok(Json(SpawnAcpRequest {
                        recovery: None,
                        agent: None,
                        model: None,
                        additional_dirs: Vec::new(),
                        provider_env: Vec::new(),
                    })),
                )
                .await
                .into_response()
            })
        };
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(3), started_rx.recv())
                .await
                .expect("initial spawn reaches its blocked launcher"),
            Some(0)
        );
        let pending = find_instance(&state, &id).await.unwrap();
        let recovery = crate::acp::establishment::EstablishmentRecovery {
            expected: crate::acp::establishment::EstablishmentExpected::capture(
                &pending,
                &crate::session::Storage::open_reference("default").unwrap(),
            )
            .unwrap(),
            action: crate::acp::establishment::EstablishmentRecoveryAction::Same,
        };

        let restarting = {
            let state = Arc::clone(&state);
            let id = id.clone();
            tokio::spawn(async move {
                restart_acp(
                    State(state),
                    Path(id),
                    Ok(Json(SpawnAcpRequest {
                        recovery: Some(recovery),
                        agent: None,
                        model: None,
                        additional_dirs: Vec::new(),
                        provider_env: Vec::new(),
                    })),
                )
                .await
                .into_response()
            })
        };
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(3), started_rx.recv())
                .await
                .expect("restart cancels the first launch and starts its replacement"),
            Some(1)
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), first)
            .await
            .expect("the cancelled spawn request releases its guards")
            .expect("initial spawn handler panicked");

        let duplicate = restart_acp(
            State(Arc::clone(&state)),
            Path(id.clone()),
            Ok(Json(SpawnAcpRequest {
                recovery: None,
                agent: None,
                model: None,
                additional_dirs: Vec::new(),
                provider_env: Vec::new(),
            })),
        )
        .await
        .into_response();
        assert_eq!(duplicate.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(duplicate.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).starts_with("restart_in_progress:"));

        let stopped = shutdown_acp(State(Arc::clone(&state)), Path(id.clone()))
            .await
            .into_response();
        assert_eq!(stopped.status(), StatusCode::NO_CONTENT);
        let restart_response = tokio::time::timeout(std::time::Duration::from_secs(1), restarting)
            .await
            .expect("replacement launch observes shutdown")
            .expect("restart handler panicked");
        assert_eq!(restart_response.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(restart_response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).starts_with("spawn_cancelled:"));
        assert_eq!(
            state.acp_supervisor.worker_state(&id).await,
            crate::daemon::AcpWorkerState::Absent
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn delete_cancels_a_start_before_waiting_for_its_submission_guard() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let _home = crate::session::test_support::isolate_app_dir();
        let mut instance = crate::session::Instance::new("stop-start", "/tmp/aoe-stop-start-api");
        instance.view = crate::session::View::Structured;
        instance.source_profile = "default".into();
        crate::server::test_support::select_retained_for_test(&mut instance);
        let id = instance.id.clone();
        crate::server::test_support::seed_instances_on_disk_for_test(
            "default",
            vec![instance.clone()],
        );

        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let launcher: crate::acp::supervisor::Launcher = {
            let calls = Arc::clone(&calls);
            Arc::new(move |config, _session_id| {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                let started = started_tx.clone();
                Box::pin(async move {
                    let lease = config
                        .launch_lease
                        .expect("supervisor retains launch lease");
                    started.send(call).expect("test receiver remains open");
                    lease.cancellation().cancelled().await;
                    Err(crate::acp::acp_client::AcpError::Spawn(
                        "launch cancelled before runner birth".into(),
                    ))
                })
            })
        };
        let state = crate::server::test_support::build_test_app_state_with_launcher(
            vec![instance],
            launcher,
        );
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let spawning = {
            let state = Arc::clone(&state);
            let id = id.clone();
            tokio::spawn(async move {
                spawn_acp(
                    State(state),
                    Path(id),
                    Ok(Json(SpawnAcpRequest {
                        recovery: None,
                        agent: None,
                        model: None,
                        additional_dirs: Vec::new(),
                        provider_env: Vec::new(),
                    })),
                )
                .await
                .into_response()
            })
        };
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(3), started_rx.recv())
                .await
                .expect("initial spawn reaches its blocked launcher"),
            Some(0)
        );

        let stopped = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            shutdown_acp(State(Arc::clone(&state)), Path(id.clone())),
        )
        .await
        .expect("DELETE cancels launch before waiting on the submission guard")
        .into_response();
        assert_eq!(stopped.status(), StatusCode::NO_CONTENT);
        tokio::time::timeout(std::time::Duration::from_secs(1), spawning)
            .await
            .expect("cancelled start releases the submission guard")
            .expect("spawn handler panicked");
        assert_eq!(
            state.acp_supervisor.worker_state(&id).await,
            crate::daemon::AcpWorkerState::Absent
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn unhealthy_shutdown_does_not_cancel_a_pending_spawn() {
        let _home = crate::session::test_support::isolate_app_dir();
        let mut instance =
            crate::session::Instance::new("unhealthy-stop", "/tmp/aoe-unhealthy-stop");
        instance.view = crate::session::View::Structured;
        instance.source_profile = "default".into();
        let id = instance.id.clone();
        crate::server::test_support::seed_instances_on_disk_for_test(
            "default",
            vec![instance.clone()],
        );
        let state = crate::server::test_support::build_test_app_state(vec![instance]);
        let pending = state.acp_supervisor.begin_spawn_request(&id).unwrap();
        *state.canonical_health.write().await = crate::daemon::RuntimeHealth::Degraded {
            code: crate::daemon::ReloadFailureCode::Metadata,
            profiles: Vec::new(),
        };
        let response = shutdown_acp(State(Arc::clone(&state)), Path(id))
            .await
            .into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(!pending.cancellation().is_cancelled());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn shutdown_cancels_a_pre_admission_spawn_before_the_launcher() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let _home = crate::session::test_support::isolate_app_dir();
        let mut instance = crate::session::Instance::new("pre-admission", "/tmp/aoe-pre-admission");
        instance.view = crate::session::View::Structured;
        instance.source_profile = "default".into();
        let id = instance.id.clone();
        crate::server::test_support::seed_instances_on_disk_for_test(
            "default",
            vec![instance.clone()],
        );

        let calls = Arc::new(AtomicUsize::new(0));
        let launcher: crate::acp::supervisor::Launcher = {
            let calls = Arc::clone(&calls);
            Arc::new(move |_config, _session_id| {
                calls.fetch_add(1, Ordering::SeqCst);
                Box::pin(std::future::pending())
            })
        };
        let state = crate::server::test_support::build_test_app_state_with_launcher(
            vec![instance],
            launcher,
        );
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let delivering = state.session_service.prompt_submission(&id).await;
        let mut claims = state.session_service.watch_submission_claims();
        let spawning = {
            let state = Arc::clone(&state);
            let id = id.clone();
            tokio::spawn(async move {
                spawn_acp(
                    State(state),
                    Path(id),
                    Ok(Json(SpawnAcpRequest {
                        recovery: None,
                        agent: None,
                        model: None,
                        additional_dirs: Vec::new(),
                        provider_env: Vec::new(),
                    })),
                )
                .await
                .into_response()
            })
        };
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), claims.recv())
                .await
                .expect("spawn reaches the submission barrier")
                .expect("submission claimant remains alive"),
            id
        );

        let stopping = {
            let state = Arc::clone(&state);
            let id = id.clone();
            tokio::spawn(async move { shutdown_acp(State(state), Path(id)).await.into_response() })
        };
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), claims.recv())
                .await
                .expect("shutdown reaches the submission barrier")
                .expect("shutdown claimant remains alive"),
            id
        );
        let spawn_response = tokio::time::timeout(std::time::Duration::from_secs(1), spawning)
            .await
            .expect("stop fence cancels the pre-admission spawn without the guard dropping")
            .expect("spawn task panicked");
        assert_eq!(spawn_response.status(), StatusCode::CONFLICT);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        state
            .instances
            .write()
            .await
            .retain(|instance| instance.id != id);
        crate::session::Storage::open_unwatched("default")
            .unwrap()
            .update(|rows, _| {
                rows.retain(|instance| instance.id != id);
                Ok(())
            })
            .unwrap();
        drop(delivering);

        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(1), stopping)
                .await
                .expect("shutdown completes after the submission releases")
                .expect("shutdown task panicked")
                .status(),
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            state.acp_supervisor.worker_state(&id).await,
            crate::daemon::AcpWorkerState::Absent
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn restart_rejects_an_invalid_agent_without_stopping_the_worker() {
        for (id, agent, expected, restrict) in [
            (
                "restart-unknown-agent",
                "not-registered",
                StatusCode::BAD_REQUEST,
                false,
            ),
            (
                "restart-disallowed-agent",
                "codex",
                StatusCode::FORBIDDEN,
                true,
            ),
        ] {
            let _home = crate::session::test_support::isolate_app_dir();
            let mut instance = crate::session::Instance::new(id, "/tmp/aoe-invalid-restart-agent");
            instance.view = crate::session::View::Structured;
            instance.agent_name = Some("claude".into());
            instance.source_profile = "default".into();
            let id = instance.id.clone();
            crate::server::test_support::seed_instances_on_disk_for_test(
                "default",
                vec![instance.clone()],
            );
            let state = crate::server::test_support::build_test_app_state(vec![instance]);
            let commands = state
                .acp_supervisor
                .test_insert_worker_cmd_recording(&id)
                .await;
            if restrict {
                crate::session::config::update_config(|config| {
                    config.acp.restrict_agents = true;
                    config.acp.allowed_agents = vec!["claude".into()];
                })
                .unwrap();
            }

            let response = restart_acp(
                State(Arc::clone(&state)),
                Path(id.clone()),
                Ok(Json(SpawnAcpRequest {
                    recovery: None,
                    agent: Some(agent.into()),
                    model: None,
                    additional_dirs: Vec::new(),
                    provider_env: Vec::new(),
                })),
            )
            .await
            .into_response();
            assert_eq!(response.status(), expected, "agent={agent}");
            assert_eq!(
                state.acp_supervisor.worker_state(&id).await,
                crate::daemon::AcpWorkerState::Running,
                "invalid target must leave the existing worker live"
            );
            state.acp_supervisor.test_flush_worker_commands(&id).await;
            assert!(
                commands.lock().unwrap().is_empty(),
                "invalid target must not send shutdown to the worker"
            );
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn restart_waits_for_submission_before_stopping_a_running_worker() {
        let _home = crate::session::test_support::isolate_app_dir();
        let mut instance =
            crate::session::Instance::new("restart-barrier", "/tmp/aoe-restart-barrier");
        instance.view = crate::session::View::Structured;
        instance.agent_name = Some("claude".into());
        instance.source_profile = "default".into();
        instance.resume_intent =
            crate::session::ResumeIntent::Use("retained-restart-barrier".into());
        let id = instance.id.clone();
        crate::server::test_support::seed_instances_on_disk_for_test(
            "default",
            vec![instance.clone()],
        );

        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let launcher: crate::acp::supervisor::Launcher = Arc::new(move |config, _session_id| {
            let started = started_tx.clone();
            Box::pin(async move {
                let lease = config
                    .launch_lease
                    .expect("supervisor retains launch lease");
                started.send(()).expect("test receiver remains open");
                lease.cancellation().cancelled().await;
                Err(crate::acp::acp_client::AcpError::Spawn(
                    "launch cancelled before runner birth".into(),
                ))
            })
        });
        let state = crate::server::test_support::build_test_app_state_with_launcher(
            vec![instance],
            launcher,
        );
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let commands = state
            .acp_supervisor
            .test_insert_worker_cmd_recording(&id)
            .await;
        let submission = state.session_service.prompt_submission(&id).await;
        let mut claims = state.session_service.watch_submission_claims();
        let restarting = {
            let state = Arc::clone(&state);
            let id = id.clone();
            tokio::spawn(async move {
                restart_acp(
                    State(state),
                    Path(id),
                    Ok(Json(SpawnAcpRequest {
                        recovery: None,
                        agent: None,
                        model: None,
                        additional_dirs: Vec::new(),
                        provider_env: Vec::new(),
                    })),
                )
                .await
                .into_response()
            })
        };
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(3), claims.recv())
                .await
                .expect("restart reaches the submission barrier")
                .expect("restart claimant remains alive"),
            id
        );
        state.acp_supervisor.test_flush_worker_commands(&id).await;
        assert!(commands.lock().unwrap().is_empty());
        assert_eq!(
            state.acp_supervisor.worker_state(&id).await,
            crate::daemon::AcpWorkerState::Running
        );

        drop(submission);
        tokio::time::timeout(std::time::Duration::from_secs(3), started_rx.recv())
            .await
            .expect("restart passes the barrier and starts its replacement")
            .expect("replacement launcher remains alive");
        assert!(commands.lock().unwrap().contains(&"shutdown"));

        assert_eq!(
            shutdown_acp(State(Arc::clone(&state)), Path(id.clone()))
                .await
                .into_response()
                .status(),
            StatusCode::NO_CONTENT
        );
        let restart_response = tokio::time::timeout(std::time::Duration::from_secs(1), restarting)
            .await
            .expect("stopped replacement releases restart")
            .expect("restart task panicked");
        assert_eq!(restart_response.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn restart_survives_request_cancellation_after_worker_retirement() {
        use crate::acp::runner_lifecycle::ProcessControl as _;
        use crate::acp::supervisor::test_support::managed::with_managed;
        use crate::server::test_support::managed_control_session;
        with_managed(|scope| {
            Box::pin(async move {
                let session = managed_control_session(scope, "restart-disconnect").await;
                let state = session.state.clone();
                let id = session.id().to_owned();
                let original = session.client.lease.spawned_runner().unwrap();
                let row = find_instance(&state, &id).await.unwrap();
                let sid = session.sid.clone();
                let expected_sid = sid.clone();
                let (config, native) = scope.native_peer(
                    PathBuf::from(&row.project_path),
                    "claude",
                    "claude",
                    move |request| {
                        Some(match request["method"].as_str().unwrap() {
                            "initialize" => serde_json::json!({"jsonrpc":"2.0","id":request["id"],
                        "result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}}),
                            "session/load" => {
                                assert_eq!(request["params"]["sessionId"], expected_sid);
                                serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":{}})
                            }
                            other => panic!(
                                "restart cannot create or prompt another conversation: {other}"
                            ),
                        })
                    },
                );
                let events = scope.bind_external_launch(&id, config);
                let lock = state.instance_lock(&id).await;
                let guard = lock.lock().await;
                let request = tokio::spawn({
                    let state = state.clone();
                    let id = id.clone();
                    async move {
                        restart_acp(
                            State(state),
                            Path(id),
                            Ok(Json(SpawnAcpRequest {
                                recovery: None,
                                agent: None,
                                model: None,
                                additional_dirs: Vec::new(),
                                provider_env: Vec::new(),
                            })),
                        )
                        .await
                        .into_response()
                    }
                });
                tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    while state.acp_supervisor.worker_state(&id).await
                        != crate::daemon::AcpWorkerState::Absent
                    {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("the actual original retires before replacement exclusion");
                assert!(
                    !crate::acp::runner_lifecycle::SystemProcessControl::default()
                        .is_alive(original)
                );
                request.abort();
                assert!(request.await.unwrap_err().is_cancelled());
                drop(guard);
                let replacement = scope.observe_external_resume(id.clone(), events).await;
                replacement.ready().await;
                let replacement_owner = replacement.lease.spawned_runner().unwrap();
                assert_ne!(replacement_owner, original);
                assert_eq!(
                    find_instance(&state, &id)
                        .await
                        .unwrap()
                        .acp_session_id
                        .as_deref(),
                    Some(sid.as_str())
                );
                assert_eq!(
                    native
                        .requests()
                        .iter()
                        .filter(|method| *method == "session/load")
                        .count(),
                    1
                );
                assert!(!native
                    .requests()
                    .iter()
                    .any(|method| method == "session/new"));
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn restart_rechecks_session_and_cancellation_after_waiting_for_exclusion() {
        for (cancel, change_agent, change_view) in [
            (false, true, false),
            (true, false, false),
            (false, false, true),
        ] {
            use crate::acp::supervisor::test_support::managed::with_managed;
            use crate::server::test_support::managed_control_session;
            with_managed(|scope| {
                Box::pin(async move {
                    let session = managed_control_session(scope, "restart-exclusion").await;
                    let state = session.state.clone();
                    let id = session.id().to_owned();
                    let lock = state.instance_lock(&id).await;
                    let mut guard = Some(lock.lock().await);
                    let mut claims = state.session_service.watch_submission_claims();
                    let request = {
                        let state = Arc::clone(&state);
                        let id = id.clone();
                        tokio::spawn(async move {
                            restart_acp(
                                State(state),
                                Path(id),
                                Ok(Json(SpawnAcpRequest {
                                    recovery: None,
                                    agent: None,
                                    model: None,
                                    additional_dirs: Vec::new(),
                                    provider_env: Vec::new(),
                                })),
                            )
                            .await
                            .into_response()
                        })
                    };
                    for _ in 0..2 {
                        assert_eq!(
                            tokio::time::timeout(std::time::Duration::from_secs(3), claims.recv())
                                .await
                                .expect("restart reaches each submission barrier"),
                            Some(id.clone())
                        );
                    }
                    assert_eq!(
                        state.acp_supervisor.worker_state(&id).await,
                        crate::daemon::AcpWorkerState::Absent
                    );
                    if cancel {
                        state.acp_supervisor.cancel_restart_request(&id).await;
                    } else {
                        let mut instances = state.instances.write().await;
                        let row = instances.iter_mut().find(|row| row.id == id).unwrap();
                        if change_agent {
                            row.agent_name = Some("codex".into());
                        }
                        if change_view {
                            row.view = crate::session::View::Terminal;
                        }
                        drop(instances);
                        drop(guard.take());
                    }
                    let response = tokio::time::timeout(std::time::Duration::from_secs(3), request)
                        .await
                        .expect("restart finishes")
                        .expect("restart task");
                    assert_eq!(response.status(), StatusCode::CONFLICT);
                    assert_eq!(
                        state.acp_supervisor.worker_state(&id).await,
                        crate::daemon::AcpWorkerState::Absent
                    );
                    drop(guard.take());
                })
            })
            .await;
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn selectors_wait_for_a_provider_switch_before_choosing_the_worker() {
        use super::super::config::{
            acp_set_config_option, acp_set_mode, SetConfigOptionRequest, SetModeRequest,
        };
        use std::future::Future;
        use std::task::Poll;

        for legacy_mode in [false, true] {
            use crate::acp::supervisor::test_support::managed::with_managed;
            use crate::server::test_support::managed_control_session;
            with_managed(|scope| Box::pin(async move {
            let session = managed_control_session(scope, "selector-provider-order").await;
            let state = session.state.clone();
            let id = session.id().to_owned();
            let current = find_instance(&state, &id).await.unwrap();
            let (initialize_seen, initialize_observed) = tokio::sync::oneshot::channel();
            let mut initialize_seen = Some(initialize_seen);
            let selected = session.sid.clone();
            let (config, native) = scope.native_peer(
                PathBuf::from(&current.project_path), "claude", "claude",
                move |request| {
                    if request["method"] == "initialize" {
                        initialize_seen.take().unwrap().send(request.clone()).unwrap();
                        return None;
                    }
                    Some(match request["method"].as_str().unwrap() {
                    "session/load" => {
                        assert_eq!(request["params"]["sessionId"], selected);
                        serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":{
                            "modes":{"currentModeId":"old-mode","availableModes":[
                                {"id":"old-mode","name":"Old"},{"id":"plan","name":"Plan"}]},
                            "configOptions":[{"id":"model","name":"Model","type":"select",
                                "category":"model","currentValue":"default","options":[
                                    {"value":"default","name":"Default"},
                                    {"value":"new-provider-model","name":"New provider model"}]}]
                        }})
                    },
                    "session/set_mode" => serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":{}}),
                    "session/set_config_option" => serde_json::json!({"jsonrpc":"2.0","id":request["id"],
                        "result":{"configOptions":[]}}),
                    other => panic!("selector replacement must retain the selected conversation: {other}"),
                    })
                },
            );
            let events = scope.bind_external_launch(&id, config);
            let lock = state.instance_lock(&id).await;
            let guard = lock.lock().await;
            let mut switch_claims = state.session_service.watch_submission_claims();
            let mut switching = Box::pin(async {
                switch_acp_provider(
                    State(state.clone()),
                    Path(id.clone()),
                    Ok(Json(SwitchProviderRequest { provider: "vertex".into() })),
                ).await.into_response()
            });
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                tokio::select! {
                    claimed = switch_claims.recv() => {
                        assert_eq!(claimed.unwrap().as_str(), id.as_str());
                    }
                    response = &mut switching => panic!(
                        "provider switch bypassed its instance exclusion: {}", response.status(),
                    ),
                }
            }).await.expect("the actual switch claims submission before instance exclusion");
            let mut claims = switch_claims;
            let mut selecting = Box::pin(async {
                if legacy_mode {
                    acp_set_mode(
                        State(state.clone()),
                        Path(id.clone()),
                        Ok(Json(SetModeRequest {
                            mode_id: "plan".into(),
                            request_nonce: None,
                        })),
                    )
                    .await
                    .into_response()
                } else {
                    acp_set_config_option(
                        State(state.clone()),
                        Path(id.clone()),
                        Ok(Json(SetConfigOptionRequest {
                            config_id: "model".into(),
                            value: "new-provider-model".into(),
                            request_nonce: None,
                        })),
                    )
                    .await
                    .into_response()
                }
            });
            std::future::poll_fn(|cx| {
                assert!(
                    selecting.as_mut().poll(cx).is_pending(),
                    "the provider switch still owns submission"
                );
                Poll::Ready(())
            })
            .await;
            assert_eq!(
                tokio::time::timeout(std::time::Duration::from_secs(5), claims.recv())
                    .await
                    .unwrap()
                    .as_deref(),
                Some(id.as_str())
            );
            let command = if legacy_mode { "session/set_mode" } else { "session/set_config_option" };
            assert!(!session.native.requests().iter().any(|method| method == command),
                "no selection may reach the old provider while submission is held");
            drop(guard);
            let mut switched = None;
            let mut initialize_observed = initialize_observed;
            let initialize = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                loop {
                    tokio::select! {
                        request = &mut initialize_observed => break request.unwrap(),
                        response = &mut switching, if switched.is_none() => {
                            assert_eq!(response.status(), StatusCode::OK);
                            switched = Some(response);
                        },
                    }
                }
            }).await.expect("the actual provider switch reaches its replacement Native");
            native.send(serde_json::json!({"jsonrpc":"2.0","id":initialize["id"],
                "result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}})).await;
            let switched = match switched {
                Some(response) => response,
                None => tokio::time::timeout(std::time::Duration::from_secs(10), switching)
                    .await.unwrap(),
            };
            assert_eq!(switched.status(), StatusCode::OK);
            let replacement = scope.observe_external_resume(id.clone(), events).await;
            replacement.ready().await;
            let response = tokio::time::timeout(std::time::Duration::from_secs(5), selecting)
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            state.acp_supervisor.test_flush_worker_commands(&id).await;
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while !native.requests().iter().any(|method| method == command) {
                    tokio::task::yield_now().await;
                }
            }).await.expect("the new provider receives the queued selector");
            assert!(!session.native.requests().iter().any(|method| method == command));
            assert_eq!(native.requests().iter().filter(|method| *method == command).count(), 1);
            assert!(!native.requests().iter().any(|method| method == "session/new"));
            let live = find_instance(&state, &id).await.unwrap();
            let disk = crate::server::test_support::load_instances_from_disk_for_test("test");
            let stored = disk.iter().find(|row| row.id == id).unwrap();
            for row in [&live, stored] {
                assert_eq!(row.agent_provider.as_deref(), Some("vertex"));
                assert_eq!(
                    row.agent_model.as_deref(),
                    Some(if legacy_mode {
                        PROVIDER_DEFAULT_MODEL
                    } else {
                        "new-provider-model"
                    })
                );
            }
            })).await;
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn backend_handoff_freezes_source_before_arming_its_explicit_new_destination() {
        use crate::acp::establishment::{AcpEstablishment, EstablishmentTarget};
        use crate::server::test_support as support;
        for superseded in [false, true] {
            let _home = crate::session::test_support::isolate_app_dir();
            let project = tempfile::tempdir().unwrap();
            let mut source =
                crate::session::Instance::new("backend-source", project.path().to_str().unwrap());
            source.source_profile = "default".into();
            source.view = crate::session::View::Structured;
            source.agent_name = Some("claude".into());
            source.acp_session_id = Some("selected-original".into());
            source.acp_effort = Some("high".into());
            support::select_retained_for_test(&mut source);
            let id = source.id.clone();
            support::seed_instances_on_disk_for_test("default", vec![source.clone()]);
            let state = support::build_test_app_state(vec![source.clone()]);
            support::refresh_canonical_metadata_for_test(&state).await;
            let exclusion = crate::acp::sandbox::LaunchExclusion::acquire(state.clone(), &id, true)
                .await
                .unwrap();
            let (native, exclusion) = launch_store_for(&state, &source, exclusion).await.unwrap();
            if superseded {
                let store = native.clone();
                let source_id = id.clone();
                tokio::task::spawn_blocking(move || {
                    store.commit(&mut |rows, _| {
                        rows.iter_mut()
                            .find(|row| row.id == source_id)
                            .unwrap()
                            .resume_intent =
                            crate::session::ResumeIntent::Use("newer-user-selection".into());
                        Ok(())
                    })
                })
                .await
                .unwrap()
                .unwrap();
            }
            let before = find_instance(&state, &id).await.unwrap();
            let result = prepare_agent_switch(
                native,
                source,
                "codex".into(),
                Some("selected-model".into()),
            )
            .await;
            let retained = find_instance(&state, &id).await.unwrap();
            if superseded {
                assert!(result
                    .unwrap_err()
                    .is::<crate::session::LifecycleReservationError>());
                assert_eq!(
                    serde_json::to_value(&retained).unwrap(),
                    serde_json::to_value(&before).unwrap()
                );
            } else {
                let attempt = result.unwrap();
                assert_eq!(attempt.target, EstablishmentTarget::New);
                assert_eq!(attempt.preimage.agent_name.as_deref(), Some("codex"));
                assert_eq!(retained.agent_name.as_deref(), Some("codex"));
                assert_eq!(retained.agent_model.as_deref(), Some("selected-model"));
                assert_eq!(retained.acp_effort, None);
                assert_eq!(retained.acp_session_id, None);
                assert!(
                    matches!(&retained.acp_establishment, Some(AcpEstablishment::Pending { attempt: recorded })
                    if recorded.as_ref() == attempt.as_ref())
                );
                assert!(
                    attempt.producer.is_none(),
                    "a durable proposal is not an issued Native operation"
                );
            }
            assert_eq!(
                state.acp_supervisor.worker_state(&id).await,
                crate::daemon::AcpWorkerState::Absent
            );
            drop(exclusion);
        }
    }

    /// Both stores get the switch, and nothing adapter-specific survives it.
    #[tokio::test]
    #[serial_test::serial]
    async fn an_agent_switch_persists_its_model_to_both_stores() {
        use crate::session::test_support::isolate_app_dir;
        let profile = "default";

        // (requested model, what both stores must hold afterwards)
        for (requested, expected) in [(Some("gpt-5.6-sol"), Some("gpt-5.6-sol")), (None, None)] {
            let _tmp = isolate_app_dir();
            let mut inst = crate::session::Instance::new("claude", "/tmp/aoe-switch-model");
            inst.view = crate::session::View::Structured;
            inst.source_profile = profile.into();
            inst.agent_name = Some("claude".to_string());
            inst.agent_model = Some("claude-fable-5-1".to_string());
            inst.acp_effort = Some("high".to_string());
            inst.acp_session_id = Some("acp-old".to_string());
            let id = inst.id.clone();
            crate::server::test_support::seed_instances_on_disk_for_test(
                profile,
                vec![inst.clone()],
            );
            let state = crate::server::test_support::build_test_app_state(vec![inst]);

            crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
            let launch_row = find_instance(&state, &id).await.unwrap();
            let exclusion = crate::acp::sandbox::LaunchExclusion::acquire(state.clone(), &id, true)
                .await
                .unwrap();
            let (native, exclusion) = launch_store_for(&state, &launch_row, exclusion)
                .await
                .unwrap();
            let _exclusion = persist_agent_switch(native, &id, "codex", requested, exclusion)
                .await
                .unwrap();

            let on_disk = crate::server::test_support::load_instances_from_disk_for_test(profile);
            let stored = on_disk.iter().find(|i| i.id == id).expect("seeded row");
            assert_eq!(stored.agent_name.as_deref(), Some("codex"));
            assert_eq!(
                stored.agent_model.as_deref(),
                expected,
                "requested {requested:?}: the disk row is what a restart reads"
            );
            assert_eq!(stored.acp_session_id, None);
            assert_eq!(stored.acp_effort, None);
            assert!(stored.terminal_rebind_pending);

            let memory = state.instances.read().await;
            let live = memory.iter().find(|i| i.id == id).expect("instance");
            assert_eq!(live.agent_model.as_deref(), expected);
            assert_eq!(live.agent_name.as_deref(), Some("codex"));
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn agent_switch_completion_survives_respawn_but_rejects_a_new_launch() {
        use crate::server::test_support as support;
        for superseded in [false, true] {
            let _app_dir = crate::session::test_support::isolate_app_dir();
            let mut instance =
                crate::session::Instance::new("switch-lineage", "/tmp/aoe-switch-lineage");
            instance.source_profile = "default".into();
            instance.view = crate::session::View::Structured;
            instance.agent_name = Some("claude".into());
            instance.agent_model = Some("old-model".into());
            let id = instance.id.clone();
            support::seed_instances_on_disk_for_test("default", vec![instance.clone()]);
            let state = support::build_test_app_state(vec![instance.clone()]);
            support::refresh_canonical_metadata_for_test(&state).await;
            let exclusion = crate::acp::sandbox::LaunchExclusion::acquire(state.clone(), &id, true)
                .await
                .unwrap();
            let (native, exclusion) = launch_store_for(&state, &instance, exclusion)
                .await
                .unwrap();
            let launch_epoch = state.acp_supervisor.test_insert_worker(&id).await;
            drop(exclusion);
            let replacement = if superseded {
                state.acp_supervisor.test_remove_worker(&id).await;
                state.acp_supervisor.test_insert_worker(&id).await
            } else {
                state.acp_supervisor.test_respawn_worker(&id).await
            };
            assert_ne!(replacement, launch_epoch);
            let completion = complete_agent_switch(
                &state,
                native,
                &id,
                "codex",
                Some("new-model"),
                launch_epoch,
            )
            .await;
            if superseded {
                assert!(completion
                    .err()
                    .unwrap()
                    .is::<crate::session::LifecycleReservationError>());
            } else {
                drop(completion.expect("automatic descendant must retain the switch intent"));
            }
            let rows = support::load_instances_from_disk_for_test("default");
            let row = rows.iter().find(|row| row.id == id).unwrap();
            let expected = if superseded {
                ("claude", "old-model")
            } else {
                ("codex", "new-model")
            };
            assert_eq!(
                (row.agent_name.as_deref(), row.agent_model.as_deref()),
                (Some(expected.0), Some(expected.1))
            );
            let rows = state.instances.read().await;
            let row = rows.iter().find(|row| row.id == id).unwrap();
            assert_eq!(
                (row.agent_name.as_deref(), row.agent_model.as_deref()),
                (Some(expected.0), Some(expected.1))
            );
        }
    }

    /// A failed destination write never grants the new backend native authority.
    /// The old actor is retired; storage recovery permits one deliberate retry.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_failed_agent_switch_write_stops_the_target_and_a_retry_recovers() {
        use crate::acp::runner_lifecycle::ProcessControl as _;
        use crate::acp::supervisor::test_support::managed::with_managed;
        use crate::server::test_support as support;
        with_managed(|scope| Box::pin(async move {
            let session = support::managed_control_session(scope, "failed-backend-write").await;
            let state = session.state.clone();
            let id = session.id().to_owned();
            let original = session.client.lease.spawned_runner().unwrap();
            let switch = || async {
                switch_acp_agent(State(state.clone()), Path(id.clone()),
                    Ok(Json(SwitchAgentRequest { target: "codex".into(), model: None, reason: None })),
                ).await.into_response()
            };
            let switched = || state.acp_event_store.replay_from(&id, 0).unwrap().iter()
                .filter(|stored| matches!(stored.event.as_ref(), crate::acp::Event::AgentSwitched { .. })).count();
            let failure = support::fail_session_writes_for_test("test");
            let response = switch().await;
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert!(!status.is_success(), "{}", String::from_utf8_lossy(&body));
            assert!(!crate::acp::runner_lifecycle::SystemProcessControl::default().is_alive(original));
            assert!(!state.acp_supervisor.is_running(&id).await);
            assert_eq!(switched(), 0);
            assert_eq!(state.telemetry_structured.agent_switches.load(std::sync::atomic::Ordering::Relaxed), 0);
            for row in [
                find_instance(&state, &id).await.unwrap(),
                support::load_instances_from_disk_for_test("test").into_iter().find(|row| row.id == id).unwrap(),
            ] {
                assert_eq!(row.agent_name.as_deref(), Some("claude"));
                assert_eq!(row.acp_session_id.as_deref(), Some(session.sid.as_str()));
            }
            drop(failure);
            support::accept_runtime_read_cache_for_test(&state).await;
            session.client.shutdown().await.expect("settle only the exact original retirement");
            let row = find_instance(&state, &id).await.unwrap();
            let (config, native) = scope.native_peer(
                PathBuf::from(&row.project_path), "codex", &row.tool,
                |request| Some(match request["method"].as_str().unwrap() {
                    "initialize" => serde_json::json!({"jsonrpc":"2.0","id":request["id"],
                        "result":{"protocolVersion":1,"agentCapabilities":{}}}),
                    "session/new" => serde_json::json!({"jsonrpc":"2.0","id":request["id"],
                        "result":{"sessionId":"new-explicit-backend-conversation"}}),
                    other => panic!("backend switch cannot reuse the previous backend's Native SID: {other}"),
                }),
            );
            let events = scope.bind_external_launch(&id, config);
            let response = switch().await;
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
            assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
            let replacement = scope.observe_external_resume(id.clone(), events).await;
            replacement.ready().await;
            assert_ne!(replacement.lease.spawned_runner(), Some(original));
            assert_eq!(switched(), 1);
            assert_eq!(state.telemetry_structured.agent_switches.load(std::sync::atomic::Ordering::Relaxed), 1);
            for row in [
                find_instance(&state, &id).await.unwrap(),
                support::load_instances_from_disk_for_test("test").into_iter().find(|row| row.id == id).unwrap(),
            ] {
                assert_eq!(row.agent_name.as_deref(), Some("codex"));
                assert_eq!(row.acp_effort, None);
                assert_eq!(row.acp_session_id.as_deref(), Some("new-explicit-backend-conversation"));
            }
            assert_eq!(native.requests().iter().filter(|method| *method == "session/new").count(), 1);
            assert!(!native.requests().iter().any(|method| method == "session/load"));
        })).await;
    }

    /// The pick reaches both stores, and the model resets to the provider's
    /// default: ids are provider-specific, and a resumed transcript would
    /// otherwise keep its old one. Everything naming the conversation survives, because the
    /// provider changes where the tokens come from, not which transcript is
    /// resumed.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_provider_switch_persists_the_pick_and_resets_the_model() {
        use crate::session::test_support::isolate_app_dir;
        let profile = "default";

        for provider in crate::session::environment::AGENT_PROVIDERS {
            let _tmp = isolate_app_dir();
            let mut inst = crate::session::Instance::new("claude", "/tmp/aoe-switch-provider");
            inst.source_profile = profile.into();
            inst.view = crate::session::View::Structured;
            inst.agent_name = Some("claude".to_string());
            inst.agent_model = Some("claude-fable-5-1".to_string());
            inst.acp_effort = Some("high".to_string());
            inst.acp_session_id = Some("acp-old".to_string());
            let id = inst.id.clone();
            crate::server::test_support::seed_instances_on_disk_for_test(
                profile,
                vec![inst.clone()],
            );
            let state = crate::server::test_support::build_test_app_state(vec![inst]);

            crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
            let expected = find_instance(&state, &id).await.unwrap();
            let exclusion = crate::acp::sandbox::LaunchExclusion::acquire(state.clone(), &id, true)
                .await
                .unwrap();
            let (native, exclusion) = launch_store_for(&state, &expected, exclusion)
                .await
                .unwrap();
            persist_provider_switch(native, &expected, provider, exclusion)
                .await
                .expect("persisting the pick");

            let on_disk = crate::server::test_support::load_instances_from_disk_for_test(profile);
            let stored = on_disk.iter().find(|i| i.id == id).expect("seeded row");
            assert_eq!(
                stored.agent_provider.as_deref(),
                Some(*provider),
                "the disk row is what a restart reads"
            );
            assert_eq!(stored.agent_model.as_deref(), Some(PROVIDER_DEFAULT_MODEL));
            assert_eq!(stored.acp_session_id.as_deref(), Some("acp-old"));
            assert_eq!(stored.acp_effort.as_deref(), Some("high"));

            let memory = state.instances.read().await;
            let live = memory.iter().find(|i| i.id == id).expect("instance");
            assert_eq!(live.agent_provider.as_deref(), Some(*provider));
            assert_eq!(live.agent_model.as_deref(), Some(PROVIDER_DEFAULT_MODEL));
        }
    }

    /// A status poll that read `sessions.json` before the switch committed must
    /// not land after it: the spawn request is built from the memory row, so
    /// the stale reload would respawn on the old routing and model.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_reload_read_before_the_switch_cannot_undo_it() {
        use crate::session::test_support::isolate_app_dir;
        let profile = "default";
        let _tmp = isolate_app_dir();
        let mut inst = crate::session::Instance::new("claude", "/tmp/aoe-switch-provider-stale");
        inst.source_profile = profile.into();
        inst.view = crate::session::View::Structured;
        inst.agent_name = Some("claude".to_string());
        inst.agent_model = Some("claude-fable-5-1".to_string());
        crate::server::test_support::select_retained_for_test(&mut inst);
        let id = inst.id.clone();
        crate::server::test_support::seed_instances_on_disk_for_test(profile, vec![inst.clone()]);
        let state = crate::server::test_support::build_test_app_state(vec![inst]);

        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let read_metadata = state.canonical_metadata.read().await.clone();
        let read_epoch = state
            .mutation_epoch
            .load(std::sync::atomic::Ordering::SeqCst);
        let stale = crate::server::test_support::load_instances_from_disk_for_test(profile);
        let expected = find_instance(&state, &id).await.unwrap();
        let exclusion = crate::acp::sandbox::LaunchExclusion::acquire(state.clone(), &id, true)
            .await
            .unwrap();
        let (native, exclusion) = launch_store_for(&state, &expected, exclusion)
            .await
            .unwrap();
        persist_provider_switch(native, &expected, "vertex", exclusion)
            .await
            .expect("persisting the pick");
        crate::server::reload::reload_state_instances_from_disk(
            &state,
            stale.into(),
            Vec::new(),
            crate::server::state::StatusSource::DiskOnly,
            read_epoch,
            Ok(read_metadata),
            Default::default(),
        )
        .await;

        let instance = find_instance(&state, &id).await.expect("instance");
        let request = spawn_request_for(&instance, "claude".to_string(), None);
        assert_eq!(request.provider.as_deref(), Some("vertex"));
        assert_eq!(request.model.as_deref(), Some(PROVIDER_DEFAULT_MODEL));
    }

    /// The switch tears the worker down, so a busy worker, or one whose last
    /// teardown is unproven, is refused before the row or container changes.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_provider_switch_refuses_a_busy_or_unsettled_worker() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        use crate::server::test_support as support;
        with_managed(|scope| {
            Box::pin(async move {
                let mut session = support::managed_control_session(scope, "provider-busy").await;
                let prompt = session.start_prompt("still working").await;
                let state = session.state.clone();
                let id = session.id().to_owned();
                let before = find_instance(&state, &id).await.unwrap();
                let original = session
                    .client
                    .lease
                    .spawned_runner()
                    .expect("actual Native actor");
                let response = switch_acp_provider(
                    State(state.clone()),
                    Path(id.clone()),
                    Ok(Json(SwitchProviderRequest {
                        provider: "vertex".into(),
                    })),
                )
                .await
                .into_response();
                assert_eq!(response.status(), StatusCode::CONFLICT);
                let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .unwrap();
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(body["error"], "turn_active");
                assert!(state.acp_supervisor.is_running(&id).await);
                assert_eq!(session.client.lease.spawned_runner(), Some(original));
                let after = find_instance(&state, &id).await.unwrap();
                assert_eq!(
                    serde_json::to_value(&after).unwrap(),
                    serde_json::to_value(&before).unwrap()
                );
                let stored = support::load_instances_from_disk_for_test("test")
                    .into_iter()
                    .find(|row| row.id == id)
                    .unwrap();
                assert_eq!(stored.agent_provider, before.agent_provider);
                assert_eq!(stored.agent_model, before.agent_model);
                assert!(!session
                    .native
                    .requests()
                    .iter()
                    .any(|method| method == "session/cancel"));
                session.finish_prompt(&prompt, None).await;
            })
        })
        .await;

        let profile = "default";
        {
            let case = "unproven stop";
            let code = "worker_not_stopped";
            let _tmp = crate::session::test_support::isolate_app_dir();
            let mut inst = crate::session::Instance::new("claude", "/tmp/aoe-switch-provider-gate");
            inst.source_profile = profile.into();
            inst.view = crate::session::View::Structured;
            inst.agent_name = Some("claude".to_string());
            inst.agent_model = Some("claude-fable-5-1".to_string());
            support::select_retained_for_test(&mut inst);
            let id = inst.id.clone();
            support::seed_instances_on_disk_for_test(profile, vec![inst.clone()]);
            let state = support::build_test_app_state(vec![inst]);
            support::refresh_canonical_metadata_for_test(&state).await;
            let lease = state.acp_supervisor.test_hold_stopping(&id).unwrap();

            let response = switch_acp_provider(
                State(Arc::clone(&state)),
                Path(id.clone()),
                Ok(Json(SwitchProviderRequest {
                    provider: "vertex".to_string(),
                })),
            )
            .await
            .into_response();

            assert_eq!(response.status(), StatusCode::CONFLICT, "{case}");
            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"], code, "{case}");
            let on_disk = crate::server::test_support::load_instances_from_disk_for_test(profile);
            let memory = state.instances.read().await;
            for row in [
                on_disk.iter().find(|i| i.id == id),
                memory.iter().find(|i| i.id == id),
            ] {
                let row = row.expect("seeded row");
                assert_eq!(row.agent_provider, None, "{case}");
                assert_eq!(
                    row.agent_model.as_deref(),
                    Some("claude-fable-5-1"),
                    "{case}"
                );
            }
            state.acp_supervisor.test_settle_stopping(&lease);
        }
    }

    #[test]
    fn acp_agent_entries_follow_policy_and_wire_shape() {
        let registry = crate::acp::AgentRegistry::with_defaults();
        let names = |p: &AgentPolicy| -> Vec<String> {
            acp_agent_entries(&registry, p)
                .into_iter()
                .map(|e| e.name)
                .collect()
        };

        let all = names(&AgentPolicy::for_test(false, &[]));
        assert_eq!(all.len(), registry.agents.len());
        assert!(all.windows(2).all(|w| w[0] <= w[1]), "sorted: {all:?}");
        // A listed name missing from the registry does not invent an entry.
        assert_eq!(
            names(&AgentPolicy::for_test(
                true,
                &["opencode", "claude", "not-a-real-agent"]
            )),
            vec!["claude".to_string(), "opencode".to_string()]
        );
        assert!(names(&AgentPolicy::for_test(true, &[])).is_empty());

        let entries = acp_agent_entries(
            &registry,
            &AgentPolicy::for_test(true, &["claude", "gemini"]),
        );
        let claude = serde_json::to_value(&entries[0]).unwrap();
        assert_eq!(claude["command"], "claude-agent-acp");
        assert!(!entries[0].description.is_empty());
        // Lifecycle is omitted while active.
        assert!(claude.get("lifecycle").is_none(), "{claude}");
        let gemini = serde_json::to_value(&entries[1]).unwrap();
        assert_eq!(gemini["lifecycle"]["state"], "deprecated");
        assert_eq!(gemini["lifecycle"]["replacement"], "antigravity");
    }

    #[test]
    fn rate_limit_resume_marker_follows_the_durable_park() {
        use crate::acp::event_store::RateLimitPark;
        let ts = |raw| {
            DateTime::parse_from_rfc3339(raw)
                .unwrap()
                .with_timezone(&Utc)
        };
        let fallback = ts("2099-01-01T00:00:00Z");
        let resets_at = ts("2099-02-03T04:05:06Z");
        let info = |resets_at| RateLimitInfo {
            status: "limited".to_string(),
            resets_at,
            kind: "rate_limit".to_string(),
        };
        let park = |info, cap_reached| RateLimitPark {
            seq: 1,
            info,
            recorded_at_ms: 0,
            cap_reached,
            last_resume_attempt_ms: None,
        };
        let cases = [
            ("no park", None, None),
            (
                "reported reset",
                Some(park(Some(info(Some(resets_at))), false)),
                Some(resets_at),
            ),
            (
                "reset unknown",
                Some(park(Some(info(None)), false)),
                Some(fallback),
            ),
            ("limit row pruned", Some(park(None, false)), Some(fallback)),
            (
                "cap park",
                Some(park(Some(info(Some(resets_at))), true)),
                Some(fallback),
            ),
        ];
        for (label, park, expected) in cases {
            assert_eq!(
                rate_limit_resume_marker_resets_at(park.as_ref(), fallback),
                expected,
                "{label}"
            );
        }
    }

    /// #4092: the manual resume installs a continuation under the session's
    /// submission authority, so `/acp/spawn` must claim it ahead of the
    /// instance lock. The reverse order would close a cycle with every path
    /// that takes the submission guard before the instance lock.
    #[tokio::test]
    #[serial_test::serial]
    async fn spawn_claims_the_submission_guard_before_the_instance_lock() {
        use crate::session::test_support::isolate_app_dir;
        let _app_dir = isolate_app_dir();
        let mut inst = crate::session::Instance::new("sess-4092-spawn", "/tmp/aoe-4092-spawn");
        inst.id = "sess-4092-spawn".to_string();
        inst.view = crate::session::View::Structured;
        let id = inst.id.clone();
        let state = crate::server::test_support::build_test_app_state(vec![inst]);

        let _held = state
            .session_service
            .prompt_submission_for_session(&id)
            .await
            .expect("seeded session must admit a submission");
        let mut claims = state.session_service.watch_submission_claims();
        let spawn = {
            let state = Arc::clone(&state);
            let id = id.clone();
            async move {
                spawn_acp(
                    State(state),
                    Path(id),
                    Ok(Json(SpawnAcpRequest {
                        recovery: None,
                        agent: None,
                        model: None,
                        additional_dirs: Vec::new(),
                        provider_env: Vec::new(),
                    })),
                )
                .await
                .into_response()
            }
        };
        tokio::pin!(spawn);
        assert!(futures_util::poll!(&mut spawn).is_pending());
        assert_eq!(
            claims
                .try_recv()
                .expect("spawn reaches its submission claim"),
            id
        );
        let instance_lock = state.instance_lock(&id).await;
        let _instance_lock = instance_lock
            .try_lock()
            .expect("submission must precede the instance lock");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn rate_limit_manual_resume_tolerates_an_already_running_worker() {
        use crate::acp::event_store::ControlKnowledge::{Indeterminate, Known};
        use crate::acp::supervisor::test_support::managed::with_managed;
        use crate::acp::Event;
        use crate::server::test_support::managed_control_session;
        with_managed(|scope| {
            Box::pin(async move {
                let mut live = managed_control_session(scope, "manual-quota-running").await;
                let ordinary = managed_control_session(scope, "ordinary-running").await;
                let id = live.id().to_owned();
                let state = live.state.clone();
                let original = state
                    .acp_supervisor
                    .current_native_availability(&id)
                    .await
                    .unwrap();
                live.park("continue the interrupted work").await;
                let frontier = state.acp_event_store.highest_seq(&id);
                let request = || {
                    Ok(Json(SpawnAcpRequest {
                        recovery: None,
                        agent: None,
                        model: None,
                        additional_dirs: Vec::new(),
                        provider_env: Vec::new(),
                    }))
                };
                let response = spawn_acp(State(state.clone()), Path(id.clone()), request())
                    .await
                    .into_response();
                assert_eq!(response.status(), StatusCode::OK);
                let prompt = live.next_prompt().await;
                assert_eq!(prompt["params"]["sessionId"], live.sid);
                assert_eq!(
                    prompt["params"]["prompt"][0]["text"],
                    "continue the interrupted work"
                );
                assert_eq!(
                    state
                        .acp_supervisor
                        .current_native_availability(&id)
                        .await
                        .unwrap()
                        .owner,
                    original.owner
                );
                assert_eq!(
                    state.acp_supervisor.current_lease(&id),
                    Some(original.lease.clone())
                );
                let delivered = state.acp_event_store.replay_from(&id, frontier).unwrap();
                let breadcrumbs = delivered
                    .iter()
                    .filter(|stored| {
                        matches!(
                            stored.event.as_ref(),
                            Event::RateLimitAutoResumed { manual: true, .. }
                        )
                    })
                    .collect::<Vec<_>>();
                let prompts = delivered
                    .iter()
                    .filter(|stored| matches!(stored.event.as_ref(), Event::UserPromptSent { .. }))
                    .collect::<Vec<_>>();
                assert_eq!(breadcrumbs.len(), 1);
                assert_eq!(prompts.len(), 1);
                assert!(breadcrumbs[0].seq < prompts[0].seq);
                assert!(matches!(
                    state.acp_event_store.rate_limit_park(&id).unwrap(),
                    Known(None)
                ));
                assert_eq!(
                    state
                        .acp_event_store
                        .rate_limit_redelivery_streak(&id)
                        .unwrap(),
                    Known(0)
                );
                let ordinary_before = ordinary.state.acp_event_store.highest_seq(ordinary.id());
                let ordinary_requests = ordinary.native.requests();
                let ordinary_response = spawn_acp(
                    State(ordinary.state.clone()),
                    Path(ordinary.id().into()),
                    request(),
                )
                .await
                .into_response();
                assert_eq!(ordinary_response.status(), StatusCode::OK);
                assert_eq!(
                    ordinary.state.acp_event_store.highest_seq(ordinary.id()),
                    ordinary_before
                );
                assert_eq!(ordinary.native.requests(), ordinary_requests);
                live.finish_prompt(&prompt, Some("rate_limit")).await;
                let park = match state.acp_event_store.rate_limit_park(&id).unwrap() {
                    Known(Some(park)) => park,
                    other => panic!("expected known park: {other:?}"),
                };
                state
                    .acp_event_store
                    .test_conn()
                    .execute(
                        "DELETE FROM acp_events WHERE session_id=?1 AND seq<?2",
                        rusqlite::params![id, i64::try_from(park.seq).unwrap()],
                    )
                    .unwrap();
                state.acp_event_store.control_cache().forget(&id);
                assert!(matches!(
                    state.acp_event_store.rate_limit_park(&id).unwrap(),
                    Indeterminate
                ));
                let before = state.acp_event_store.highest_seq(&id);
                let methods = live.native.requests();
                let reattached = spawn_acp(State(state.clone()), Path(id.clone()), request())
                    .await
                    .into_response();
                assert_eq!(reattached.status(), StatusCode::OK);
                assert!(matches!(
                    state.acp_event_store.rate_limit_park(&id).unwrap(),
                    Indeterminate
                ));
                let refused = spawn_acp(
                    State(state.clone()),
                    Path(id.clone()),
                    Ok(Json(SpawnAcpRequest {
                        recovery: None,
                        agent: None,
                        model: Some("new-model".into()),
                        additional_dirs: Vec::new(),
                        provider_env: Vec::new(),
                    })),
                )
                .await
                .into_response();
                assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
                assert_eq!(state.acp_event_store.highest_seq(&id), before);
                assert_eq!(live.native.requests(), methods);
                assert_eq!(
                    state.acp_supervisor.current_lease(&id),
                    Some(original.lease)
                );
            })
        })
        .await;
    }
}
