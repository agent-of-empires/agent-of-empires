//! The ensure-* lifecycle endpoints and terminal attach/kill.

use super::*;
use crate::session::SessionStore;

pub async fn stop_auxiliary(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<crate::daemon::StopAuxiliaryBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    if let Some(response) = crate::server::api::cityhall_block(&state) {
        return response;
    }
    let Json(target) = match body {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    match crate::server::pane::stop_native_auxiliary(
        &state,
        &id,
        crate::server::pane::AuxiliaryStopRequest::Target(target),
    )
    .await
    {
        Ok(cursor) => crate::server::runtime::mutation_response(&cursor, StatusCode::NO_CONTENT),
        Err(error) => {
            if let Some(response) = lifecycle_rejection(&state, &error) {
                return response;
            }
            if error.is::<crate::tmux::LegacyToolUnavailable>() {
                return (StatusCode::CONFLICT,
                    Json(serde_json::json!({"error":"legacy_tool_ownership_unavailable", "message":error.to_string()})))
                    .into_response();
            }
            if error.is::<crate::server::pane::AuxiliaryTargetUnavailable>() {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error":"auxiliary_target_unavailable"})),
                )
                    .into_response();
            }
            if error.is::<crate::session::NativeStoreUnavailable>() {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            tracing::warn!(target: "http.api.sessions", session = %id, %error, "auxiliary stop failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

pub async fn ensure_tool(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<crate::daemon::EnsureToolBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    if let Some(response) = crate::server::api::cityhall_block(&state) {
        return response;
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return rejection.into_response(),
    };
    let namespace = state.profile_namespace.read().await;
    if *state.canonical_health.read().await != crate::daemon::RuntimeHealth::Healthy {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let lock = state.instance_lock(&id).await;
    let guard = lock.lock().await;
    let Some(mut instance) = state
        .instances
        .read()
        .await
        .iter()
        .find(|row| row.id == id)
        .cloned()
    else {
        return crate::server::api::session_not_found();
    };
    let worker_state = state.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let native = crate::server::session_store::NativeSessionStore::open(
            worker_state.clone(),
            &instance.source_profile,
            None,
        )?;
        anyhow::ensure!(
            body.expected.as_ref().is_none_or(|expected| {
                expected.matches(&instance) && expected.matches_source(native.storage())
            }),
            crate::session::LifecycleReservationError::Superseded
        );
        let legacy_scope = if body.adoption.is_some() {
            let loaded = crate::server::reload::load_all_profiles(&worker_state.file_watch)
                .map_err(|error| error.source.context(crate::session::NativeStoreUnavailable))?;
            Some(crate::server::pane::legacy_tool_scope(
                &loaded.instances,
                &loaded.metadata.raw_tool_names,
            ))
        } else {
            None
        };
        let (tool, created) = instance.start_tool_with_size_in(
            &body.tool_name,
            body.size.map(|size| (size.cols.get(), size.rows.get())),
            &native,
            body.adoption.as_ref(),
            legacy_scope.as_ref(),
        )?;
        Ok((tool, created, instance, body.tool_name, native))
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result);
    drop(guard);
    drop(namespace);
    let result = match result {
        Ok((tool, true, instance, tool_name, native)) => {
            tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                tool.wait_until_ready()?;
                Ok((tool, true, instance, tool_name, native))
            })
            .await
            .map_err(anyhow::Error::from)
            .and_then(|result| result)
        }
        Ok(result) => Ok(result),
        Err(error) => Err(error),
    };
    let result = match result {
        Ok((_tool, created, instance, tool_name, native)) => {
            crate::server::pane::publish_auxiliary_after_ensure(
                &state,
                instance,
                crate::session::AuxiliaryTarget::Tool { tool_name },
                created,
                native,
            )
            .await
        }
        Err(error) => Err(error),
    };
    let receipt = match result {
        Ok(tool) => tool,
        Err(error) => {
            if let Some(response) = lifecycle_rejection(&state, &error) {
                return response;
            }
            if error.is::<crate::session::NativeStoreUnavailable>() {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            if error.is::<crate::server::pane::AuxiliaryTargetUnavailable>() {
                return StatusCode::CONFLICT.into_response();
            }
            if error.is::<crate::tmux::LegacyToolUnavailable>() {
                return (StatusCode::CONFLICT,
                    Json(serde_json::json!({"error":"legacy_tool_ownership_unavailable", "message":error.to_string()})))
                    .into_response();
            }
            if error.is::<crate::session::ToolLaunchUnavailable>() {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "tool_unavailable"})),
                )
                    .into_response();
            }
            tracing::warn!(target: "http.api.sessions", session = %id, %error, "tool ensure failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    crate::server::runtime::mutation_response(&receipt.cursor, Json(receipt.outcome))
}

pub async fn ensure_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<
        Option<Json<crate::daemon::StartSessionBody>>,
        axum::extract::rejection::JsonRejection,
    >,
) -> impl IntoResponse {
    super::lifecycle::prepare_agent_session(
        state,
        id,
        body,
        super::lifecycle::AgentPreparation::Ensure,
        None,
    )
    .await
}

pub(super) fn physical_agent_target(
    instance: &Instance,
) -> anyhow::Result<Option<(String, Arc<crate::session::LegacyToolObservation>)>> {
    let panes = crate::tmux::batch_pane_metadata()?;
    let Some((name, pane)) =
        crate::tmux::agent_pane_metadata_in(&panes, &instance.id, &instance.title)?
    else {
        return Ok(None);
    };
    if pane.pane_dead {
        return Ok(None);
    }
    let physical = pane
        .physical_observation()
        .ok_or(crate::session::LaunchIndeterminateCause::OwnershipUnproven)?;
    Ok(Some((name.to_owned(), physical)))
}

#[derive(Debug, thiserror::Error)]
#[error("Agent pane is not ready")]
struct AgentTargetUnavailable;

pub(super) fn indeterminate_launch_response(
    cause: crate::session::LaunchIndeterminateCause,
) -> axum::response::Response {
    (
        StatusCode::CONFLICT,
        crate::daemon::ApiErrorCode::LaunchIndeterminate.header(),
        Json(serde_json::json!({
            "error": "launch_indeterminate", "cause": cause.as_str(), "message": cause.message(),
        })),
    )
        .into_response()
}

pub(super) struct PreparedAgentTarget {
    pub(super) created_at: chrono::DateTime<chrono::Utc>,
    pub(super) target: crate::daemon::TerminalTarget,
    pub(super) native: crate::server::session_store::NativeSessionStore,
}

impl PreparedAgentTarget {
    pub(super) fn captured(
        created_at: chrono::DateTime<chrono::Utc>,
        generation: u64,
        profile: String,
        pane: crate::session::PaneObservation,
        status: crate::daemon::TerminalTargetStatus,
        native: crate::server::session_store::NativeSessionStore,
    ) -> Option<Self> {
        if pane.state != crate::session::PanePresence::Alive || pane.legacy_tool.is_some() {
            return None;
        }
        Some(Self {
            created_at,
            native,
            target: crate::daemon::TerminalTarget {
                tmux_session: pane.tmux_session?,
                physical: Some(pane.physical?),
                status,
                lifecycle_generation: generation,
                profile,
            },
        })
    }
}

pub(super) async fn agent_target_response(
    state: &Arc<AppState>,
    id: &str,
    outcome: Option<crate::session::StartOutcome>,
) -> axum::response::Response {
    if let Some(crate::session::StartOutcome::Indeterminate { cause }) = outcome {
        return indeterminate_launch_response(cause);
    }
    let Some(mut instance) = state
        .instances
        .read()
        .await
        .iter()
        .find(|row| row.id == id)
        .cloned()
    else {
        return crate::server::api::session_not_found();
    };
    if instance.is_structured() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let namespace = state.profile_namespace.read().await;
    let lock = state.instance_lock(id).await;
    let guard = lock.lock().await;
    let worker_state = state.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        use crate::session::SessionStore;
        let native = crate::server::session_store::NativeSessionStore::open(
            worker_state,
            &instance.source_profile,
            None,
        )?;
        let generation = instance.lifecycle_generation;
        let title = instance.title.clone();
        let _ownership = instance.acquire_auxiliary_locks_in(&native)?;
        anyhow::ensure!(
            instance.lifecycle_generation == generation && instance.title == title,
            crate::session::LifecycleReservationError::Superseded
        );
        anyhow::ensure!(!instance.is_structured(), AgentTargetUnavailable);
        native.configuration(Some(&instance.source_profile))?;
        let (name, physical) = physical_agent_target(&instance)?.ok_or(AgentTargetUnavailable)?;
        native.adopt_agent_observation(
            &instance,
            crate::session::PaneObservation {
                physical: Some(physical.clone()),
                state: crate::session::PanePresence::Alive,
                tmux_session: Some(name.clone()),
                legacy_tool: None,
            },
        )?;
        Ok(PreparedAgentTarget {
            created_at: instance.created_at,
            target: crate::daemon::TerminalTarget {
                tmux_session: name,
                physical: Some(physical),
                lifecycle_generation: instance.lifecycle_generation,
                profile: instance.source_profile,
                status: crate::daemon::TerminalTargetStatus::Alive,
            },
            native,
        })
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result);
    let prepared = match result {
        Ok(target) => target,
        Err(error) => return agent_target_error(state, error),
    };
    drop(guard);
    drop(namespace);
    prepared_agent_target_response(state, id, outcome, Some(prepared)).await
}

/// Publish only the exact target captured by this operation under ownership.
pub(super) async fn prepared_agent_target_response(
    state: &Arc<AppState>,
    id: &str,
    outcome: Option<crate::session::StartOutcome>,
    prepared: Option<PreparedAgentTarget>,
) -> axum::response::Response {
    if let Some(crate::session::StartOutcome::Indeterminate { cause }) = outcome {
        return indeterminate_launch_response(cause);
    }
    let Some(PreparedAgentTarget {
        created_at,
        target,
        native,
    }) = prepared
    else {
        return agent_target_error(state, AgentTargetUnavailable.into());
    };
    if let Err(error) = native.storage().verify_bound_profile() {
        return agent_target_error(
            state,
            error.context(crate::session::LifecycleReservationError::Superseded),
        );
    }
    let snapshot = match state.runtime.publish(state).await {
        Ok(snapshot) => snapshot,
        Err(error) => return agent_target_error(state, error),
    };
    if let Err(error) = native.storage().verify_bound_profile() {
        return agent_target_error(
            state,
            error.context(crate::session::LifecycleReservationError::Superseded),
        );
    }
    if snapshot.value.contents.health != crate::daemon::RuntimeHealth::Healthy {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if !snapshot.value.contents.sessions.iter().any(|row| {
        row.id == id
            && row.created_at.parse::<chrono::DateTime<chrono::Utc>>().ok() == Some(created_at)
            && row.lifecycle_generation == target.lifecycle_generation
            && row.profile == target.profile
            && row.agent_pane.physical == target.physical
    }) {
        return agent_target_error(
            state,
            crate::session::LifecycleReservationError::Superseded.into(),
        );
    }
    let mut body = serde_json::json!({
        "lifecycle_generation": target.lifecycle_generation,
        "profile": target.profile,
        "tmux_session": target.tmux_session,
        "physical": target.physical,
        "status": if outcome.is_some() { "restarted" } else { "alive" },
    });
    if let Some(outcome) = outcome {
        body["resume_outcome"] = match outcome {
            crate::session::StartOutcome::Resumed => "resumed",
            crate::session::StartOutcome::Fresh => "fresh",
            crate::session::StartOutcome::ResumeFailed { .. } => "resume_failed",
            crate::session::StartOutcome::Indeterminate { .. } => unreachable!("indeterminate launch cannot produce a ready target"),
            crate::session::StartOutcome::FreshAfterFailedResume { sid } => {
                body["message"] = format!("Started fresh; the prior conversation {sid} remains available in the agent's history.").into();
                body["prior_session_id"] = sid.into();
                "fresh_after_failed_resume"
            }
        }
        .into();
    }
    crate::server::runtime::mutation_response(&snapshot.value.cursor, Json(body))
}

fn agent_target_error(state: &AppState, error: anyhow::Error) -> axum::response::Response {
    if let Some(response) = lifecycle_rejection(state, &error) {
        return response;
    }
    if error.is::<crate::session::NativeStoreUnavailable>() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if error.is::<AgentTargetUnavailable>() {
        return if state.read_only {
            crate::server::api::read_only_response()
        } else {
            (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error":"agent_not_ready"})),
            )
                .into_response()
        };
    }
    tracing::warn!(target: "http.api.sessions", %error, "agent preparation failed");
    StatusCode::INTERNAL_SERVER_ERROR.into_response()
}

pub async fn ensure_terminal(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<crate::server::live_ws::TerminalIndexQuery>,
    body: Result<
        Option<Json<crate::daemon::StartSessionBody>>,
        axum::extract::rejection::JsonRejection,
    >,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    if let Some(response) = crate::server::api::cityhall_block(&state) {
        return response;
    }
    let body = match body {
        Ok(body) => body.map(|Json(body)| body).unwrap_or_default(),
        Err(rejection) => return rejection.into_response(),
    };
    let index = query.index;
    if index > crate::server::pane::MAX_TERMINAL_INDEX {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "index_out_of_range"})),
        )
            .into_response();
    }
    let namespace = state.profile_namespace.read().await;
    if *state.canonical_health.read().await != crate::daemon::RuntimeHealth::Healthy {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let lock = state.instance_lock(&id).await;
    let guard = lock.lock().await;
    let Some(mut instance) = state
        .instances
        .read()
        .await
        .iter()
        .find(|row| row.id == id)
        .cloned()
    else {
        return crate::server::api::session_not_found();
    };
    let worker_state = state.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let native = crate::server::session_store::NativeSessionStore::open(
            worker_state,
            &instance.source_profile,
            None,
        )?;
        anyhow::ensure!(
            body.expected.as_ref().is_none_or(|expected| {
                expected.matches(&instance) && expected.matches_source(native.storage())
            }),
            crate::session::LifecycleReservationError::Superseded
        );
        let (_, created) = instance.start_terminal_with_size_indexed_in(
            index,
            body.size.map(|size| (size.cols.get(), size.rows.get())),
            &native,
        )?;
        Ok((created, instance, native))
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result);
    drop(guard);
    drop(namespace);
    let result = match result {
        Ok((created, instance, native)) => {
            crate::server::pane::publish_auxiliary_after_ensure(
                &state,
                instance,
                crate::session::AuxiliaryTarget::Host { index },
                created,
                native,
            )
            .await
        }
        Err(error) => Err(error),
    };
    let receipt = match result {
        Ok(result) => result,
        Err(error) => {
            if let Some(response) = lifecycle_rejection(&state, &error) {
                return response;
            }
            if error.is::<crate::session::NativeStoreUnavailable>() {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            if error.is::<crate::server::pane::AuxiliaryTargetUnavailable>() {
                return StatusCode::CONFLICT.into_response();
            }
            tracing::warn!(target: "http.api.sessions", session = %id, %error, "terminal ensure failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    crate::server::runtime::mutation_response(
        &receipt.cursor,
        (
            if matches!(
                receipt.outcome.status,
                crate::daemon::TerminalTargetStatus::Created
            ) {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            },
            Json(receipt.outcome),
        ),
    )
}

pub async fn ensure_container_terminal(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<crate::server::live_ws::TerminalIndexQuery>,
    body: Result<
        Option<Json<crate::daemon::StartSessionBody>>,
        axum::extract::rejection::JsonRejection,
    >,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    if let Some(response) = crate::server::api::cityhall_block(&state) {
        return response;
    }
    if q.index > crate::server::pane::MAX_TERMINAL_INDEX {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "index_out_of_range"})),
        )
            .into_response();
    }
    let body = match body {
        Ok(body) => body.map(|Json(body)| body).unwrap_or_default(),
        Err(error) => return error.into_response(),
    };
    let namespace = state.profile_namespace.read().await;
    if *state.canonical_health.read().await != crate::daemon::RuntimeHealth::Healthy {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let lock = state.instance_lock(&id).await;
    let guard = lock.lock().await;
    if !state.instances.read().await.iter().any(|row| row.id == id) {
        return crate::server::api::session_not_found();
    }
    let size = body.size.map(|size| (size.cols.get(), size.rows.get()));
    drop(guard);
    drop(namespace);
    match crate::server::pane::ensure_native_container_terminal(
        &state,
        &id,
        q.index,
        size,
        body.expected,
    )
    .await
    {
        Ok((target, cursor)) => {
            let status = if matches!(&target.status, crate::daemon::TerminalTargetStatus::Created) {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            crate::server::runtime::mutation_response(&cursor, (status, Json(target)))
        }
        Err(error) => {
            if let Some(response) = lifecycle_rejection(&state, &error) {
                return response;
            }
            if error.is::<crate::session::NativeStoreUnavailable>() {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            tracing::warn!(target: "http.api.sessions", session = %id, %error, "container terminal ensure failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Kill an additional paired terminal (host + container) at `index`. Used when
/// the web dashboard closes an extra terminal tab so its tmux shell does not
/// leak for the session's lifetime. Index 0 is the primary terminal shared with
/// the native TUI; closing it in the web UI only hides the pane (the TUI keeps
/// its shell), so this endpoint rejects index 0. See #2437.
pub async fn kill_terminal(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    axum::extract::Query(q): axum::extract::Query<crate::server::live_ws::TerminalIndexQuery>,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    if let Some(resp) = crate::server::api::cityhall_block(&state) {
        return resp;
    }
    let index = q.index;
    if index == 0 || index > crate::server::pane::MAX_TERMINAL_INDEX {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "index_out_of_range"})),
        )
            .into_response();
    }
    let namespace = state.profile_namespace.read().await;
    if *state.canonical_health.read().await != crate::daemon::RuntimeHealth::Healthy {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let lock = state.instance_lock(&id).await;
    let guard = lock.lock().await;
    if !state.instances.read().await.iter().any(|row| row.id == id) {
        return crate::server::api::session_not_found();
    }
    drop(guard);
    drop(namespace);
    match crate::server::pane::stop_native_auxiliary(
        &state,
        &id,
        crate::server::pane::AuxiliaryStopRequest::PairedTerminals { index },
    )
    .await
    {
        Ok(cursor) => {
            tracing::info!(target: "session.lifecycle", session = %id, index, "terminal killed");
            crate::server::runtime::mutation_response(
                &cursor,
                (
                    StatusCode::OK,
                    Json(serde_json::json!({"status": "killed"})),
                ),
            )
        }
        Err(error) => {
            if let Some(response) = lifecycle_rejection(&state, &error) {
                return response;
            }
            if error.is::<crate::session::NativeStoreUnavailable>() {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            tracing::error!(target: "http.api.sessions", session = %id, %error, "Terminal kill failed");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": "kill_failed", "message": "Failed to kill terminal"}))).into_response()
        }
    }
}

#[cfg(test)]
mod indeterminate_tests {
    use super::*;

    #[tokio::test]
    #[serial_test::serial]
    async fn indeterminate_launch_never_returns_a_ready_target() {
        let _home = crate::session::test_support::isolate_app_dir();
        let row = Instance::new("uncertain", "/tmp");
        let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
        for cause in [
            crate::session::LaunchIndeterminateCause::CommandTimedOut,
            crate::session::LaunchIndeterminateCause::ObservationFailed,
            crate::session::LaunchIndeterminateCause::StabilizationExpired,
            crate::session::LaunchIndeterminateCause::OwnershipUnproven,
        ] {
            let response = agent_target_response(
                &state,
                &row.id,
                Some(crate::session::StartOutcome::Indeterminate { cause }),
            )
            .await;
            assert_eq!(response.status(), StatusCode::CONFLICT);
            assert_eq!(
                response.headers()[crate::daemon::ERROR_CODE_HEADER],
                "launch_indeterminate"
            );
            let body = axum::body::to_bytes(response.into_body(), 4096)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["cause"], cause.as_str());
            assert!(body.get("tmux_session").is_none());
            assert!(body.get("resume_outcome").is_none());
            let rows = state.instances.read().await;
            assert_eq!(rows[0].lifecycle_generation, row.lifecycle_generation);
            assert_eq!(rows[0].conversation_state(), row.conversation_state());
            assert_eq!(rows[0].resume_probe_failed_sid, None);
        }
    }
}
