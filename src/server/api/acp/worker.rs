//! Worker spawn, shutdown, and agent switching.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

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

async fn rate_limit_resume_probe(state: &AppState, id: &str) -> Option<DateTime<Utc>> {
    let store = Arc::clone(&state.acp_event_store);
    let id_for_probe = id.to_string();
    tokio::task::spawn_blocking(move || {
        let park = store.rate_limit_park(&id_for_probe);
        rate_limit_resume_marker_resets_at(park.as_ref(), Utc::now())
    })
    .await
    .unwrap_or_else(|e| {
        tracing::warn!(target: "http.api.acp", session = %id, "rate-limit resume probe failed: {e}");
        None
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
    if rate_limit_resume_probe(&state, &id).await.is_some() {
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
        launch_admission: Some(crate::acp::supervisor::LaunchAdmission {
            store: native,
            generation: instance.lifecycle_generation,
            namespace: Some(state.profile_namespace.clone()),
            native_scope: None,
        }),
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
        return supervisor_error_response(
            "spawn cancelled",
            &SupervisorError::SpawnCancelled(id.clone()),
        );
    }
    // Reserve the lifecycle generation while the preparation exclusion is
    // still held, then release all request locks before spawn configuration
    // and before_session hooks. Hooks may re-enter the API for this session.
    let reservation = if let Some(reservation) = resume_reservation {
        reservation
    } else {
        match state
            .acp_supervisor
            .reserve_spawn(
                &id,
                if request.stored_acp_session_id.is_some() {
                    crate::acp::supervisor::ResumeKind::Spawn
                } else {
                    crate::acp::supervisor::ResumeKind::SpawnFirst
                },
                crate::process::worker_registry::ResumeAuthority::Explicit,
            )
            .await
        {
            Ok(reservation) => reservation,
            Err(e) => return supervisor_error_response("spawn failed", &e),
        }
    };
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
            restart_acp_inner(state, id, req).await
        })
        .await
    {
        Ok(response) => response,
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

async fn restart_acp_inner(state: Arc<AppState>, id: String, req: SpawnAcpRequest) -> Response {
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

    let stop_request = state
        .acp_supervisor
        .begin_stop_request(&id, "user_stopped", None, false)
        .await;
    let was_resuming = stop_request.cancelled_launch();
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
        if let Err(response) = retire_restart_worker(&state, &preflight_instance, &restart).await {
            return response;
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
        if let Err(response) = retire_restart_worker(&state, &instance, &restart).await {
            return response;
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
        )
        .await
        {
            Ok(prepared) => prepared,
            Err(response) => return response,
        };
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
        launch_admission: Some(crate::acp::supervisor::LaunchAdmission {
            store: native,
            generation: instance.lifecycle_generation,
            namespace: Some(state.profile_namespace.clone()),
            native_scope: None,
        }),
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
    let launched = match resume_reservation {
        Some(reservation) => state.acp_supervisor.spawn_inner(request, reservation).await,
        None => {
            state
                .acp_supervisor
                .spawn_for_restart(
                    request,
                    crate::process::worker_registry::ResumeAuthority::Explicit,
                    &restart,
                )
                .await
        }
    };
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

async fn retire_restart_worker(
    state: &Arc<AppState>,
    expected: &crate::session::Instance,
    restart: &crate::acp::supervisor::RestartRequestGuard,
) -> Result<(), Response> {
    if restart.is_cancelled() {
        return Err(supervisor_error_response(
            "restart cancelled",
            &SupervisorError::SpawnCancelled(expected.id.clone()),
        ));
    }
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
                && row.agent_provider == agent_provider,
            crate::session::LifecycleReservationError::Superseded
        );
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
        let binding = crate::server::acp_events::BindingRetirement::prepare_captured(
            &owner,
            target,
            &row,
            native.storage(),
        )?;
        owner
            .acp_supervisor
            .register_delegated_binding_retirement(&binding.target, binding.checkpoint.clone())?;
        Ok((native, binding))
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result);
    let (native, binding) = prepared.map_err(|error| {
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
        binding.commit_under_native_lock(&native, &id)?;
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
    .map_err(|error| restart_retirement_error("restart native retirement checkpoint", &error))
}

fn restart_retirement_error(context: &str, error: &anyhow::Error) -> Response {
    if let Some(error) = error.downcast_ref::<SupervisorError>() {
        return supervisor_error_response(context, error);
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
            instance.agent_name = Some(target.clone());
            instance.acp_session_id = None;
            instance.import_pending = None;
            instance.acp_effort = None;
            instance.agent_model = model.clone();
            instance.terminal_rebind_pending = true;
            Ok(())
        })?;
        Ok(exclusion)
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
    let from_agent = match check_switch_target(&state, &instance, &target).await {
        Ok(agent) => agent,
        Err(resp) => return resp,
    };
    let (instance, resume_reservation) = match crate::server::api::sessions::resume_structured_stop(
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
        if let Err(e) = crate::server::acp_events::retire_native_worker(
            &state,
            &id,
            std::time::Duration::from_secs(5),
        )
        .await
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("shutdown failed before agent switch: {e}"),
            )
                .into_response();
        }
    }
    {
        let mut instances = state.instances.write().await;
        if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
            inst.acp_load_session_capable = None;
        }
    }

    let (sandbox_info, exclusion) = match crate::acp::sandbox::ensure_container_for_session(
        native.clone(),
        instance.clone(),
        exclusion,
    )
    .await
    {
        Ok(info) => info,
        Err(e) => return launch_error_response("sandbox container ensure", &e),
    };

    let model = req.model.clone();

    // A new backend starts a fresh session. Effort vocabularies are
    // adapter-specific, so the old pick is dropped too.
    let request = SpawnRequest {
        launch_admission: Some(crate::acp::supervisor::LaunchAdmission {
            store: native.clone(),
            generation: instance.lifecycle_generation,
            namespace: Some(state.profile_namespace.clone()),
            native_scope: None,
        }),
        model: model.clone(),
        effort: None,
        effort_explicit: false,
        stored_acp_session_id: None,
        fork_from: None,
        seed_history_replay: false,
        prior_conversation: false,
        sandbox_continuation: crate::acp::supervisor::SandboxContinuation::Fresh,
        claude_store_pin: None,
        ..spawn_request_for(&instance, target.clone(), sandbox_info)
    };
    if let Some(resp) = refuse_if_stored_row_shelved(&state, &instance).await {
        return resp;
    }
    let reservation = if let Some(reservation) = resume_reservation {
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
    let launch_epoch = reservation.lease().epoch();
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
                return persist_failure_response(&state, &id, "agent switch", &error).await;
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
) -> anyhow::Result<crate::acp::sandbox::LaunchExclusion> {
    use crate::session::SessionStore;
    let expected = expected.clone();
    let provider = provider.to_owned();
    tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let _lifecycle = store
            .storage()
            .acquire_instance_lifecycle_lock(&expected.id)?;
        store.commit(&mut |rows, _| {
            let row = rows
                .iter_mut()
                .find(|row| row.id == expected.id)
                .ok_or(crate::session::LifecycleReservationError::Superseded)?;
            anyhow::ensure!(
                row.lifecycle_generation == expected.lifecycle_generation
                    && row.title == expected.title
                    && row.agent_provider == expected.agent_provider
                    && row.is_structured()
                    && row.launch_is_finalized()
                    && row.terminal_handoff.is_none(),
                crate::session::LifecycleReservationError::Superseded
            );
            row.ensure_startable()?;
            row.agent_provider = Some(provider.clone());
            row.agent_model = Some(PROVIDER_DEFAULT_MODEL.to_owned());
            Ok(())
        })?;
        Ok(exclusion)
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
    // The submission guard keeps new prompts out but does not wait for the
    // running one, and the shutdown below aborts it.
    let control = state.session_service.fold_control_state(&id).await;
    if control.turn_active || control.has_active_background_agent() {
        return super::super::api_error(
            StatusCode::CONFLICT,
            "turn_active",
            "the session is mid-turn; switch providers once it finishes",
        );
    }
    let (instance, resume_reservation) =
        match crate::server::api::sessions::resume_structured_stop(&state, instance, None).await {
            Ok(instance) => instance,
            Err(response) => return response,
        };
    let (native, exclusion) = match launch_store_for(&state, &instance, exclusion).await {
        Ok(store) => store,
        Err(response) => return response,
    };

    if resume_reservation.is_none() {
        if let Err(e) = crate::server::acp_events::retire_native_worker(
            &state,
            &id,
            std::time::Duration::from_secs(5),
        )
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
    let exclusion =
        match persist_provider_switch(native.clone(), &instance, &provider, exclusion).await {
            Ok(exclusion) => exclusion,
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
    let (sandbox_info, exclusion) = match crate::acp::sandbox::ensure_container_for_session(
        native.clone(),
        updated.clone(),
        exclusion,
    )
    .await
    {
        Ok(info) => info,
        Err(e) => return launch_error_response("sandbox container ensure", &e),
    };

    let request = SpawnRequest {
        launch_admission: Some(crate::acp::supervisor::LaunchAdmission {
            store: native,
            generation: instance.lifecycle_generation,
            namespace: Some(state.profile_namespace.clone()),
            native_scope: None,
        }),
        ..spawn_request_for(&updated, agent, sandbox_info)
    };
    if let Some(resp) = refuse_if_stored_row_shelved(&state, &updated).await {
        return resp;
    }
    let reservation = if let Some(reservation) = resume_reservation {
        reservation
    } else {
        match state
            .acp_supervisor
            .reserve_spawn(
                &id,
                if request.stored_acp_session_id.is_some() {
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

            let (resumed, _reservation) =
                crate::server::api::sessions::resume_structured_stop(&state, stopped, None)
                    .await
                    .unwrap_or_else(|response| panic!("resume failed: {}", response.status()));
            assert_eq!(resumed.status, Status::Idle);
            assert!(!resumed.is_idle_dormant());
            let admission = crate::acp::supervisor::LaunchAdmission {
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
        use crate::acp::protocol::AcpBroadcastFrame;
        use crate::session::{
            ConversationBinding, ConversationProvenance, ExecutionBinding, Instance, Status,
            Storage, View,
        };
        use std::time::Duration;

        for (first_assignment, retire) in
            [(true, true), (false, true), (true, false), (false, false)]
        {
            let _home = crate::session::test_support::isolate_app_dir();
            let project = tempfile::tempdir().unwrap();
            let selected_store = project.path().join("selected-claude-store");
            std::fs::create_dir(&selected_store).unwrap();
            let sid = "11111111-1111-4111-8111-111111111111";
            let binding = ConversationBinding {
                session_id: sid.into(),
                execution: Some(ExecutionBinding {
                    agent: "claude".into(),
                    stores: vec![selected_store.clone()],
                    configuration: Vec::new(),
                    cwd: project.path().to_path_buf(),
                    cwd_filesystem: "host".into(),
                    filesystem: "host".into(),
                    exported_default_store: Some(false),
                }),
                provenance: ConversationProvenance::Observed,
                transcript_path: None,
            };
            let mut row = Instance::new("late assignment", project.path().to_str().unwrap());
            row.source_profile = "default".into();
            row.view = View::Structured;
            row.status = Status::Running;
            row.lifecycle_generation = 37;
            row.agent_session_id = Some(sid.into());
            row.agent_session_binding = Some(binding.clone());
            row.acp_session_id = (!first_assignment).then(|| sid.to_owned());
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
            let old_epoch = state.acp_supervisor.running_lease(&row.id).unwrap().epoch();
            let (native, ()) = crate::server::session_store::NativeSessionStore::open_for_launch(
                state.clone(),
                &row,
                (),
            )
            .await
            .unwrap();
            let native_scope = Some(
                crate::acp::state::NativeObservationScope::capture(&row, native.storage()).unwrap(),
            );
            state
                .acp_supervisor
                .running_lease(&row.id)
                .unwrap()
                .set_admission(Some(crate::acp::supervisor::LaunchAdmission {
                    store: native,
                    generation: row.lifecycle_generation,
                    namespace: Some(state.profile_namespace.clone()),
                    native_scope,
                }));
            let gate = crate::server::acp_events::pause_assignment_listener_for_test(&row.id, 1);
            let listener =
                tokio::spawn(crate::server::acp_events::acp_event_listener(state.clone()));
            tokio::time::timeout(Duration::from_secs(5), async {
                while state.acp_events_tx.receiver_count() == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("listener subscribed");
            let mut event = crate::acp::Event::AcpSessionAssigned {
                acp_session_id: sid.into(),
                connection_ready: false,
                native_subject: None,
            };
            let subject = crate::acp::state::NativeEventSubject::channel(
                state
                    .acp_supervisor
                    .running_lease(&row.id)
                    .unwrap()
                    .native_scope()
                    .unwrap()
                    .unwrap()
                    .context,
                Some(crate::acp::runner_lifecycle::RunnerIdentity::from_record(
                    &record,
                )),
                &event,
            );
            event.set_native_subject(subject);
            state.acp_event_store.record(&row.id, 1, &event).unwrap();
            state
                .acp_events_tx
                .send(AcpBroadcastFrame {
                    native_subject: Some(subject),
                    session_id: row.id.clone(),
                    seq: 1,
                    event: Arc::new(event),
                    worker_generation: Some(old_epoch),
                    sidebar_status: Some(Status::Idle),
                })
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), gate.arrived)
                .await
                .expect("original tagged assignment reached listener")
                .unwrap();
            assert!(state
                .acp_supervisor
                .running_lease(&row.id)
                .is_some_and(|lease| lease.epoch() == old_epoch));

            let storage = Storage::open_unwatched("default").unwrap();
            let expected_status = if retire {
                let stopped = shutdown_acp(State(state.clone()), Path(row.id.clone()))
                    .await
                    .into_response();
                assert_eq!(stopped.status(), StatusCode::NO_CONTENT);
                let exit = tokio::time::timeout(Duration::from_secs(5), runner.wait())
                    .await
                    .expect("original runner retired")
                    .unwrap();
                assert!(!exit.success());
                assert!(!state.acp_supervisor.is_owned(&row.id).await);
                assert!(crate::process::worker_registry::load(&row.id)
                    .unwrap()
                    .is_none());
                Status::Stopped
            } else {
                // Idle retirement persists its marker before claiming the still-current lease.
                {
                    let _lifecycle = storage.acquire_instance_lifecycle_lock(&row.id).unwrap();
                    storage
                        .update(|rows, _| {
                            rows.iter_mut()
                                .find(|instance| instance.id == row.id)
                                .unwrap()
                                .mark_idle_dormant();
                            Ok(())
                        })
                        .unwrap();
                }
                state
                    .instances
                    .write()
                    .await
                    .iter_mut()
                    .find(|instance| instance.id == row.id)
                    .unwrap()
                    .mark_idle_dormant();
                assert!(state
                    .acp_supervisor
                    .running_lease(&row.id)
                    .is_some_and(|lease| lease.epoch() == old_epoch));
                assert!(runner.try_wait().unwrap().is_none());
                Status::Running
            };
            let committed = storage.load().unwrap().remove(0);
            assert_eq!(committed.status, expected_status);
            assert!(committed.is_idle_dormant());
            assert!(committed.lifecycle_reservation.is_none());
            let stop_generation = committed.lifecycle_generation;
            assert_eq!(committed.agent_session_id.as_deref(), Some(sid));
            assert_eq!(committed.agent_session_binding.as_ref(), Some(&binding));
            {
                let rows = state.instances.read().await;
                let cached_stop = rows.iter().find(|instance| instance.id == row.id).unwrap();
                assert_eq!(cached_stop.status, expected_status);
                assert!(cached_stop.is_idle_dormant());
                assert_eq!(cached_stop.lifecycle_generation, stop_generation);
            }

            gate.release.send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(5), gate.applied)
                .await
                .expect("assignment application and persistence finished")
                .unwrap();
            listener.abort();
            let _ = listener.await;
            let cached = state
                .instances
                .read()
                .await
                .iter()
                .find(|instance| instance.id == row.id)
                .unwrap()
                .clone();
            let durable = storage.load().unwrap().remove(0);
            for (source, retained) in [("cache", &cached), ("disk", &durable)] {
                assert_eq!(retained.status, expected_status,
                    "{source}, first_assignment={first_assignment}, retired={retire}: observed SID is not wake authority");
                assert!(retained.is_idle_dormant(),
                    "{source}, first_assignment={first_assignment}, retired={retire}: assignment consumed Stop/dormancy");
                assert_eq!(retained.lifecycle_generation, stop_generation);
                assert_eq!(retained.acp_session_id.as_deref(), Some(sid));
                assert_eq!(retained.agent_session_id.as_deref(), Some(sid));
                assert_eq!(retained.agent_session_binding.as_ref(), Some(&binding));
                assert_eq!(
                    retained
                        .agent_session_binding
                        .as_ref()
                        .unwrap()
                        .execution
                        .as_ref()
                        .unwrap()
                        .exported_default_store,
                    Some(false)
                );
            }

            let (resumed, reservation) =
                crate::server::api::sessions::resume_structured_stop(&state, cached, None)
                    .await
                    .unwrap_or_else(|response| {
                        panic!("manual resume failed: {}", response.status())
                    });
            assert_eq!(resumed.status, Status::Idle);
            assert!(!resumed.is_idle_dormant());
            if !retire {
                let exit = tokio::time::timeout(Duration::from_secs(5), runner.wait())
                    .await
                    .expect("explicit resume retired the dormant original")
                    .unwrap();
                assert!(!exit.success());
            }
            assert!(resumed.lifecycle_generation > stop_generation);
            let reservation =
                reservation.expect("retiring the dormant original reserves its explicit successor");
            assert_ne!(reservation.lease().epoch(), old_epoch);
            assert!(!state
                .acp_supervisor
                .test_cancel_requested(reservation.lease()));
            let mut child = crate::acp::supervisor::with_durable_launch(
                Some(crate::acp::supervisor::LaunchAdmission {
                    store: Arc::new(Storage::open_unwatched("default").unwrap()),
                    generation: resumed.lifecycle_generation,
                    namespace: None,
                    native_scope: None,
                }),
                row.id.clone(),
                || {
                    std::process::Command::new("true")
                        .spawn()
                        .map_err(|error| crate::acp::acp_client::AcpError::Spawn(error.to_string()))
                },
            )
            .await
            .expect("explicit resume passes the actual native birth gate");
            assert!(child.wait().unwrap().success());
            let after_resume = storage.load().unwrap().remove(0);
            assert_eq!(after_resume.agent_session_id.as_deref(), Some(sid));
            assert_eq!(after_resume.agent_session_binding.as_ref(), Some(&binding));
            drop(reservation);
            assert_eq!(
                state.acp_supervisor.worker_state(&row.id).await,
                crate::daemon::AcpWorkerState::Absent,
                "dropping an unpublished construction abandons its lease; no worker needs shutdown"
            );
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
        use crate::acp::supervisor::test_support::HeldAttachRunner;
        use crate::acp::Event;
        use crate::session::{Instance, Status, Storage, View};

        let _home = crate::session::test_support::isolate_app_dir();
        let project = tempfile::tempdir().unwrap();
        let mut row = Instance::new("restart original binding", project.path().to_str().unwrap());
        row.source_profile = "default".into();
        row.view = View::Structured;
        row.status = Status::Running;
        row.tool = "claude".into();
        row.agent_name = Some("claude".into());
        let id = row.id.clone();
        crate::server::test_support::seed_instances_on_disk_for_test("default", vec![row.clone()]);
        let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let mut runner = HeldAttachRunner::recording_prompt(
            &id,
            project.path(),
            17,
            Some("default"),
            "original-native-restart",
        )
        .await;
        runner.record.scope = crate::process::worker_registry::ScopeOwnership::Unscoped;
        crate::process::worker_registry::save(&runner.record).unwrap();
        let mut events = state.acp_events_tx.subscribe();
        state
            .acp_supervisor
            .attach(id.clone(), project.path().into(), Vec::new(), false, None)
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let frame = events.recv().await.unwrap();
                if matches!(frame.event.as_ref(), Event::AcpSessionAssigned {
                    acp_session_id, connection_ready: true, native_subject: Some(_),
                } if acp_session_id == "original-native-restart")
                {
                    break;
                }
                assert!(!matches!(
                    frame.event.as_ref(),
                    Event::AgentStartupError { .. }
                ));
            }
        })
        .await
        .unwrap();
        assert_eq!(runner.resumed_session().await, "original-native-restart");
        let original = state
            .acp_supervisor
            .current_native_availability(&id)
            .await
            .unwrap();
        assert_eq!(original.owner, Some(runner.identity));
        assert!(
            state.instances.read().await[0].acp_session_id.is_none(),
            "the original received binding has not been consumed by an event listener"
        );
        let snapshot = || {
            state
                .acp_event_store
                .replay_from(&id, 0)
                .into_iter()
                .map(|(seq, event)| {
                    (
                        seq,
                        event.native_subject(),
                        serde_json::to_value(event).unwrap(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let history = snapshot();
        let restart = state
            .acp_supervisor
            .try_begin_restart_with_reason(&id)
            .await
            .unwrap();
        if let Err(response) = retire_restart_worker(&state, &row, &restart).await {
            let status = response.status();
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_else(|error| format!("reading bounded response body failed: {error}"));
            panic!("restart retirement failed: {status}: {body}");
        }
        assert!(
            !crate::acp::runner_lifecycle::SystemProcessControl::default()
                .is_alive(runner.identity)
        );
        let memory = state.instances.read().await;
        assert_eq!(
            memory[0].acp_session_id.as_deref(),
            Some("original-native-restart")
        );
        drop(memory);
        let stored = Storage::open_reference("default")
            .unwrap()
            .load()
            .unwrap()
            .remove(0);
        assert_eq!(
            stored.acp_session_id.as_deref(),
            Some("original-native-restart")
        );
        assert!(snapshot().starts_with(&history));
        let reservation = state
            .acp_supervisor
            .reserve_spawn_with_restart(
                &id,
                crate::acp::supervisor::ResumeKind::Spawn,
                crate::process::worker_registry::ResumeAuthority::Explicit,
                Some(&restart),
            )
            .await
            .expect("the foreground checkpoint ACK releases actual replacement admission");
        assert!(state.acp_supervisor.current_lease(&id) == Some(reservation.lease().clone()));
        assert!(reservation.lease() != &original.lease);
        assert!(!restart.is_cancelled());
        drop(reservation);
        state.shutdown.cancel();
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
        for case in ["backend", "provider", "invalid_backend", "invalid_provider"] {
            let _home = crate::session::test_support::isolate_app_dir();
            let project = tempfile::tempdir().unwrap();
            let mut row = Instance::new(case, project.path().to_str().unwrap());
            row.source_profile = "default".into();
            row.view = View::Structured;
            row.status = Status::Stopped;
            row.tool = "claude".into();
            row.agent_name = Some("claude".into());
            row.agent_provider = Some("anthropic".into());
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
                        target: if case == "backend" {
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
                let provider = if case == "provider" {
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
            if case.starts_with("invalid") {
                assert_eq!(births.load(Ordering::SeqCst), 0);
                assert_eq!(stored.lifecycle_generation, observed);
                assert_eq!(stored.status, Status::Stopped);
                assert!(stored.is_idle_dormant());
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

        let restarting = {
            let state = Arc::clone(&state);
            let id = id.clone();
            tokio::spawn(async move {
                restart_acp(
                    State(state),
                    Path(id),
                    Ok(Json(SpawnAcpRequest {
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
        let _home = crate::session::test_support::isolate_app_dir();
        let mut instance =
            crate::session::Instance::new("restart-disconnect", "/tmp/aoe-restart-disconnect");
        instance.view = crate::session::View::Structured;
        instance.agent_name = Some("claude".into());
        instance.source_profile = "default".into();
        let id = instance.id.clone();
        crate::server::test_support::seed_instances_on_disk_for_test(
            "default",
            vec![instance.clone()],
        );
        let state = crate::server::test_support::build_test_app_state_with_launcher(
            vec![instance],
            crate::server::test_support::succeeding_launcher(),
        );
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        state.acp_supervisor.test_insert_worker(&id).await;
        let lock = state.instance_lock(&id).await;
        let guard = lock.lock().await;
        let request = {
            let state = Arc::clone(&state);
            let id = id.clone();
            tokio::spawn(async move {
                restart_acp(
                    State(state),
                    Path(id),
                    Ok(Json(SpawnAcpRequest {
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
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while state.acp_supervisor.worker_state(&id).await
                != crate::daemon::AcpWorkerState::Absent
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("old worker retires");
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        drop(guard);
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while state.acp_supervisor.worker_state(&id).await
                != crate::daemon::AcpWorkerState::Running
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("replacement runs after request cancellation");
        assert_eq!(
            shutdown_acp(State(Arc::clone(&state)), Path(id))
                .await
                .into_response()
                .status(),
            StatusCode::NO_CONTENT
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn restart_rechecks_session_and_cancellation_after_waiting_for_exclusion() {
        for (cancel, change_agent, change_view) in [
            (false, true, false),
            (true, false, false),
            (false, false, true),
        ] {
            let _home = crate::session::test_support::isolate_app_dir();
            let mut instance =
                crate::session::Instance::new("restart-exclusion", "/tmp/aoe-restart-exclusion");
            instance.view = crate::session::View::Structured;
            instance.agent_name = Some("claude".into());
            instance.source_profile = "default".into();
            let id = instance.id.clone();
            crate::server::test_support::seed_instances_on_disk_for_test(
                "default",
                vec![instance.clone()],
            );
            let state = crate::server::test_support::build_test_app_state_with_launcher(
                vec![instance],
                crate::server::test_support::succeeding_launcher(),
            );
            crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
            state.acp_supervisor.test_insert_worker(&id).await;
            let lock = state.instance_lock(&id).await;
            let guard = lock.lock().await;
            let mut claims = state.session_service.watch_submission_claims();
            let request = {
                let state = Arc::clone(&state);
                let id = id.clone();
                tokio::spawn(async move {
                    restart_acp(
                        State(state),
                        Path(id),
                        Ok(Json(SpawnAcpRequest {
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
                drop(guard);
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
            let _home = crate::session::test_support::isolate_app_dir();
            let mut row = crate::session::Instance::new("provider ordering", "/repo");
            row.source_profile = "default".into();
            row.view = crate::session::View::Structured;
            row.agent_name = Some("claude".into());
            row.agent_provider = Some("anthropic".into());
            row.agent_model = Some("old-provider-model".into());
            let id = row.id.clone();
            crate::server::test_support::seed_instances_on_disk_for_test(
                "default",
                vec![row.clone()],
            );
            let state = crate::server::test_support::build_test_app_state(vec![row]);
            crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
            let old_commands = state
                .acp_supervisor
                .test_insert_worker_cmd_recording(&id)
                .await;
            let switching = state
                .session_service
                .prompt_submission_for_session(&id)
                .await
                .unwrap();
            let mut claims = state.session_service.watch_submission_claims();
            let mut selecting = Box::pin(async {
                if legacy_mode {
                    acp_set_mode(
                        State(state.clone()),
                        Path(id.clone()),
                        Ok(Json(SetModeRequest {
                            mode_id: "plan".into(),
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
            state.acp_supervisor.test_flush_worker_commands(&id).await;
            assert!(
                old_commands.lock().unwrap().is_empty(),
                "no selection may reach the old provider"
            );
            state
                .acp_supervisor
                .shutdown_and_wait(&id, std::time::Duration::from_secs(5))
                .await
                .unwrap();
            let expected = find_instance(&state, &id).await.unwrap();
            let exclusion =
                crate::acp::sandbox::LaunchExclusion::acquire(state.clone(), &id, false)
                    .await
                    .unwrap();
            let (native, exclusion) = launch_store_for(&state, &expected, exclusion)
                .await
                .unwrap();
            persist_provider_switch(native, &expected, "vertex", exclusion)
                .await
                .unwrap();
            let new_commands = state
                .acp_supervisor
                .test_insert_worker_cmd_recording(&id)
                .await;
            drop(switching);
            let response = tokio::time::timeout(std::time::Duration::from_secs(5), selecting)
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            state.acp_supervisor.test_flush_worker_commands(&id).await;
            let command = if legacy_mode {
                "set_mode"
            } else {
                "set_config_option"
            };
            assert!(!old_commands.lock().unwrap().contains(&command));
            assert_eq!(*new_commands.lock().unwrap(), [command]);
            let live = find_instance(&state, &id).await.unwrap();
            let disk = crate::server::test_support::load_instances_from_disk_for_test("default");
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
            state
                .acp_supervisor
                .shutdown_and_wait(&id, std::time::Duration::from_secs(5))
                .await
                .unwrap();
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

    /// The target spawns, then the stored write fails on a valid row: the
    /// caller gets the 500, the new worker is stopped, nothing announces a
    /// switch, and neither store moves. Once storage recovers the retry lands.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_failed_agent_switch_write_stops_the_target_and_a_retry_recovers() {
        use crate::server::test_support as support;
        let profile = "default";
        let _tmp = crate::session::test_support::isolate_app_dir();
        let project = tempfile::tempdir().unwrap();
        let mut inst = crate::session::Instance::new("claude", project.path().to_str().unwrap());
        inst.view = crate::session::View::Structured;
        inst.status = crate::session::Status::Idle;
        inst.source_profile = profile.to_string();
        inst.agent_name = Some("claude".to_string());
        inst.acp_effort = Some("high".to_string());
        let id = inst.id.clone();
        support::seed_instances_on_disk_for_test(profile, vec![inst.clone()]);
        let state =
            support::build_test_app_state_with_launcher(vec![inst], support::succeeding_launcher());
        support::refresh_canonical_metadata_for_test(&state).await;
        state.acp_supervisor.test_insert_worker(&id).await;
        let switch = || async {
            switch_acp_agent(
                State(state.clone()),
                Path(id.clone()),
                Ok(Json(crate::acp::protocol::SwitchAgentRequest {
                    target: "codex".to_string(),
                    model: None,
                    reason: None,
                })),
            )
            .await
            .into_response()
        };
        let switched = || {
            state
                .acp_event_store
                .replay_from(&id, 0)
                .iter()
                .filter(|(_, event)| {
                    matches!(event, crate::acp::state::Event::AgentSwitched { .. })
                })
                .count()
        };

        let failure = support::fail_session_writes_for_test(profile);
        let resp = switch().await;
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&body);
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        assert_eq!(
            state.acp_supervisor.worker_state(&id).await,
            crate::daemon::AcpWorkerState::Absent,
            "the target worker is stopped"
        );
        assert_eq!(switched(), 0);
        assert_eq!(
            state
                .telemetry_structured
                .agent_switches
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        let memory = state.instances.read().await;
        assert_eq!(memory[0].agent_name.as_deref(), Some("claude"));
        assert_eq!(memory[0].acp_effort.as_deref(), Some("high"));
        drop(memory);
        let stored = support::load_instances_from_disk_for_test(profile);
        assert_eq!(stored[0].agent_name.as_deref(), Some("claude"));
        assert_eq!(stored[0].acp_effort.as_deref(), Some("high"));

        drop(failure);
        // The failed commit left the store degraded until the next reload.
        support::accept_runtime_read_cache_for_test(&state).await;
        let resp = switch().await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(switched(), 1);
        assert_eq!(
            state
                .telemetry_structured
                .agent_switches
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        let stored = support::load_instances_from_disk_for_test(profile);
        assert_eq!(stored[0].agent_name.as_deref(), Some("codex"));
        assert_eq!(stored[0].acp_effort, None);
        assert_eq!(
            state.instances.read().await[0].agent_name.as_deref(),
            Some("codex")
        );
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
        use crate::session::test_support::isolate_app_dir;
        let profile = "default";

        for (case, code) in [
            ("mid-turn", "turn_active"),
            ("unproven stop", "worker_not_stopped"),
        ] {
            let _tmp = isolate_app_dir();
            let mut inst = crate::session::Instance::new("claude", "/tmp/aoe-switch-provider-gate");
            inst.source_profile = profile.into();
            inst.view = crate::session::View::Structured;
            inst.agent_name = Some("claude".to_string());
            inst.agent_model = Some("claude-fable-5-1".to_string());
            let id = inst.id.clone();
            crate::server::test_support::seed_instances_on_disk_for_test(
                profile,
                vec![inst.clone()],
            );
            let state = crate::server::test_support::build_test_app_state(vec![inst]);
            if code == "turn_active" {
                state.acp_supervisor.test_insert_worker(&id).await;
                let prompt = crate::acp::state::Event::UserPromptSent {
                    prompt_id: None,
                    text: "still working".to_string(),
                    attachments: Vec::new(),
                    synthesized: false,
                    origin_plugin: None,
                };
                state
                    .acp_event_store
                    .record_at(&id, 1, &prompt, Utc::now().timestamp_millis())
                    .unwrap();
            } else {
                state.acp_supervisor.test_hold_stopping(&id);
            }

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
            if code == "turn_active" {
                assert!(
                    state.acp_supervisor.is_running(&id).await,
                    "{case}: the worker must keep its turn"
                );
            }
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
        use crate::acp::supervisor::test_support::HeldAttachRunner;
        use crate::acp::Event;
        use crate::session::{Instance, Status, Storage, View};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let _home = crate::session::test_support::isolate_app_dir();
        let project = tempfile::tempdir().unwrap();
        let mut row = Instance::new("manual rate-limit resume", project.path().to_str().unwrap());
        row.id = "sess-3514-manual-resume-already-running".into();
        row.source_profile = "default".into();
        row.status = Status::Running;
        row.view = View::Structured;
        row.tool = "claude".into();
        row.agent_name = Some("claude".into());
        row.acp_session_id = Some("same-native-manual-quota".into());
        let id = row.id.clone();
        let mut ordinary_row =
            Instance::new("ordinary running worker", project.path().to_str().unwrap());
        ordinary_row.id = "sess-ordinary-manual-resume-already-running".into();
        ordinary_row.source_profile = "default".into();
        ordinary_row.status = Status::Running;
        ordinary_row.view = View::Structured;
        ordinary_row.tool = "claude".into();
        ordinary_row.agent_name = Some("claude".into());
        let ordinary_id = ordinary_row.id.clone();
        crate::server::test_support::seed_instances_on_disk_for_test(
            "default",
            vec![row.clone(), ordinary_row.clone()],
        );

        let launches = Arc::new(AtomicUsize::new(0));
        let launcher: crate::acp::supervisor::Launcher = {
            let launches = Arc::clone(&launches);
            Arc::new(move |_, _| {
                launches.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {
                    Err(crate::acp::acp_client::AcpError::Spawn(
                        "an already-running worker must not be started again".into(),
                    ))
                })
            })
        };
        let state = crate::server::test_support::build_test_app_state_with_launcher(
            vec![row, ordinary_row],
            launcher,
        );
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let ordinary_commands = state
            .acp_supervisor
            .test_insert_worker_cmd_recording(&ordinary_id)
            .await;

        let resets_at = Utc::now() - chrono::Duration::minutes(30);
        let prompt = || crate::acp::Event::UserPromptSent {
            text: "continue the interrupted work".into(),
            attachments: Vec::new(),
            prompt_id: None,
            synthesized: false,
            origin_plugin: None,
        };
        let rate_limit = || crate::acp::Event::RateLimit {
            info: RateLimitInfo {
                status: "limited".into(),
                resets_at: Some(resets_at),
                kind: "usage".into(),
            },
        };
        let stopped = || crate::acp::Event::Stopped {
            reason: "rate_limited".into(),
        };
        let events = [
            prompt(),
            rate_limit(),
            stopped(),
            crate::acp::Event::RateLimitAutoResumed {
                resets_at,
                manual: false,
            },
            prompt(),
            rate_limit(),
            stopped(),
        ];
        let recorded_at_ms =
            Utc::now().timestamp_millis() - chrono::Duration::hours(1).num_milliseconds();
        for (index, event) in events.iter().enumerate() {
            state
                .acp_event_store
                .record_at(
                    &id,
                    u64::try_from(index).unwrap() + 1,
                    event,
                    recorded_at_ms,
                )
                .unwrap();
        }
        state
            .acp_supervisor
            .hydrate_seqs([(id.clone(), state.acp_event_store.highest_seq(&id))]);
        assert_eq!(state.acp_event_store.rate_limit_redelivery_streak(&id), 1);
        let park = state.acp_event_store.rate_limit_park(&id).unwrap();
        let mut runner = HeldAttachRunner::recording_prompt(
            &id,
            project.path(),
            17,
            Some("default"),
            "same-native-manual-quota",
        )
        .await;
        let mut observed = state.acp_events_tx.subscribe();
        state
            .acp_supervisor
            .attach(id.clone(), project.path().into(), Vec::new(), false, None)
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let frame = observed.recv().await.unwrap();
                if matches!(frame.event.as_ref(), Event::AcpSessionAssigned {
                    acp_session_id,
                    connection_ready: true,
                    ..
                } if acp_session_id == "same-native-manual-quota")
                {
                    break;
                }
                assert!(!matches!(
                    frame.event.as_ref(),
                    Event::AgentStartupError { .. }
                ));
            }
        })
        .await
        .unwrap();
        assert_eq!(runner.resumed_session().await, "same-native-manual-quota");
        let original = state
            .acp_supervisor
            .current_native_availability(&id)
            .await
            .unwrap();
        assert_eq!(original.owner, Some(runner.identity));
        assert_eq!(
            state.acp_event_store.rate_limit_park(&id).unwrap().seq,
            park.seq
        );
        let frontier = state.acp_event_store.highest_seq(&id);

        let request =
            || -> Result<Json<SpawnAcpRequest>, axum::extract::rejection::JsonRejection> {
                Ok(Json(SpawnAcpRequest {
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
        assert!(state.acp_supervisor.is_running(&id).await);
        assert_eq!(launches.load(Ordering::SeqCst), 0);
        let native_prompt =
            tokio::time::timeout(std::time::Duration::from_secs(10), runner.next_prompt())
                .await
                .unwrap();
        assert_eq!(native_prompt["sessionId"], "same-native-manual-quota");
        let native_text = native_prompt["prompt"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|block| block["type"] == "text")
            .map(|block| block["text"].as_str().unwrap())
            .collect::<Vec<_>>()
            .join("");
        assert_eq!(native_text, "continue the interrupted work");
        assert!(state.acp_supervisor.current_lease(&id) == Some(original.lease.clone()));
        assert_eq!(
            state
                .acp_supervisor
                .current_native_availability(&id)
                .await
                .unwrap()
                .owner,
            Some(runner.identity)
        );
        assert!(crate::process::worker_registry::is_record_live(
            &runner.record
        ));
        let memory = state.instances.read().await;
        let current = memory.iter().find(|row| row.id == id).unwrap();
        assert!(current.pending_initial_turn.is_none());
        assert_eq!(
            current.acp_session_id.as_deref(),
            Some("same-native-manual-quota")
        );
        drop(memory);
        let stored = Storage::open_unwatched("default")
            .unwrap()
            .load()
            .unwrap()
            .into_iter()
            .find(|instance| instance.id == id)
            .expect("session remains stored");
        assert!(stored.pending_initial_turn.is_none());
        assert_eq!(
            stored.acp_session_id.as_deref(),
            Some("same-native-manual-quota")
        );
        assert!(state.acp_event_store.rate_limit_park(&id).is_none());
        assert_eq!(state.acp_event_store.rate_limit_redelivery_streak(&id), 1);
        let delivered = state.acp_event_store.replay_from(&id, frontier);
        let breadcrumbs = delivered
            .iter()
            .filter_map(|(seq, event)| {
                matches!(event, Event::RateLimitAutoResumed { manual: true, .. }).then_some(*seq)
            })
            .collect::<Vec<_>>();
        let prompts = delivered
            .iter()
            .filter_map(|(seq, event)| match event {
                Event::UserPromptSent { text, .. } => Some((*seq, text.as_str())),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(breadcrumbs.len(), 1);
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].1, "continue the interrupted work");
        assert!(breadcrumbs[0] < prompts[0].0);

        let ordinary_response =
            spawn_acp(State(state.clone()), Path(ordinary_id.clone()), request())
                .await
                .into_response();
        assert_eq!(ordinary_response.status(), StatusCode::CONFLICT);
        assert!(state.acp_supervisor.is_running(&ordinary_id).await);
        assert!(state
            .acp_event_store
            .replay_from(&ordinary_id, 1)
            .is_empty());
        assert_eq!(launches.load(Ordering::SeqCst), 0);
        assert!(ordinary_commands.lock().unwrap().is_empty());
        runner.complete_turn();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let frame = observed.recv().await.unwrap();
                if matches!(frame.event.as_ref(), Event::Stopped { reason } if reason == "prompt_complete") {
                    break;
                }
            }
        })
        .await
        .unwrap();
        state.acp_supervisor.test_remove_worker(&id).await;
        original.client.shutdown().await.unwrap();
        state.acp_supervisor.test_remove_worker(&ordinary_id).await;
        state.shutdown.cancel();
    }
}
