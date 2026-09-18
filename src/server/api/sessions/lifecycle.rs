//! Pin, color, archive, trash/restore, rename/summarize triggers, and
//! stop/start/snooze/unread endpoints.

use super::*;

pub async fn update_session_pin(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdatePinBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    let namespace = state.profile_namespace.read().await;
    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;
    if let Some(response) = cityhall_block_non_structured(&state, &id).await {
        return response;
    }

    let profile = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return crate::server::api::session_not_found();
        };
        inst.source_profile.clone()
    };

    let pinned = body.pinned;

    let persist_id = id.clone();
    let committed = commit_profile_update(
        &state,
        profile,
        "pin update",
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                if pinned {
                    inst.pin();
                } else {
                    inst.unpin();
                }
            }
        },
        None,
    )
    .await;
    if let Err(response) = committed {
        return response;
    }
    drop(_guard);
    drop(namespace);
    crate::server::runtime::session_mutation_response(&state, &id, None::<()>).await
}

pub async fn update_session_favorite(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdateFavoriteBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return rejection.into_response(),
    };

    let namespace = state.profile_namespace.read().await;
    let lock = state.instance_lock(&id).await;
    let guard = lock.lock().await;
    if let Some(response) = cityhall_block_non_structured(&state, &id).await {
        return response;
    }
    let profile = {
        let instances = state.instances.read().await;
        let Some(instance) = instances.iter().find(|instance| instance.id == id) else {
            return crate::server::api::session_not_found();
        };
        instance.source_profile.clone()
    };
    let persist_id = id.clone();
    let committed = commit_profile_update(
        &state,
        profile,
        "favorite update",
        move |instances| {
            if let Some(instance) = instances
                .iter_mut()
                .find(|instance| instance.id == persist_id)
            {
                if body.favorited {
                    instance.favorite();
                } else {
                    instance.unfavorite();
                }
            }
        },
        None,
    )
    .await;
    if let Err(response) = committed {
        return response;
    }
    drop(guard);
    drop(namespace);
    crate::server::runtime::session_mutation_response(&state, &id, None::<()>).await
}

pub async fn update_session_color(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdateColorBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    // Validate up front so an unknown color never reaches disk. `None` clears
    // the label. Mirrors the CLI's palette check.
    let new_color = body.color.map(|c| c.trim().to_lowercase());
    if let Some(c) = &new_color {
        if !crate::session::is_valid_session_color(c) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!("invalid color {c:?}; expected one of: red, amber, green, or null"),
                })),
            )
                .into_response();
        }
    }

    let namespace = state.profile_namespace.read().await;
    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;
    if let Some(response) = cityhall_block_non_structured(&state, &id).await {
        return response;
    }

    let profile = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return crate::server::api::session_not_found();
        };
        inst.source_profile.clone()
    };

    let persist_id = id.clone();
    let committed = commit_profile_update(
        &state,
        profile,
        "color update",
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                // Pre-validated above, so this cannot fail.
                let _ = inst.set_color(new_color);
            }
        },
        None,
    )
    .await;
    if let Err(response) = committed {
        return response;
    }
    drop(_guard);
    drop(namespace);
    crate::server::runtime::session_mutation_response(&state, &id, None::<()>).await
}

pub async fn update_session_archive(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdateArchiveBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(response) = cityhall_block_non_structured(&state, &id).await {
        return response;
    }
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return rejection.into_response(),
    };
    let namespace = state.profile_namespace.read().await;
    if *state.canonical_health.read().await != crate::daemon::RuntimeHealth::Healthy {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let Some(submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return crate::server::api::session_not_found();
    };
    let lock = state.instance_lock(&id).await;
    let guard = lock.lock().await;
    let profile = {
        let instances = state.instances.read().await;
        let Some(instance) = instances.iter().find(|row| row.id == id) else {
            return crate::server::api::session_not_found();
        };
        instance.source_profile.clone()
    };
    let worker_state = state.clone();
    let worker_id = id.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let native = crate::server::session_store::NativeSessionStore::open(
            worker_state.clone(), &profile, Some(worker_id.clone()),
        )?;
        let store: &dyn crate::session::SessionStore = &native;
        let _title = crate::session::acquire_session_title_lock(&worker_id)?;
        let _lifecycle = store.storage().acquire_instance_lifecycle_lock(&worker_id)?;
        let reserved = store.update(|rows, _| {
            let row = rows.iter_mut().find(|row| row.id == worker_id)
                .ok_or(LifecycleTargetError::Missing)?;
            if worker_state.cityhall_mode && !row.is_structured() {
                return Err(LifecycleTargetError::CityHall.into());
            }
            let now = chrono::Utc::now();
            if matches!(row.status, Status::Creating | Status::Deleting)
                || row.has_fresh_lifecycle_reservation(now) {
                return Err(LifecycleTargetError::Busy.into());
            }
            if !body.archived {
                row.unarchive();
                return Ok(None);
            }
            let generation = row.try_acquire_lifecycle_reservation(
                LifecycleOperation::Stop, Instance::LIFECYCLE_RESERVATION_TTL, now,
            )?;
            row.archive();
            Ok(Some((generation, row.clone())))
        })?;
        let Some((generation, mut instance)) = reserved else {
            return Ok(());
        };
        instance.source_profile = profile;
        instance.file_watch = Some(worker_state.file_watch.clone());
        let stopped = if instance.is_structured() {
            match tokio::runtime::Handle::current().block_on(worker_state.acp_supervisor.shutdown(&worker_id)) {
                Ok(()) | Err(crate::acp::supervisor::SupervisorError::UnknownSession(_)) => {}
                Err(error) => tracing::warn!(target: "acp.supervisor", session = %worker_id, %error, "shutdown during archive failed"),
            }
            if body.kill_pane {
                instance.kill_ancillary_tmux_sessions_locked()
            } else { Ok(()) }
        } else if body.kill_pane {
            instance.kill_all_tmux_sessions_locked()
        } else { Ok(()) };
        store.update(|rows, _| {
            let row = rows.iter_mut().find(|row| row.id == worker_id)
                .ok_or(LifecycleTargetError::Missing)?;
            let status = if stopped.is_err() {
                Status::Error
            } else if instance.is_structured() || body.kill_pane {
                Status::Stopped
            } else {
                row.status
            };
            if !row.finish_lifecycle_status(LifecycleOperation::Stop, generation, status) {
                return Err(LifecycleTargetError::Busy.into());
            }
            if let Err(error) = &stopped { row.last_error = Some(error.to_string()); }
            Ok(())
        })?;
        if instance.is_structured() || body.kill_pane {
            native.refresh_pane_observations(&instance)?;
        }
        stopped
    }).await.map_err(anyhow::Error::from).and_then(|result| result);
    if let Err(error) = result {
        if let Some(response) = lifecycle_rejection(&state, &error) {
            return response;
        }
        if error.is::<crate::session::NativeStoreUnavailable>() {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        tracing::warn!(target: "http.api.sessions", session = %id, %error, "archive commit failed");
        return persist_failed_response();
    }
    drop(guard);
    drop(submission);
    drop(namespace);
    crate::server::runtime::session_mutation_response(&state, &id, None::<()>).await
}

/// Recheck ownership under identity and lifecycle exclusion before relocation.
pub async fn trash_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<TrashSessionBody>>,
) -> impl IntoResponse {
    if let Some(response) = cityhall_block_non_structured(&state, &id).await {
        return response;
    }
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    let body = body.map(|Json(body)| body).unwrap_or_default();
    let namespace = state.profile_namespace.read().await;
    if *state.canonical_health.read().await != crate::daemon::RuntimeHealth::Healthy {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let Some(submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return crate::server::api::session_not_found();
    };
    let lock = state.instance_lock(&id).await;
    let guard = lock.lock().await;
    let profile = {
        let instances = state.instances.read().await;
        let Some(instance) = instances.iter().find(|row| row.id == id) else {
            return crate::server::api::session_not_found();
        };
        instance.source_profile.clone()
    };
    let worker_state = state.clone();
    let worker_id = id.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let native = crate::server::session_store::NativeSessionStore::open(
            worker_state.clone(), &profile, Some(worker_id.clone()),
        )?;
        let store: &dyn crate::session::SessionStore = &native;
        let title = crate::session::acquire_session_title_lock(&worker_id)?;
        let _lifecycle = store.storage().acquire_instance_lifecycle_lock(&worker_id)?;
        let mut instance = store.update(|rows, _| {
            let row = rows.iter_mut().find(|row| row.id == worker_id)
                .ok_or(LifecycleTargetError::Missing)?;
            if worker_state.cityhall_mode && !row.is_structured() {
                return Err(LifecycleTargetError::CityHall.into());
            }
            row.try_acquire_lifecycle_reservation(
                LifecycleOperation::Trash, Instance::LIFECYCLE_RESERVATION_TTL, chrono::Utc::now(),
            )?;
            row.trash();
            Ok(row.clone())
        })?;
        instance.source_profile = profile;
        instance.file_watch = Some(worker_state.file_watch.clone());
        let generation = instance.lifecycle_generation;
        let stopped = if instance.is_structured() {
            match tokio::runtime::Handle::current().block_on(worker_state.acp_supervisor.shutdown(&worker_id)) {
                Ok(()) | Err(crate::acp::supervisor::SupervisorError::UnknownSession(_)) => {}
                Err(error) => tracing::warn!(target: "acp.supervisor", session = %worker_id, %error, "shutdown during trash failed"),
            }
            if body.kill_pane {
                instance.kill_ancillary_tmux_sessions_locked()
            } else { Ok(()) }
        } else if body.kill_pane {
            instance.kill_all_tmux_sessions_locked()
        } else { Ok(()) };
        if instance.is_structured() || body.kill_pane {
            native.refresh_pane_observations(&instance)?;
        }
        drop(_lifecycle);
        drop(title);
        let _identity = crate::session::acquire_session_identity_lock()?;
        let _lifecycle = store.storage().acquire_instance_lifecycle_lock(&worker_id)?;
        instance = store.update(|rows, _| {
            let row = rows.iter().find(|row| row.id == worker_id)
                .ok_or(LifecycleTargetError::Missing)?;
            if !row.is_trashed() || !row.lifecycle_reservation_is_owned(LifecycleOperation::Trash, generation) {
                return Err(LifecycleTargetError::Busy.into());
            }
            Ok(row.clone())
        })?;
        instance.source_profile = store.storage().profile().to_owned();
        instance.file_watch = Some(worker_state.file_watch.clone());
        let outcome = match &stopped {
            Ok(()) => crate::session::trash::prepare_trashed_worktree(&mut instance),
            Err(error) => crate::session::trash::RelocateOutcome::Failed { reason: format!("Tmux teardown failed; worktree retained: {error}") },
        };
        store.update(|rows, _| {
            if matches!(outcome, crate::session::trash::RelocateOutcome::Relocated { .. }) {
                let relocation = crate::session::trash::TrashRelocation {
                    new_project_path: instance.project_path,
                    pre_trash_project_path: instance.pre_trash_project_path,
                };
                match crate::session::claim::commit_trash_relocation(rows, &worker_id, generation, &relocation) {
                    crate::session::claim::RelocationCommit::Persisted => Ok(()),
                    crate::session::claim::RelocationCommit::AlreadyGone => Err(LifecycleTargetError::Missing.into()),
                    crate::session::claim::RelocationCommit::Superseded => Err(LifecycleTargetError::Busy.into()),
                }
            } else {
                let row = rows.iter_mut().find(|row| row.id == worker_id)
                    .ok_or(LifecycleTargetError::Missing)?;
                if !row.release_lifecycle_reservation_if_owned(LifecycleOperation::Trash, generation) {
                    return Err(LifecycleTargetError::Busy.into());
                }
                if let Err(error) = &stopped {
                    row.status = Status::Error;
                    row.last_error = Some(error.to_string());
                }
                Ok(())
            }
        })?;
        Ok(outcome)
    }).await.map_err(anyhow::Error::from).and_then(|result| result);
    let relocation = match result {
        Ok(crate::session::trash::RelocateOutcome::Failed { reason }) => {
            tracing::warn!(target: "http.api.sessions", session = %id, %reason, "trash worktree relocation skipped");
            crate::daemon::TrashRelocationOutcome::Failed { reason }
        }
        Ok(crate::session::trash::RelocateOutcome::Skipped) => {
            crate::daemon::TrashRelocationOutcome::Skipped
        }
        Ok(crate::session::trash::RelocateOutcome::Relocated { .. }) => {
            crate::daemon::TrashRelocationOutcome::Relocated
        }
        Err(error) => {
            if let Some(response) = lifecycle_rejection(&state, &error) {
                return response;
            }
            if error.is::<crate::session::NativeStoreUnavailable>() {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            tracing::warn!(target: "http.api.sessions", session = %id, %error, "trash transition failed");
            return persist_failed_response();
        }
    };
    drop(guard);
    drop(submission);
    drop(namespace);
    crate::server::runtime::session_mutation_response(
        &state,
        &id,
        Some(crate::daemon::TrashOutcome { relocation }),
    )
    .await
}

pub async fn restore_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(response) = cityhall_block_non_structured(&state, &id).await {
        return response;
    }
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    let namespace = state.profile_namespace.read().await;
    if *state.canonical_health.read().await != crate::daemon::RuntimeHealth::Healthy {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let Some(submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return crate::server::api::session_not_found();
    };
    let lock = state.instance_lock(&id).await;
    let guard = lock.lock().await;
    let profile = {
        let instances = state.instances.read().await;
        let Some(instance) = instances.iter().find(|row| row.id == id) else {
            return crate::server::api::session_not_found();
        };
        instance.source_profile.clone()
    };
    #[derive(Debug, thiserror::Error)]
    #[error("{0}")]
    struct WorktreeFailure(String);

    let worker_state = state.clone();
    let worker_id = id.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let native = crate::server::session_store::NativeSessionStore::open(
            worker_state.clone(),
            &profile,
            Some(worker_id.clone()),
        )?;
        let store: &dyn crate::session::SessionStore = &native;
        let _identity = crate::session::acquire_session_identity_lock()?;
        let _lifecycle = store
            .storage()
            .acquire_instance_lifecycle_lock(&worker_id)?;
        let claimed = store.update(|rows, _| {
            let row = rows
                .iter()
                .find(|row| row.id == worker_id)
                .ok_or(LifecycleTargetError::Missing)?;
            if worker_state.cityhall_mode && !row.is_structured() {
                return Err(LifecycleTargetError::CityHall.into());
            }
            let now = chrono::Utc::now();
            if !row.is_trashed() {
                if row.has_fresh_lifecycle_reservation(now) {
                    return Err(LifecycleTargetError::Busy.into());
                }
                return Ok(None);
            }
            match crate::session::claim::decide_restore_claim(rows, &worker_id, now)? {
                crate::session::claim::RestoreClaimDecision::Claimed(generation) => {
                    let row = rows
                        .iter()
                        .find(|row| row.id == worker_id)
                        .ok_or(LifecycleTargetError::Missing)?;
                    Ok(Some((generation, row.clone())))
                }
                crate::session::claim::RestoreClaimDecision::AlreadyGone => {
                    Err(LifecycleTargetError::Missing.into())
                }
                crate::session::claim::RestoreClaimDecision::Busy(_) => {
                    Err(LifecycleTargetError::Busy.into())
                }
            }
        })?;
        let Some((generation, mut instance)) = claimed else {
            return Ok(());
        };
        instance.source_profile = profile;
        instance.file_watch = Some(worker_state.file_watch.clone());
        if let crate::session::trash::RestoreOutcome::Failed { reason } =
            crate::session::trash::restore_worktree_location(&mut instance)
        {
            store.update(|rows, _| {
                let row = rows
                    .iter_mut()
                    .find(|row| row.id == worker_id)
                    .ok_or(LifecycleTargetError::Missing)?;
                if !row
                    .release_lifecycle_reservation_if_owned(LifecycleOperation::Restore, generation)
                {
                    return Err(LifecycleTargetError::Busy.into());
                }
                Ok(())
            })?;
            return Err(WorktreeFailure(reason).into());
        }
        store.update(|rows, _| {
            match crate::session::claim::finalize_restore_commit(
                rows,
                &worker_id,
                generation,
                &instance.project_path,
                &instance.pre_trash_project_path,
            ) {
                crate::session::claim::RestoreCommit::Committed => Ok(()),
                crate::session::claim::RestoreCommit::AlreadyGone => {
                    Err(LifecycleTargetError::Missing.into())
                }
                crate::session::claim::RestoreCommit::Superseded => {
                    Err(LifecycleTargetError::Busy.into())
                }
            }
        })
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result);
    if let Err(error) = result {
        if let Some(response) = lifecycle_rejection(&state, &error) {
            return response;
        }
        if let Some(WorktreeFailure(reason)) = error.downcast_ref::<WorktreeFailure>() {
            return (StatusCode::CONFLICT, Json(serde_json::json!({
                "error": "worktree_restore_failed", "message": format!("Could not restore the worktree: {reason}"),
            }))).into_response();
        }
        if error.is::<crate::session::NativeStoreUnavailable>() {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        tracing::warn!(target: "http.api.sessions", session = %id, %error, "restore transition failed");
        return persist_failed_response();
    }
    drop(guard);
    drop(submission);
    drop(namespace);
    crate::server::runtime::session_mutation_response(&state, &id, None::<()>).await
}

/// `POST /api/sessions/:id/smart-rename`. Manual "Auto-name now" recovery for
/// a structured-view session whose automatic smart rename never landed (the
/// one-shot timed out, returned unusable output, or the daemon restarted with
/// the in-memory attempted set cleared). Clears the per-session attempted gate
/// and re-runs the one-shot against the session's first prompt.
///
/// Only targets a still-default-named session: a session the user (or a prior
/// rename) already named is left alone, so this never overwrites a chosen
/// title. The actual rename runs detached and best-effort, exactly like the
/// prompt-handler trigger; a `202` means "re-run started", not "renamed".
pub async fn force_smart_rename(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // CityHall rejects terminal targets before admitting any work.
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    if let Some(resp) = crate::server::api::acp::read_only_block(&state) {
        return resp;
    }

    let Some((profile, tool, command, project_path, sandboxed, title, structured)) = ({
        let instances = state.instances.read().await;
        instances.iter().find(|i| i.id == id).map(|i| {
            (
                i.source_profile.clone(),
                i.tool.clone(),
                i.command.clone(),
                i.project_path.clone(),
                i.is_sandboxed(),
                i.title.clone(),
                i.is_structured(),
            )
        })
    }) else {
        return crate::server::api::session_not_found();
    };

    // Manual requests bypass only the setting; the worker revalidates eligibility.
    let resolved = crate::session::config::repo_config::resolve_config_with_repo_or_warn(
        &profile,
        std::path::Path::new(&project_path),
    );
    let config = &resolved.session;
    if let Err(reason) = crate::session::smart_rename::check_eligible_resolved(
        true,
        true,
        &title,
        &tool,
        &config.smart_rename_agent,
        sandboxed,
        &command,
        &config.agent_command_override,
    ) {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": reason.as_str(), "message": reason.user_message() })),
        )
            .into_response();
    }

    // A sandboxed session's one-shot runs inside its container, so a stopped
    // container is the one remaining way the spawned job would drop the session
    // after the static gate passed. Probe it here too, else this would answer 202
    // while nothing renames, which is exactly what the gate above exists to
    // prevent. Same check and wording as the TUI's preflight; the spawned
    // try_smart_rename re-probes and stays the authority.
    if sandboxed {
        use crate::containers::Probe;
        let sid = id.clone();
        let probe = tokio::task::spawn_blocking(move || {
            crate::containers::DockerContainer::from_session_id(&sid).probe_running()
        })
        .await;
        // A failed inspection is not a stopped container: telling the user to
        // start a container that may already be running sends them the wrong
        // way, so the runtime error is surfaced as its own state. Same split as
        // the TUI preflight.
        let unknown = match probe {
            Ok(Probe::Running) => None,
            Ok(Probe::NotRunning) => {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "error": "container_not_running",
                        "message": "The session's sandbox container is not running, so its agent cannot be asked for a name. Open the session to start it, then try again.",
                    })),
                )
                    .into_response();
            }
            Ok(Probe::Unknown(e)) => Some(e.to_string()),
            Err(e) => Some(e.to_string()),
        };
        if let Some(err) = unknown {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": "container_state_unknown",
                    "message": format!("Couldn't check the session's sandbox container, so its agent cannot be asked for a name: {err}"),
                })),
            )
                .into_response();
        }
    }

    if !structured {
        state.runtime.work.spawn(
            "server.terminal_smart_rename",
            crate::session::smart_rename::try_terminal_smart_rename(
                state.clone(),
                profile,
                id,
                true,
            ),
        );
        return StatusCode::ACCEPTED.into_response();
    }

    let Some((first_user_prompt, agent_prose)) = state
        .acp_event_store
        .first_turn_context(&id, crate::session::smart_rename::FIRST_TURN_AGENT_BYTES)
    else {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": "no_prompt", "message": "No prompt to name this session from yet" })), 
        )
            .into_response();
    };
    let context = crate::session::smart_rename::render_first_turn(&first_user_prompt, &agent_prose);

    // Clear the attempted gate so try_smart_rename does not short-circuit on a
    // prior failed attempt. The inflight guard inside try_smart_rename still
    // prevents a concurrent one-shot for the same session.
    {
        let mut attempted = state
            .smart_rename_attempted
            .lock()
            .expect("smart_rename_attempted poisoned");
        attempted.remove(&id);
    }

    state.runtime.work.spawn(
        "server.smart_rename",
        crate::session::smart_rename::try_smart_rename(
            state.clone(),
            id,
            crate::session::smart_rename::SmartRenameInput {
                first_user_prompt,
                context,
            },
            // Manual requests bypass the automatic setting.
            true,
        ),
    );
    StatusCode::ACCEPTED.into_response()
}

/// On-demand "summarize the conversation so far" for a structured-view
/// session. Preflights the same eligibility gate the spawned task re-applies
/// so the caller never gets a 202 for a session that would silently drop, then
/// runs the summary one-shot detached (best-effort, like the automatic
/// trigger). A `202` means "summary started", not "summary ready"; the result
/// arrives later as a `ConversationSummary` event over the structured-view WS.
/// Bypasses the `conversation_summary` setting and the delta threshold: an
/// explicit request always runs if the session is eligible. See #2808.
pub async fn summarize_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // CityHall: only act on structured sessions this mode created; refuse a
    // non-structured (or unknown) target so a locked-down client cannot
    // respawn/destroy/edit an enumerated plain session. See #7.
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    if let Some(resp) = crate::server::api::acp::read_only_block(&state) {
        return resp;
    }

    let Some((profile, tool, command, sandboxed, structured)) = ({
        let instances = state.instances.read().await;
        instances.iter().find(|i| i.id == id).map(|i| {
            (
                i.source_profile.clone(),
                i.tool.clone(),
                i.command.clone(),
                i.is_sandboxed(),
                i.is_structured(),
            )
        })
    }) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "message": "Session not found" })),
        )
            .into_response();
    };

    let config = crate::session::config::profile_config::resolve_config_or_warn(&profile);
    if let Err(reason) = crate::session::conversation_summary::resolve_summary_agent(
        structured,
        &tool,
        &config.session.smart_rename_agent,
        sandboxed,
        &command,
        &config.session.agent_command_override,
    ) {
        use crate::session::smart_rename::SkipReason;
        let (status, message) = match reason {
            SkipReason::NotStructured => (
                StatusCode::BAD_REQUEST,
                "Session is not a structured-view session",
            ),
            SkipReason::Sandboxed => (
                StatusCode::CONFLICT,
                "Conversation summary is not available for sandboxed sessions",
            ),
            SkipReason::NoOneshot => (
                StatusCode::CONFLICT,
                "The summary agent has no one-shot mode",
            ),
            SkipReason::CommandOverridden => (
                StatusCode::CONFLICT,
                "The summary agent's command is overridden",
            ),
            // resolve_summary_agent never returns the rename-only reasons.
            SkipReason::NameNotDefault
            | SkipReason::Disabled
            | SkipReason::SandboxRenameAgentMismatch => (
                StatusCode::CONFLICT,
                "Conversation summary is unavailable for this session",
            ),
        };
        return (status, Json(serde_json::json!({ "message": message }))).into_response();
    }

    state.runtime.work.spawn(
        "server.conversation_summary",
        crate::session::conversation_summary::try_conversation_summary(
            state.clone(),
            id,
            crate::session::conversation_summary::SummaryTrigger::Manual,
        ),
    );
    StatusCode::ACCEPTED.into_response()
}

#[derive(Debug, thiserror::Error)]
pub(super) enum LifecycleTargetError {
    #[error("session not found")]
    Missing,
    #[error("session is not writable in CityHall mode")]
    CityHall,
    #[error("session lifecycle is busy or superseded")]
    Busy,
}

pub(crate) fn lifecycle_rejection(
    state: &AppState,
    error: &anyhow::Error,
) -> Option<axum::response::Response> {
    match error.downcast_ref::<LifecycleTargetError>() {
        Some(LifecycleTargetError::Missing) => {
            return Some(if state.cityhall_mode {
                crate::server::api::cityhall_response()
            } else {
                crate::server::api::session_not_found()
            })
        }
        Some(LifecycleTargetError::CityHall) => {
            return Some(crate::server::api::cityhall_response())
        }
        _ => {}
    }
    if matches!(
        error.downcast_ref::<LifecycleTargetError>(),
        Some(LifecycleTargetError::Busy)
    ) || matches!(
        error.downcast_ref::<crate::session::LifecycleReservationError>(),
        Some(
            crate::session::LifecycleReservationError::Busy(_)
                | crate::session::LifecycleReservationError::Superseded
        )
    ) {
        return Some((StatusCode::CONFLICT,
            crate::daemon::ApiErrorCode::LifecycleLocked.header(),
            Json(serde_json::json!({"error": "lifecycle_busy", "message": "Session lifecycle is busy"}))).into_response());
    }
    None
}

async fn adopt_lifecycle_commit<R>(
    state: &Arc<AppState>,
    profile: String,
    id: &str,
    result: anyhow::Result<(R, Vec<Instance>, Vec<crate::session::Group>)>,
    publication: &tokio::sync::RwLockWriteGuard<'_, ()>,
) -> Result<R, axum::response::Response> {
    if let Err(error) = &result {
        if let Some(response) = lifecycle_rejection(state, error) {
            return Err(response);
        }
    }
    let result = result.map_err(|error| {
        tracing::warn!(target: "http.api.sessions", session = %id, "lifecycle commit failed: {error}");
    });
    super::update::adopt_profile_update(state, profile, result, |row_id| row_id == id, publication)
        .await
}

// The caller retains namespace, submission, and instance exclusion.
async fn stop_with_commit(
    state: &Arc<AppState>,
    profile: String,
    id: &str,
) -> Result<(), axum::response::Response> {
    let stop_profile = profile.clone();
    let stop_id = id.to_owned();
    let file_watch = state.file_watch.clone();
    let opened = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let storage = Storage::open(&stop_profile, file_watch)?;
        let title = crate::session::acquire_session_title_lock(&stop_id)?;
        let lifecycle = storage.acquire_instance_lifecycle_lock(&stop_id)?;
        let transition = storage.acquire_write_transition()?;
        Ok((storage, title, lifecycle, transition))
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result);
    let (storage, title, lifecycle, transition) = match opened {
        Ok(opened) => opened,
        Err(error) => {
            let publication = state.publication.write().await;
            return adopt_lifecycle_commit(state, profile, id, Err(error), &publication).await;
        }
    };

    let publication = state.publication.write().await;
    if *state.canonical_health.read().await != crate::daemon::RuntimeHealth::Healthy {
        return Err(StatusCode::SERVICE_UNAVAILABLE.into_response());
    }
    let reserve_id = id.to_owned();
    let cityhall = state.cityhall_mode;
    let reserved = tokio::task::spawn_blocking(move || {
        let result = transition.update_with_snapshot(&storage, |rows, _| {
            let row = rows
                .iter_mut()
                .find(|row| row.id == reserve_id)
                .ok_or(if cityhall {
                    LifecycleTargetError::CityHall
                } else {
                    LifecycleTargetError::Missing
                })?;
            if cityhall && !row.is_structured() {
                return Err(LifecycleTargetError::CityHall.into());
            }
            if matches!(row.status, Status::Creating | Status::Deleting) {
                return Err(LifecycleTargetError::Busy.into());
            }
            let generation = row.try_acquire_lifecycle_reservation(
                LifecycleOperation::Stop,
                Instance::LIFECYCLE_RESERVATION_TTL,
                chrono::Utc::now(),
            )?;
            if row.status == Status::Stopped {
                row.release_lifecycle_reservation_if_owned(LifecycleOperation::Stop, generation);
                return Ok(None);
            }
            if row.is_structured() {
                row.status = Status::Stopped;
                row.mark_idle_dormant();
            }
            Ok(Some(generation))
        });
        (storage, lifecycle, transition, result)
    })
    .await;
    let (storage, lifecycle, transition, result) = match reserved {
        Ok(reserved) => reserved,
        Err(error) => {
            return adopt_lifecycle_commit(state, profile, id, Err(error.into()), &publication)
                .await
        }
    };
    let generation =
        adopt_lifecycle_commit(state, profile.clone(), id, result, &publication).await?;
    drop(publication);
    drop(transition);
    let Some(generation) = generation else {
        return Ok(());
    };
    let instance = state
        .instances
        .read()
        .await
        .iter()
        .find(|row| row.id == id)
        .cloned()
        .ok_or_else(crate::server::api::session_gone_after_persist)?;

    let (effect, pi_update) = if instance.is_structured() {
        let result = match state.acp_supervisor.shutdown(id).await {
            Ok(()) | Err(crate::acp::supervisor::SupervisorError::UnknownSession(_)) => Ok(()),
            Err(error) => Err(anyhow::Error::from(error)),
        };
        (result, None)
    } else {
        match tokio::task::spawn_blocking(move || {
            let result = instance.stop_resources_locked();
            let pi = if result.is_ok() {
                instance.read_pi_sidecar_update()
            } else {
                None
            };
            (result, pi)
        })
        .await
        {
            Ok(result) => result,
            Err(error) => (Err(error.into()), None),
        }
    };
    let stopped = effect.is_ok();
    let prepared = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let panes = match crate::tmux::batch_pane_metadata() {
            Ok(panes) => Some(panes),
            Err(error) => {
                tracing::warn!(target: "http.api.sessions", "post-stop pane observation failed: {error}");
                None
            }
        };
        let transition = storage.acquire_write_transition()?;
        Ok((storage, lifecycle, transition, panes))
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result);
    let publication = state.publication.write().await;
    let (storage, lifecycle, transition, panes) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            return adopt_lifecycle_commit(state, profile, id, Err(error), &publication).await
        }
    };
    if *state.canonical_health.read().await != crate::daemon::RuntimeHealth::Healthy {
        return Err(StatusCode::SERVICE_UNAVAILABLE.into_response());
    }
    let commit_id = id.to_owned();
    let finished = tokio::task::spawn_blocking(move || {
        let result = transition.update_with_snapshot(&storage, |rows, _| {
            let row = rows
                .iter_mut()
                .find(|row| row.id == commit_id)
                .ok_or(LifecycleTargetError::Missing)?;
            if !row.finish_lifecycle_status(
                LifecycleOperation::Stop,
                generation,
                if stopped {
                    Status::Stopped
                } else {
                    Status::Error
                },
            ) {
                return Err(LifecycleTargetError::Busy.into());
            }
            if let Some(pi) = pi_update {
                pi.apply(row);
            }
            if !stopped {
                row.last_error = Some("Failed to stop session resources".into());
            }
            Ok(())
        });
        ((storage, lifecycle, transition), result)
    })
    .await;
    let (prepared, result) = match finished {
        Ok(finished) => finished,
        Err(error) => {
            return adopt_lifecycle_commit(state, profile, id, Err(error.into()), &publication)
                .await
        }
    };
    adopt_lifecycle_commit(state, profile, id, result, &publication).await?;
    {
        let mut rows = state.instances.write().await;
        let row = rows
            .iter_mut()
            .find(|row| row.id == id)
            .ok_or_else(crate::server::api::session_gone_after_persist)?;
        let metadata = state.canonical_metadata.read().await;
        let tools = metadata
            .auxiliary_tools
            .get(&row.source_profile)
            .map(Vec::as_slice)
            .unwrap_or_default();
        crate::server::pane::sample_panes(row, tools, panes.as_ref());
        state
            .mutation_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    drop(publication);
    drop(prepared);
    drop(title);
    if let Err(error) = effect {
        tracing::warn!(target: "http.api.sessions", session = %id, "stop resources failed: {error}");
        return Err((StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "stop_failed", "message": "Failed to stop session resources"})))
            .into_response());
    }
    let cleanup_id = id.to_owned();
    tokio::task::spawn_blocking(move || crate::hooks::cleanup_hook_status_dir(&cleanup_id))
        .await.map_err(|error| {
            tracing::warn!(target: "http.api.sessions", session = %id, "stop cleanup failed: {error}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        })?;
    Ok(())
}

/// Stop resources while retaining the session record for resume.
pub async fn stop_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(response) = cityhall_block_non_structured(&state, &id).await {
        return response;
    }
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    let namespace = state.profile_namespace.read().await;
    if *state.canonical_health.read().await != crate::daemon::RuntimeHealth::Healthy {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let Some(submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return crate::server::api::session_not_found();
    };
    let lock = state.instance_lock(&id).await;
    let guard = lock.lock().await;
    let profile = {
        let rows = state.instances.read().await;
        let Some(row) = rows.iter().find(|row| row.id == id) else {
            return crate::server::api::session_not_found();
        };
        row.source_profile.clone()
    };
    if let Err(response) = stop_with_commit(&state, profile, &id).await {
        return response;
    }
    drop(guard);
    drop(submission);
    drop(namespace);
    crate::server::runtime::session_mutation_response(&state, &id, None::<()>).await
}

/// Resume a stopped session; structured workers restart through reconciliation.
pub async fn start_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<
        Option<Json<crate::daemon::StartSessionBody>>,
        axum::extract::rejection::JsonRejection,
    >,
) -> impl IntoResponse {
    prepare_agent_session(state, id, body, AgentPreparation::Start, None).await
}

pub async fn restart_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<
        Option<Json<crate::daemon::RestartSessionBody>>,
        axum::extract::rejection::JsonRejection,
    >,
) -> axum::response::Response {
    if let Some(response) = crate::server::api::cityhall_block(&state) {
        return response;
    }
    let body = match body {
        Ok(body) => body.map(|Json(body)| body).unwrap_or_default(),
        Err(error) => return error.into_response(),
    };
    prepare_agent_session(
        state,
        id,
        Ok(None),
        AgentPreparation::Restart,
        Some(Arc::new(body)),
    )
    .await
}

fn admit_restart(
    row: &mut Instance,
    body: &crate::daemon::RestartSessionBody,
) -> anyhow::Result<()> {
    let now = chrono::Utc::now();
    if row.is_trashed()
        || row.is_archived()
        || matches!(row.status, Status::Creating | Status::Deleting)
        || row.has_fresh_lifecycle_reservation(now)
    {
        return Err(LifecycleTargetError::Busy.into());
    }
    row.try_acquire_lifecycle_reservation(
        LifecycleOperation::Launch,
        Instance::LIFECYCLE_RESERVATION_TTL,
        now,
    )?;
    if let Some(tool) = &body.tool {
        if row.tool != *tool {
            row.swap_tool(tool);
        }
    }
    if let Some(command) = &body.command_override {
        row.command.clone_from(command);
    }
    if let Some(extra_args) = &body.extra_args {
        row.extra_args.clone_from(extra_args);
    }
    if body.unsnooze {
        row.unsnooze();
    }
    row.touch_last_accessed();
    row.idle_dormant_since = None;
    row.idle_entered_at = None;
    row.last_error = None;
    row.last_error_check = None;
    row.status = Status::Starting;
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum AgentPreparation {
    Start,
    Restart,
    Ensure,
}

pub(super) async fn prepare_agent_session(
    state: Arc<AppState>,
    id: String,
    body: Result<
        Option<Json<crate::daemon::StartSessionBody>>,
        axum::extract::rejection::JsonRejection,
    >,
    preparation: AgentPreparation,
    restart: Option<Arc<crate::daemon::RestartSessionBody>>,
) -> axum::response::Response {
    if preparation == AgentPreparation::Ensure && state.cityhall_mode {
        return crate::server::api::cityhall_response();
    }
    if state.read_only && preparation == AgentPreparation::Ensure {
        return super::ensure::agent_target_response(&state, &id, None).await;
    }
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    let namespace = state.profile_namespace.read().await;
    if *state.canonical_health.read().await != crate::daemon::RuntimeHealth::Healthy {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let Some(submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return if state.cityhall_mode {
            crate::server::api::cityhall_response()
        } else {
            crate::server::api::session_not_found()
        };
    };
    let lock = state.instance_lock(&id).await;
    let guard = lock.lock().await;
    let instance = {
        let instances = state.instances.read().await;
        let Some(instance) = instances.iter().find(|row| row.id == id) else {
            return if state.cityhall_mode {
                crate::server::api::cityhall_response()
            } else {
                crate::server::api::session_not_found()
            };
        };
        instance.clone()
    };
    if preparation == AgentPreparation::Ensure && instance.is_structured() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if state.cityhall_mode && !instance.is_structured() {
        return crate::server::api::cityhall_response();
    }
    let body = match body {
        Ok(body) => body.map(|Json(body)| body).unwrap_or_default(),
        Err(rejection) => return rejection.into_response(),
    };
    let size = restart
        .as_ref()
        .and_then(|body| body.size.as_ref())
        .or(body.size.as_ref())
        .map(|size| (size.cols.get(), size.rows.get()));
    let worker_restart = restart.clone();
    let worker_state = state.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        use crate::session::SessionStore;
        let profile = instance.source_profile.clone();
        let mut native = crate::server::session_store::NativeSessionStore::open(
            worker_state.clone(),
            &profile,
            Some(instance.id.clone()),
        )?;
        native.configuration(Some(&profile))?;
        let identity = worker_restart
            .as_ref()
            .map(|_| crate::session::acquire_session_identity_lock())
            .transpose()?;
        let title_lock = crate::session::acquire_session_title_lock(&instance.id)?;
        let mut lifecycle_lock = native
            .storage()
            .acquire_instance_lifecycle_lock(&instance.id)?;
        let target = worker_restart
            .as_ref()
            .and_then(|body| body.profile.as_deref())
            .filter(|target| *target != profile);
        let target_native = target
            .map(|target| {
                let store = crate::server::session_store::NativeSessionStore::open(
                    worker_state.clone(),
                    target,
                    Some(instance.id.clone()),
                )?;
                store.configuration(Some(target))?;
                Ok::<_, anyhow::Error>(store)
            })
            .transpose()?;
        let mut outgoing = instance.clone();
        if let Some(body) = &worker_restart {
            outgoing.reconcile_from_store(&native)?;
            let mut probe = outgoing.clone();
            if let Some(target) = target {
                probe.source_profile = target.into();
            }
            admit_restart(&mut probe, body)?;
            if let Some(target_native) = &target_native {
                let rows = target_native.storage().load()?;
                if rows.iter().any(|row| row.id == outgoing.id)
                    || is_duplicate_session(rows.iter(), &probe.title, &probe.project_path, None)
                {
                    return Err(duplicate_session_error(&probe.title));
                }
            }
            // Capture using the outgoing tool/profile, before the atomic edit parks its SID.
            if !outgoing.is_structured() {
                outgoing.capture_before_restart_in(&native)?;
            }
        }
        let mut started = if let (Some(body), Some(target_native)) =
            (&worker_restart, target_native)
        {
            let mut moved = None;
            native.move_instances_to(
                &target_native,
                &[(outgoing.clone(), outgoing.clone())],
                &crate::session::GroupMovePlan::single(&outgoing.group_path, &outgoing.group_path),
                |existing, candidates| {
                    let candidate = &mut candidates[0];
                    if is_duplicate_session(
                        existing.iter(),
                        &candidate.title,
                        &candidate.project_path,
                        None,
                    ) {
                        return Err(duplicate_session_error(&candidate.title));
                    }
                    admit_restart(candidate, body)?;
                    moved = Some(candidate.clone());
                    Ok(())
                },
            )?;
            lifecycle_lock = target_native
                .storage()
                .acquire_instance_lifecycle_lock(&instance.id)?;
            native = target_native;
            moved
        } else {
            let store: &dyn crate::session::SessionStore = &native;
            store.update(|rows, _| {
                let row = rows
                    .iter_mut()
                    .find(|row| row.id == instance.id)
                    .ok_or(LifecycleTargetError::Missing)?;
                if worker_state.cityhall_mode && !row.is_structured() {
                    return Err(LifecycleTargetError::CityHall.into());
                }
                if let Some(body) = &worker_restart {
                    row.source_profile.clone_from(&profile);
                    admit_restart(row, body)?;
                    return Ok(Some(row.clone()));
                }
                let now = chrono::Utc::now();
                if row.is_trashed()
                    || matches!(row.status, Status::Creating | Status::Deleting)
                    || row.has_fresh_lifecycle_reservation(now)
                {
                    return Err(LifecycleTargetError::Busy.into());
                }
                if preparation == AgentPreparation::Ensure {
                    anyhow::ensure!(
                        !row.is_structured(),
                        "Structured sessions have no agent pane"
                    );
                    if super::ensure::ready_agent_session(row)?.is_some() {
                        return Ok(None);
                    }
                } else if row.status != Status::Stopped {
                    return Ok(None);
                }
                let generation = row.try_acquire_lifecycle_reservation(
                    LifecycleOperation::Launch,
                    Instance::LIFECYCLE_RESERVATION_TTL,
                    now,
                )?;
                row.idle_dormant_since = None;
                row.idle_entered_at = None;
                row.last_error = None;
                if row.is_structured() {
                    row.status = Status::Idle;
                    row.release_lifecycle_reservation_if_owned(
                        LifecycleOperation::Launch,
                        generation,
                    );
                    return Ok(None);
                }
                row.status = Status::Starting;
                Ok(Some(row.clone()))
            })?
        };
        let Some(mut started) = started.take() else {
            return Ok(None);
        };
        started.source_profile = native.storage().profile().to_owned();
        started.file_watch = Some(worker_state.file_watch.clone());
        crate::server::reload::merge_runtime_fields(outgoing, &mut started);
        let store: &dyn crate::session::SessionStore = &native;
        if worker_restart.as_ref().is_some_and(|body| {
            body.discard_sandbox_container
                || body
                    .tool
                    .as_ref()
                    .is_some_and(|tool| *tool != instance.tool)
        }) {
            started.discard_reserved_restart_container(store, started.lifecycle_generation)?;
        }
        let generation = started.prepare_reserved_launch_hooks(
            store,
            worker_restart.is_none(),
            crate::session::LaunchReservation {
                generation: started.lifecycle_generation,
                title_lock,
                lifecycle_lock,
            },
        )?;
        drop(identity);
        Ok(Some((generation, started, native)))
    })
    .await;
    drop(guard);
    drop(submission);
    drop(namespace);
    let hooked = match result {
        Ok(Ok(Some((generation, mut started, native)))) => {
            tokio::task::spawn_blocking(move || {
                let _timeout = restart.as_ref().filter(|body| body.bound_hooks).map(|_| {
                    crate::session::recovery::HookTimeoutScope::new(
                        crate::session::recovery::recovery_hook_timeout(),
                    )
                });
                let hooks = started.run_pre_launch_hooks(
                    restart.as_ref().is_some_and(|body| body.skip_on_launch),
                    &native,
                    None,
                );
                Ok(Some((generation, started, native, hooks)))
            })
            .await
        }
        Ok(Ok(None)) => Ok(Ok(None)),
        Ok(Err(error)) => Ok(Err(error)),
        Err(error) => Err(error),
    };
    let namespace = state.profile_namespace.read().await;
    let submission = state
        .session_service
        .prompt_submission_for_session(&id)
        .await;
    let guard = lock.lock().await;
    let mut restart_identity = None;
    let result = match hooked {
        Ok(Ok(Some((generation, mut started, native, hooks)))) => {
            tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                use crate::session::SessionStore;
                let outcome = started.finish_reserved_launch(
                    &native,
                    size,
                    if preparation == AgentPreparation::Ensure {
                        crate::session::ResumeAttemptPolicy::Allow
                    } else {
                        crate::session::ResumeAttemptPolicy::HonorAutoResumeSetting
                    },
                    true,
                    generation,
                    hooks,
                );
                let title = crate::session::acquire_session_title_lock(&started.id)?;
                let lifecycle = native.storage().acquire_instance_lifecycle_lock(&started.id)?;
                let panes = match crate::tmux::batch_pane_metadata() {
                    Ok(panes) => Some(panes),
                    Err(error) => {
                        tracing::warn!(target: "http.api.sessions", session = %started.id, "post-launch pane observation failed: {error}");
                        None
                    }
                };
                Ok(Some((generation, started, outcome, (title, lifecycle), panes)))
            })
            .await
        }
        Ok(Ok(None)) => Ok(Ok(None)),
        Ok(Err(error)) => Ok(Err(error)),
        Err(error) => Err(error),
    };
    let outcome = match result {
        Ok(Ok(Some((generation, started, outcome, ownership, panes)))) => {
            let adopted = {
                let _publication = state.publication.write().await;
                let mut instances = state.instances.write().await;
                match instances.iter_mut().find(|row| row.id == id) {
                    Some(row)
                        if row.lifecycle_generation == generation
                            && started.lifecycle_generation == generation
                            && row.source_profile == started.source_profile
                            && row.title == started.title =>
                    {
                        row.inherit_process_runtime(started);
                        restart_identity =
                            Some((row.lifecycle_generation, row.source_profile.clone()));
                        let metadata = state.canonical_metadata.read().await;
                        let tools = metadata
                            .auxiliary_tools
                            .get(&row.source_profile)
                            .map(Vec::as_slice)
                            .unwrap_or_default();
                        crate::server::pane::sample_panes(row, tools, panes.as_ref());
                        state
                            .mutation_epoch
                            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        state.runtime.request_publish();
                        true
                    }
                    _ => false,
                }
            };
            drop(ownership);
            if adopted {
                outcome.map(Some)
            } else {
                Err(crate::session::LifecycleReservationError::Superseded.into())
            }
        }
        Ok(Ok(None)) => Ok(None),
        Ok(Err(error)) => Err(error),
        Err(error) => Err(error.into()),
    };
    let outcome = match outcome {
        Err(error) => {
            if let Some(response) = lifecycle_rejection(&state, &error) {
                return response;
            }
            tracing::warn!(target: "http.api.sessions", session = %id, %error, "session start failed");
            if error.is::<crate::session::NativeStoreUnavailable>() {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    serde_json::json!({"error": "start_failed", "message": "Session start failed"}),
                ),
            )
                .into_response();
        }
        Ok(Some(crate::session::StartOutcome::ResumeFailed { sid })) => {
            return (StatusCode::CONFLICT, crate::daemon::ApiErrorCode::ResumeFailed.header(), Json(serde_json::json!({
                "error": "resume_failed", "message": "Resume failed; preserved for explicit retry", "resume_session_id": sid,
            }))).into_response();
        }
        Ok(outcome) => outcome,
    };
    drop(guard);
    drop(submission);
    drop(namespace);
    if preparation == AgentPreparation::Restart {
        let Some((generation, profile)) = restart_identity else {
            return StatusCode::CONFLICT.into_response();
        };
        let rows = state.instances.read().await;
        let Some(row) = rows.iter().find(|row| {
            row.id == id && row.lifecycle_generation == generation && row.source_profile == profile
        }) else {
            return StatusCode::CONFLICT.into_response();
        };
        let target = if row.is_structured() {
            None
        } else {
            let Some(name) = row
                .agent_pane
                .tmux_session
                .as_ref()
                .filter(|_| row.agent_pane.state == crate::session::PanePresence::Alive)
            else {
                return StatusCode::CONFLICT.into_response();
            };
            Some(crate::daemon::TerminalTarget {
                tmux_session: name.clone(),
                status: crate::daemon::TerminalTargetStatus::Restarted,
            })
        };
        drop(rows);
        return crate::server::runtime::session_mutation_response(
            &state,
            &id,
            Some(crate::daemon::RestartOutcome {
                lifecycle_generation: generation,
                profile,
                target,
            }),
        )
        .await;
    }
    if preparation == AgentPreparation::Ensure {
        super::ensure::agent_target_response(&state, &id, outcome).await
    } else {
        crate::server::runtime::session_mutation_response(&state, &id, None::<()>).await
    }
}

pub async fn update_session_snooze(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdateSnoozeBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    // Share the CLI and TUI duration bounds.
    if let Some(minutes) = body.minutes {
        if let Err(msg) = crate::session::validate_snooze_duration(minutes as u64) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "validation_failed",
                    "message": msg,
                })),
            )
                .into_response();
        }
    }

    let namespace = state.profile_namespace.read().await;
    // Submission precedes the instance lock for worker teardown.
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return crate::server::api::session_not_found();
    };
    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;
    if let Some(response) = cityhall_block_non_structured(&state, &id).await {
        return response;
    }

    let (was_structured_view, profile) = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return crate::server::api::session_not_found();
        };

        let structured_view = inst.is_structured();
        (structured_view, inst.source_profile.clone())
    };

    let minutes = body.minutes;

    let persist_id = id.clone();
    let committed = commit_profile_update(
        &state,
        profile,
        "snooze update",
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                match minutes {
                    Some(m) => inst.snooze(m),
                    None => inst.unsnooze(),
                }
            }
        },
        None,
    )
    .await;
    if let Err(response) = committed {
        return response;
    }

    // Preserve the transcript; reconciliation resumes it after snooze expires.
    if was_structured_view && minutes.is_some() {
        match state.acp_supervisor.shutdown(&id).await {
            Ok(()) | Err(crate::acp::supervisor::SupervisorError::UnknownSession(_)) => {}
            Err(e) => tracing::warn!(
                target: "acp.supervisor",
                session = %id,
                "shutdown during snooze failed: {e}"
            ),
        }
    }

    drop(_guard);
    drop(_submission);
    drop(namespace);
    crate::server::runtime::session_mutation_response(&state, &id, None::<()>).await
}

/// Set an explicit unread target; a disabled indicator leaves the row unchanged.
pub async fn update_session_unread(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdateUnreadBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    let mark_unread = body.unread;

    let namespace = state.profile_namespace.read().await;
    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;
    if let Some(response) = cityhall_block_non_structured(&state, &id).await {
        return response;
    }

    let profile = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return crate::server::api::session_not_found();
        };
        inst.source_profile.clone()
    };

    // A disabled indicator leaves the durable row untouched.
    if crate::session::unread_enabled() {
        let persist_id = id.clone();
        let committed = commit_profile_update(
            &state,
            profile,
            "unread update",
            move |instances| {
                if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                    if mark_unread {
                        inst.mark_unread();
                    } else {
                        inst.mark_read();
                    }
                }
            },
            None,
        )
        .await;
        if let Err(response) = committed {
            return response;
        }
    }

    drop(_guard);
    drop(namespace);
    crate::server::runtime::session_mutation_response(&state, &id, None::<()>).await
}
