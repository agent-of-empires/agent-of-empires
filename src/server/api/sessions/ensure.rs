//! The ensure-* lifecycle endpoints and terminal attach/kill.

use super::*;

pub async fn stop_auxiliary(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<crate::session::AuxiliaryTarget>, axum::extract::rejection::JsonRejection>,
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
            worker_state,
            &instance.source_profile,
            None,
        )?;
        let (tool, created) = instance.start_tool_with_size_in(
            &body.tool_name,
            body.size.map(|size| (size.cols.get(), size.rows.get())),
            &native,
        )?;
        Ok((tool, created, instance, body.tool_name))
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result);
    drop(guard);
    drop(namespace);
    let result = match result {
        Ok((tool, true, instance, tool_name)) => {
            tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                tool.wait_until_ready()?;
                Ok((tool, true, instance, tool_name))
            })
            .await
            .map_err(anyhow::Error::from)
            .and_then(|result| result)
        }
        Ok(result) => Ok(result),
        Err(error) => Err(error),
    };
    let result = match result {
        Ok((tool, created, instance, tool_name)) => {
            let identity = (
                instance.lifecycle_generation,
                instance.source_profile.clone(),
            );
            crate::server::pane::publish_auxiliary_after_ensure(
                &state,
                instance,
                crate::session::AuxiliaryTarget::Tool { tool_name },
            )
            .await
            .map(|cursor| (tool, created, cursor, identity))
        }
        Err(error) => Err(error),
    };
    let (tool, created, cursor, (lifecycle_generation, profile)) = match result {
        Ok(tool) => tool,
        Err(error) => {
            if let Some(response) = lifecycle_rejection(&state, &error) {
                return response;
            }
            if error.is::<crate::session::NativeStoreUnavailable>() {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
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
    crate::server::runtime::mutation_response(
        &cursor,
        Json(crate::daemon::TerminalTarget {
            tmux_session: tool.session_name().to_owned(),
            status: if created {
                crate::daemon::TerminalTargetStatus::Created
            } else {
                crate::daemon::TerminalTargetStatus::Exists
            },
            lifecycle_generation,
            profile,
        }),
    )
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

pub(super) fn ready_agent_session(instance: &Instance) -> anyhow::Result<Option<String>> {
    let panes = crate::tmux::batch_pane_metadata()?;
    let Some((name, pane)) =
        crate::tmux::agent_pane_metadata_in(&panes, &instance.id, &instance.title)?
    else {
        return Ok(None);
    };
    if pane.pane_dead {
        return Ok(None);
    }
    if !instance.expects_shell()
        && !instance.has_command_override()
        && crate::hooks::read_hook_status(&instance.id).is_none()
    {
        let command = pane
            .pane_current_command
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Agent pane command is unknown"))?;
        if crate::tmux::utils::is_pane_running_shell_command(
            command,
            pane.pane_start_command_is_protected,
        ) {
            return Ok(None);
        }
    }
    Ok(Some(name.to_owned()))
}

#[derive(Debug, thiserror::Error)]
#[error("Agent pane is not ready")]
struct AgentTargetUnavailable;

pub(super) async fn agent_target_response(
    state: &Arc<AppState>,
    id: &str,
    outcome: Option<crate::session::StartOutcome>,
) -> axum::response::Response {
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
    if outcome.is_some() {
        instance = match tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let session = instance.tmux_session()?;
            instance.wait_for_pane_ready(&session);
            Ok(instance)
        })
        .await
        {
            Ok(Ok(instance)) => instance,
            Ok(Err(error)) => return agent_target_error(state, error),
            Err(error) => return agent_target_error(state, error.into()),
        };
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
        let ownership = instance.acquire_auxiliary_locks_in(&native)?;
        anyhow::ensure!(
            instance.lifecycle_generation == generation && instance.title == title,
            crate::session::LifecycleReservationError::Superseded
        );
        anyhow::ensure!(!instance.is_structured(), AgentTargetUnavailable);
        native.configuration(Some(&instance.source_profile))?;
        let name = ready_agent_session(&instance)?.ok_or(AgentTargetUnavailable)?;
        native.adopt_agent_observation(
            &instance,
            crate::session::PaneObservation {
                state: crate::session::PanePresence::Alive,
                tmux_session: Some(name.clone()),
            },
        )?;
        Ok((instance, name, ownership))
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result);
    let (instance, name, ownership) = match result {
        Ok(target) => target,
        Err(error) => return agent_target_error(state, error),
    };
    let current = {
        let _publication = state.publication.read().await;
        state.instances.read().await.iter().any(|row| {
            row.id == id
                && row.lifecycle_generation == instance.lifecycle_generation
                && row.source_profile == instance.source_profile
                && row.title == instance.title
        })
    };
    drop(ownership);
    drop(guard);
    drop(namespace);
    if !current {
        return agent_target_error(
            state,
            crate::session::LifecycleReservationError::Superseded.into(),
        );
    }
    let snapshot = match state.runtime.publish(state).await {
        Ok(snapshot) => snapshot,
        Err(error) => return agent_target_error(state, error),
    };
    if snapshot.value.contents.health != crate::daemon::RuntimeHealth::Healthy {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if !snapshot.value.contents.sessions.iter().any(|row| {
        row.id == id
            && row.lifecycle_generation == instance.lifecycle_generation
            && row.profile == instance.source_profile
            && row.title == instance.title
    }) {
        return agent_target_error(
            state,
            crate::session::LifecycleReservationError::Superseded.into(),
        );
    }
    let mut body = serde_json::json!({
        "lifecycle_generation": instance.lifecycle_generation,
        "profile": instance.source_profile,
        "tmux_session": name,
        "status": if outcome.is_some() { "restarted" } else { "alive" },
    });
    if let Some(outcome) = outcome {
        body["resume_outcome"] = match outcome {
            crate::session::StartOutcome::Resumed => "resumed",
            crate::session::StartOutcome::Fresh => "fresh",
            crate::session::StartOutcome::ResumeFailed { .. } => "resume_failed",
            crate::session::StartOutcome::FreshAfterFailedResume { sid } => {
                body["message"] = format!("Started fresh; the prior conversation {sid} remains available in the agent's history.").into();
                body["prior_session_id"] = sid.into();
                "fresh_after_failed_resume"
            }
        }.into();
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
        let (terminal, created) = instance.start_terminal_with_size_indexed_in(
            index,
            body.size.map(|size| (size.cols.get(), size.rows.get())),
            &native,
        )?;
        Ok((terminal, created, instance))
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result);
    drop(guard);
    drop(namespace);
    let result = match result {
        Ok((terminal, created, instance)) => {
            let identity = (
                instance.lifecycle_generation,
                instance.source_profile.clone(),
            );
            crate::server::pane::publish_auxiliary_after_ensure(
                &state,
                instance,
                crate::session::AuxiliaryTarget::Host { index },
            )
            .await
            .map(|cursor| (terminal, created, cursor, identity))
        }
        Err(error) => Err(error),
    };
    let (terminal, created, cursor, (lifecycle_generation, profile)) = match result {
        Ok(result) => result,
        Err(error) => {
            if let Some(response) = lifecycle_rejection(&state, &error) {
                return response;
            }
            if error.is::<crate::session::NativeStoreUnavailable>() {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            tracing::warn!(target: "http.api.sessions", session = %id, %error, "terminal ensure failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    crate::server::runtime::mutation_response(
        &cursor,
        (
            if created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            },
            Json(crate::daemon::TerminalTarget {
                tmux_session: terminal.name().to_owned(),
                status: if created {
                    crate::daemon::TerminalTargetStatus::Created
                } else {
                    crate::daemon::TerminalTargetStatus::Exists
                },
                lifecycle_generation,
                profile,
            }),
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
    match crate::server::pane::ensure_native_container_terminal(&state, &id, q.index, size).await {
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
        Ok(cursor) => crate::server::runtime::mutation_response(
            &cursor,
            (
                StatusCode::OK,
                Json(serde_json::json!({"status": "killed"})),
            ),
        ),
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
