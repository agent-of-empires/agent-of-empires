//! Worker spawn, shutdown, and agent switching.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::acp::protocol::{SwitchAgentRequest, SwitchAgentResponse};
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
    // Submission authority before `instance_lock`, as the permanent DELETE
    // path takes them (#4092); the claim also proves the session exists.
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return session_not_found();
    };
    let inst_lock = state.instance_lock(&id).await;
    let _guard = inst_lock.lock().await;
    let Some(instance) = find_instance(&state, &id).await else {
        return session_not_found();
    };
    if !instance.is_structured() {
        return not_structured_response();
    }
    if let Err(blocked) = instance.ensure_startable() {
        return crate::server::api::start_blocked_response(blocked);
    }

    let origin = match state.capture_operation_origin(&instance) {
        Ok(origin) => origin,
        Err(error) => {
            return (
                StatusCode::CONFLICT,
                format!("original launch authority is no longer valid: {error}"),
            )
                .into_response()
        }
    };
    let reservation = match state
        .acp_supervisor
        .begin_resume(
            &id,
            crate::acp::runner_lifecycle::NativeResume::Spawn,
            origin.clone(),
            true,
        )
        .await
    {
        Ok(crate::acp::supervisor::ResumeReservationOutcome::Reserved(reservation)) => reservation,
        Ok(crate::acp::supervisor::ResumeReservationOutcome::AlreadyPresent) => {
            return supervisor_error_response("spawn failed", &SupervisorError::AlreadyRunning(id))
        }
        Err(error) => return supervisor_error_response("spawn failed", &error),
    };
    let _body_custody = reservation.execution_admission().begin_job();
    let explicit = req.agent.clone().or_else(|| instance.agent_name.clone());
    let agent = pick_agent(&state, &instance, explicit.as_deref()).await;
    let sandbox_info = match crate::acp::sandbox::ensure_container_for_session_locked(
        &state.instances,
        reservation.execution_admission(),
        false,
    )
    .await
    {
        Ok(info) => info,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("sandbox container ensure failed: {e}"),
            )
                .into_response();
        }
    };
    let rate_limit_resume_resets_at = rate_limit_resume_probe(&state, &id).await;

    let request = SpawnRequest {
        additional_dirs: req.additional_dirs,
        provider_env: req
            .provider_env
            .into_iter()
            .map(|p| (p.key, p.value))
            .collect(),
        model: req.model.or_else(|| instance.agent_model.clone()),
        ..spawn_request_for(&instance, agent.clone(), sandbox_info, origin)
    };
    match state.acp_supervisor.spawn_inner(request, reservation).await {
        Ok(()) => {}
        Err(SupervisorError::AlreadyRunning(_)) if rate_limit_resume_resets_at.is_some() => {}
        Err(e) => return supervisor_error_response("spawn failed", &e),
    }
    if let Some(resets_at) = rate_limit_resume_resets_at {
        // Continue the rate-limit-interrupted turn once the worker is live.
        let _ = install_rate_limit_continuation(&state, &id, _submission).await;
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
    // Worker-stopping barrier (#3650): wait out any in-flight submission.
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return session_not_found();
    };
    let Some(instance) = crate::server::api::find_instance(&state, &id).await else {
        return session_not_found();
    };
    let original = match state.capture_operation_origin(&instance) {
        Ok(original) => original,
        Err(error) => return (StatusCode::CONFLICT, error.to_string()).into_response(),
    };
    let stop = match crate::session::runner_journal::reserve_stop_from_origin(original, false) {
        Ok(stop) => stop,
        Err(error) => return (StatusCode::CONFLICT, error.to_string()).into_response(),
    };
    match state.acp_supervisor.shutdown(stop.clone()).await {
        Ok(()) => match crate::session::runner_journal::release_owned_stop(&stop) {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(error) => (StatusCode::CONFLICT, error.to_string()).into_response(),
        },
        Err(e) => supervisor_error_response("shutdown failed", &e),
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

/// Record the new backend in memory and on disk from one mutation, so the two
/// cannot drift. Nothing adapter-specific survives the change: the ACP session
/// id, the pending import, the effort pick and the model are the new agent's to
/// resolve, which is the rule `Instance::swap_tool` already applies.
async fn persist_agent_switch(
    state: &AppState,
    issuance: crate::acp::runner_lifecycle::ExecutionAdmission,
    cached_original: Arc<crate::session::LaunchOrigin>,
    target: &str,
    model: Option<&str>,
) -> Result<(), Response> {
    let original = issuance.origin().ok_or_else(|| {
        (
            StatusCode::CONFLICT,
            "backend switch lost its issued original",
        )
            .into_response()
    })?;
    let mut instances = state.instances.write().await;
    let slot = instances
        .iter_mut()
        .find(|row| row.id == original.session_id())
        .ok_or_else(session_not_found)?;
    let cached = crate::session::LaunchOrigin::capture(slot)
        .map_err(|error| (StatusCode::CONFLICT, error.to_string()).into_response())?;
    if !cached_original.recognizes_published_snapshot(&cached)
        && !original.recognizes_published_snapshot(&cached)
    {
        return Err((
            StatusCode::CONFLICT,
            "backend switch cache original was superseded",
        )
            .into_response());
    }
    let target = target.to_owned();
    let model = model.map(str::to_owned);
    let custody = issuance.begin_job();
    let emitted = tokio::task::spawn_blocking(move || {
        let _custody = custody;
        original.update_storage(|_, row| {
            issuance.commit_effect(|| {
                row.agent_name = Some(target);
                row.acp_session_id = None;
                row.import_pending = None;
                row.acp_effort = None;
                row.agent_model = model;
                Ok(row.clone())
            })
        })
    })
    .await
    .map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("original backend metadata job failed: {error}"),
        )
            .into_response()
    })?
    .map_err(|error| {
        (
            StatusCode::CONFLICT,
            format!("original backend metadata was superseded: {error}"),
        )
            .into_response()
    })?;
    let mut emitted = emitted;
    emitted.storage_origin = slot.storage_origin.clone();
    *slot = crate::server::reload::merge_runtime_fields(slot, emitted);
    Ok(())
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
    // Worker-stopping barrier (#3650).
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return session_not_found();
    };
    // Custom agents are profile-specific, so the instance is needed to validate.
    let Some(instance) = find_instance(&state, &id).await else {
        return session_not_found();
    };
    if let Err(blocked) = instance.ensure_startable() {
        return crate::server::api::start_blocked_response(blocked);
    }
    let origin = match state.capture_operation_origin(&instance) {
        Ok(origin) => origin,
        Err(error) => {
            return (
                StatusCode::CONFLICT,
                format!("original switch authority is no longer valid: {error}"),
            )
                .into_response()
        }
    };
    let cached_original = origin.clone();
    let from_agent = match check_switch_target(&state, &instance, &target).await {
        Ok(agent) => agent,
        Err(resp) => return resp,
    };
    let before_seq = state.acp_event_store.highest_seq(&id);
    let stop = match crate::session::runner_journal::reserve_stop_from_origin(origin, false) {
        Ok(stop) => stop,
        Err(error) => return (StatusCode::CONFLICT, error.to_string()).into_response(),
    };

    if let Err(e) = state
        .acp_supervisor
        .shutdown_and_wait(stop.clone(), std::time::Duration::from_secs(5))
        .await
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("shutdown failed before agent switch: {e}"),
        )
            .into_response();
    }
    let mut rows = state.instances.clone().write_owned().await;
    let publication_stop = stop.clone();
    let publication_baseline = cached_original.clone();
    if let Err(error) = tokio::task::spawn_blocking(move || {
        publication_stop.with_scope(|stored| {
            let slot = rows
                .iter_mut()
                .find(|row| row.id == publication_stop.session_id())
                .ok_or_else(|| anyhow::anyhow!("backend switch original view row disappeared"))?;
            let cached = crate::session::LaunchOrigin::capture(slot)?;
            anyhow::ensure!(
                publication_baseline.recognizes_published_snapshot(&cached)
                    || publication_stop
                        .current_projection()
                        .recognizes_published_snapshot(&cached),
                "backend switch original view row was superseded before its Stop ACK"
            );
            *slot = crate::server::reload::merge_runtime_fields(slot, stored.clone());
            slot.acp_load_session_capable = None;
            Ok(())
        })
    })
    .await
    .unwrap_or_else(|error| {
        Err(anyhow::anyhow!(
            "original backend Stop publication failed: {error}"
        ))
    }) {
        return (StatusCode::CONFLICT, error.to_string()).into_response();
    }
    let origin = stop.cancellation_origin();
    if let Err(error) = crate::session::runner_journal::release_owned_stop(&stop) {
        return (StatusCode::CONFLICT, error.to_string()).into_response();
    }
    let reservation = match state
        .acp_supervisor
        .begin_resume(
            &id,
            crate::acp::runner_lifecycle::NativeResume::Spawn,
            origin,
            true,
        )
        .await
    {
        Ok(crate::acp::supervisor::ResumeReservationOutcome::Reserved(reservation)) => reservation,
        Ok(crate::acp::supervisor::ResumeReservationOutcome::AlreadyPresent) => {
            return (StatusCode::CONFLICT, "backend switch was superseded").into_response()
        }
        Err(error) => {
            return supervisor_error_response("backend switch preparation failed", &error)
        }
    };
    let issuance = reservation.execution_admission();
    let _body_custody = issuance.begin_job();

    let inst_lock = state.instance_lock(&id).await;
    let sandbox_info = match crate::acp::sandbox::ensure_container_for_session(
        &state.instances,
        &inst_lock,
        issuance.clone(),
        false,
    )
    .await
    {
        Ok(info) => info,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("sandbox container ensure failed: {e}"),
            )
                .into_response();
        }
    };

    let model = req.model.clone();
    // A new backend starts a fresh session. Effort vocabularies are
    // adapter-specific, so the old pick is dropped too.
    let request = SpawnRequest {
        model: model.clone(),
        effort: None,
        effort_explicit: false,
        stored_acp_session_id: None,
        fork_from: None,
        seed_history_replay: false,
        sandbox_continuation: crate::acp::supervisor::SandboxContinuation::Fresh,
        claude_store_pin: None,
        ..spawn_request_for(
            &instance,
            target.clone(),
            sandbox_info,
            issuance
                .origin()
                .expect("prepared backend switch owns its source"),
        )
    };
    if let Err(e) = state.acp_supervisor.spawn_inner(request, reservation).await {
        return supervisor_error_response("spawn failed", &e);
    }
    state
        .telemetry_structured
        .agent_switches
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if let Err(response) =
        persist_agent_switch(&state, issuance, cached_original, &target, model.as_deref()).await
    {
        return response;
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::agent_policy::AgentPolicy;
    use crate::acp::state::RateLimitInfo;

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

            let original = crate::session::runner_journal::capture_unique_origin(&id).unwrap();
            let issuance = crate::acp::runner_lifecycle::ExecutionAdmission::new();
            issuance.set_origin(original.clone()).unwrap();
            persist_agent_switch(&state, issuance, original, "codex", requested)
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
