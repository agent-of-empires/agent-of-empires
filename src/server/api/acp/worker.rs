//! Worker spawn, shutdown, and agent switching.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::acp::protocol::{
    SwitchAgentRequest, SwitchAgentResponse, SwitchProviderRequest, SwitchProviderResponse,
};
use crate::server::acp_reconciler::install_rate_limit_continuation;
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
    let exclusion =
        match crate::acp::sandbox::LaunchExclusion::acquire(state.clone(), &id, true).await {
            Ok(exclusion) => exclusion,
            Err(_) => return session_not_found(),
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
        return (StatusCode::CONFLICT, Json(serde_json::json!({
            "error": "creation_not_finalized", "message": "The structured session launch is not finalized",
        }))).into_response();
    }

    let instance =
        match crate::server::api::sessions::resume_structured_stop(&state, instance).await {
            Ok(instance) => instance,
            Err(response) => return response,
        };
    let explicit = req.agent.clone().or_else(|| instance.agent_name.clone());
    let agent = pick_agent(&state, &instance, explicit.as_deref()).await;
    let (native, exclusion) = match launch_store_for(&state, &instance, exclusion).await {
        Ok(store) => store,
        Err(response) => return response,
    };
    let (sandbox_info, mut exclusion) = match crate::acp::sandbox::ensure_container_for_session(
        native.clone(),
        instance.clone(),
        exclusion,
    )
    .await
    {
        Ok(info) => info,
        Err(e) => return launch_error_response("sandbox container ensure", &e),
    };
    let rate_limit_resume_resets_at = rate_limit_resume_probe(&state, &id).await;

    // Explicit resume has settled and consumed only the observed native stop.

    let request = SpawnRequest {
        launch_admission: Some(crate::acp::supervisor::LaunchAdmission {
            store: native,
            generation: instance.lifecycle_generation,
            namespace: Some(state.profile_namespace.clone()),
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
    let reservation = match state
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
        Ok(reservation) => Some(reservation),
        Err(SupervisorError::AlreadyRunning(_)) if rate_limit_resume_resets_at.is_some() => None,
        Err(e) => return supervisor_error_response("spawn failed", &e),
    };
    if rate_limit_resume_resets_at.is_some() {
        if let Some(submission) = exclusion.take_submission() {
            let _ = install_rate_limit_continuation(&state, &id, submission).await;
        }
    }
    drop(exclusion);
    if let Some(reservation) = reservation {
        if let Err(e) = state.acp_supervisor.spawn_inner(request, reservation).await {
            return supervisor_error_response("spawn failed", &e);
        }
    }
    if let Some(resets_at) = rate_limit_resume_resets_at {
        // The manual breadcrumb is the budget's disarm step, so it fires
        // whether or not a queued prompt superseded the continuation.
        state
            .acp_supervisor
            .publish_rate_limit_auto_resumed(&id, resets_at, true);
    }
    Json(SpawnAcpResponse {
        session_id: id,
        agent,
        status: "running",
    })
    .into_response()
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
            let _namespace = state.profile_namespace.read().await;
            if *state.canonical_health.read().await != crate::daemon::RuntimeHealth::Healthy {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            let Some(_submission) = state
                .session_service
                .prompt_submission_for_session(&id)
                .await
            else {
                return session_not_found();
            };
            let lock = state.instance_lock(&id).await;
            let _guard = lock.lock().await;
            let Some(instance) = find_instance(&state, &id).await else {
                return session_not_found();
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
                Ok(()) => StatusCode::NO_CONTENT.into_response(),
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
    Json(req): Json<SwitchAgentRequest>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    if let Some(resp) = cityhall_block(&state) {
        return resp;
    }
    let target = req.target.trim().to_string();
    if target.is_empty() {
        return (StatusCode::BAD_REQUEST, "target is required").into_response();
    }
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
    let instance =
        match crate::server::api::sessions::resume_structured_stop(&state, instance).await {
            Ok(instance) => instance,
            Err(response) => return response,
        };
    let (native, exclusion) = match launch_store_for(&state, &instance, exclusion).await {
        Ok(store) => store,
        Err(response) => return response,
    };
    let before_seq = state.acp_event_store.highest_seq(&id);

    if let Err(e) = state
        .acp_supervisor
        .shutdown_and_wait(&id, std::time::Duration::from_secs(5))
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("shutdown failed before agent switch: {e}"),
        )
            .into_response();
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
    let reservation = match state
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
    let instance =
        match crate::server::api::sessions::resume_structured_stop(&state, instance).await {
            Ok(instance) => instance,
            Err(response) => return response,
        };
    let (native, exclusion) = match launch_store_for(&state, &instance, exclusion).await {
        Ok(store) => store,
        Err(response) => return response,
    };

    if let Err(e) = state
        .acp_supervisor
        .shutdown_and_wait(&id, std::time::Duration::from_secs(5))
        .await
    {
        if matches!(e, SupervisorError::TeardownPending(_)) {
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
    // No provider mutation until the previous runner is proven absent.
    if state.acp_supervisor.worker_state(&id).await != crate::daemon::AcpWorkerState::Absent {
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
            Err(error) => return launch_error_response("persisting the provider switch", &error),
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
        }),
        ..spawn_request_for(&updated, agent, sandbox_info)
    };
    if let Some(resp) = refuse_if_stored_row_shelved(&state, &updated).await {
        return resp;
    }
    let reservation = match state
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
    };
    drop(exclusion);
    if let Err(e) = state.acp_supervisor.spawn_inner(request, reservation).await {
        return supervisor_error_response("spawn failed after provider switch", &e);
    }

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

    async fn durable_stop_state(row: crate::session::Instance) -> Arc<AppState> {
        crate::server::test_support::seed_instances_on_disk_for_test("default", vec![row.clone()]);
        let state = crate::server::test_support::build_test_app_state(vec![row]);
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        state
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
            &mut std::collections::HashSet::new(),
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

            let resumed = crate::server::api::sessions::resume_structured_stop(&state, stopped)
                .await
                .unwrap_or_else(|response| panic!("resume failed: {}", response.status()));
            assert_eq!(resumed.status, Status::Idle);
            assert!(!resumed.is_idle_dormant());
            let admission = crate::acp::supervisor::LaunchAdmission {
                store: Arc::new(Storage::open_unwatched("default").unwrap()),
                generation: resumed.lifecycle_generation,
                namespace: None,
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
        let response = crate::server::api::sessions::resume_structured_stop(&state, observed)
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
        let response = crate::server::api::sessions::resume_structured_stop(&state, newer)
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
                &mut std::collections::HashSet::new(),
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
                    Json(SwitchAgentRequest {
                        target: if case == "backend" {
                            "codex".into()
                        } else {
                            "not-a-registered-agent".into()
                        },
                        model: None,
                        reason: None,
                    }),
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
        inst.agent_name = Some("claude".to_string());
        inst.acp_effort = Some("high".to_string());
        let id = inst.id.clone();
        support::seed_instances_on_disk_for_test(profile, vec![inst.clone()]);
        let state =
            support::build_test_app_state_with_launcher(vec![inst], support::succeeding_launcher());
        state.acp_supervisor.test_insert_worker(&id).await;
        let switch = || async {
            switch_acp_agent(
                State(state.clone()),
                Path(id.clone()),
                Json(crate::acp::protocol::SwitchAgentRequest {
                    target: "codex".to_string(),
                    model: None,
                    reason: None,
                }),
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

        // Launch admission opens its lock file inside the directory; create it first.
        drop(
            crate::session::Storage::new_unwatched(profile)
                .unwrap()
                .acquire_instance_lifecycle_lock(&id)
                .unwrap(),
        );
        let lock = support::lock_profile_dir_for_test(profile);
        if !lock.blocks_writes() {
            return;
        }
        let resp = switch().await;
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8_lossy(&body);
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        assert!(
            body.contains("persist_failed") && body.contains("agent switch"),
            "{body}"
        );
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

        drop(lock);
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
}
