//! Pin, color, archive, trash/restore, rename/summarize triggers, and
//! stop/start/snooze/unread endpoints.

use super::*;
use std::path::PathBuf;

#[derive(Deserialize)]
pub struct UpdatePinBody {
    pub pinned: bool,
}

#[derive(Deserialize)]
pub struct UpdateColorBody {
    /// A palette member (`red` / `amber` / `green`) sets the label; `null`
    /// clears it. Validated against `crate::session::is_valid_session_color`.
    #[serde(default)]
    pub color: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateArchiveBody {
    pub archived: bool,
    /// On archive, tear down every tmux session this instance owns. `false`
    /// keeps tmux state alive; structured-view supervisor shutdown is
    /// unconditional. Ignored when `archived = false` (#1868).
    #[serde(default = "default_kill_pane")]
    pub kill_pane: bool,
}

fn default_kill_pane() -> bool {
    true
}

#[derive(Deserialize)]
pub struct TrashSessionBody {
    /// On trash, tear down every tmux session this instance owns. `false` keeps
    /// tmux state alive; structured-view supervisor shutdown, which preserves
    /// the transcript, is unconditional. Defaults to `true`.
    #[serde(default = "default_kill_pane")]
    pub kill_pane: bool,
}

// A no-body trash request resolves through `unwrap_or_default()`, so `Default`
// must match the serde field default (`true`); the derived one would leave the
// pane running (#2523).
impl Default for TrashSessionBody {
    fn default() -> Self {
        Self {
            kill_pane: default_kill_pane(),
        }
    }
}

#[derive(Deserialize)]
pub struct UpdateSnoozeBody {
    /// `Some(positive minutes)` snoozes for that duration; `None` unsnoozes.
    /// Validated against `crate::session::validate_snooze_duration`, so the TUI
    /// dialog and CLI bounds apply here too.
    #[serde(default)]
    pub minutes: Option<u32>,
}

#[derive(Deserialize)]
pub struct UpdateUnreadBody {
    /// `true` flags the session manually unread; `false` marks it read,
    /// clearing both auto and manual markers. The auto-clear on view is driven
    /// separately by the client, which only fires it for an `auto` marker, so a
    /// `false` here never drops a manual flag the user meant to keep.
    pub unread: bool,
}

pub async fn update_session_pin(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdatePinBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    let profile = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return session_not_found();
        };
        inst.source_profile.clone()
    };

    let pinned = body.pinned;

    // Persist first; only mutate memory once disk is durable. See #1589.
    let persist_id = id.clone();
    if persist_session_update(
        profile,
        "pin update",
        state.file_watch.clone(),
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                if pinned {
                    inst.pin();
                } else {
                    inst.unpin();
                }
            }
        },
    )
    .await
    .is_err()
    {
        return persist_failed_response();
    }

    let mut instances = state.instances.write().await;
    let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
        tracing::warn!(
            target: "http.api.sessions",
            session = %id,
            "pin update: instance vanished after persist"
        );
        return crate::server::api::session_gone_after_persist();
    };
    if pinned {
        inst.pin();
    } else {
        inst.unpin();
    }

    let response =
        SessionResponse::from_instance(&*inst, crate::claude_settings::read_tui_fullscreen());
    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

pub async fn update_session_color(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdateColorBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    // Validate up front so an unknown color never reaches disk; `None` clears
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

    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    let profile = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return session_not_found();
        };
        inst.source_profile.clone()
    };

    // Persist first; only mutate memory once disk is durable. See #1589.
    let persist_id = id.clone();
    let persist_color = new_color.clone();
    if persist_session_update(
        profile,
        "color update",
        state.file_watch.clone(),
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                // Pre-validated above, so this cannot fail.
                let _ = inst.set_color(persist_color);
            }
        },
    )
    .await
    .is_err()
    {
        return persist_failed_response();
    }

    let mut instances = state.instances.write().await;
    let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
        tracing::warn!(
            target: "http.api.sessions",
            session = %id,
            "color update: instance vanished after persist"
        );
        return crate::server::api::session_gone_after_persist();
    };
    let _ = inst.set_color(new_color);

    let response =
        SessionResponse::from_instance(&*inst, crate::claude_settings::read_tui_fullscreen());
    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

pub async fn update_session_archive(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdateArchiveBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return session_not_found();
    };
    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    let expected = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|row| row.id == id) else {
            return session_not_found();
        };
        inst.clone()
    };
    let profile = expected.source_profile.clone();
    if !body.archived {
        let persist_id = id.clone();
        if persist_session_update_locked(
            profile,
            "unarchive update",
            state.file_watch.clone(),
            id.clone(),
            move |instances| {
                if let Some(row) = instances.iter_mut().find(|row| row.id == persist_id) {
                    row.unarchive();
                }
            },
        )
        .await
        .is_err()
        {
            return persist_failed_response();
        }
        let mut instances = state.instances.write().await;
        let Some(row) = instances.iter_mut().find(|row| row.id == id) else {
            return crate::server::api::session_gone_after_persist();
        };
        row.unarchive();
        return (
            StatusCode::OK,
            Json(serde_json::json!(SessionResponse::from_instance(
                row,
                crate::claude_settings::read_tui_fullscreen()
            ))),
        )
            .into_response();
    }

    let claimed = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let storage = expected
            .storage_origin
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("archive has no cached physical original"))?
            .as_ref()
            .clone();
        let stop = crate::session::runner_journal::reserve_owned_stop(&storage, &expected, false)?;
        Ok((storage, stop))
    })
    .await;
    let (storage, generation) = match claimed {
        Ok(Ok(claimed)) => claimed,
        Ok(Err(error)) => {
            return api_error(StatusCode::CONFLICT, "lifecycle_busy", error.to_string())
        }
        Err(_) => return persist_failed_response(),
    };
    // A reversible archive cancels admissions and settles runners, but never
    // sends session/delete or touches the transcript and checkout. No flock is
    // retained across either asynchronous wait.
    let settled = state
        .acp_supervisor
        .shutdown_and_require_dead(generation.clone())
        .await;
    if let Err(error) = settled {
        let _ = tokio::task::spawn_blocking(move || {
            crate::session::runner_journal::release_owned_stop(&generation)
        })
        .await;
        return api_error(StatusCode::CONFLICT, "teardown_pending", error.to_string());
    }
    let persist_id = id.clone();
    let kill_pane = body.kill_pane;
    let published = tokio::task::spawn_blocking(move || {
        crate::session::runner_journal::finish_owned_stop(&generation, |row| {
            if kill_pane {
                if let Err(error) = row.kill_locked() {
                    tracing::debug!(session = %persist_id, %error, "archive tmux teardown failed");
                }
                row.kill_ancillary_tmux_sessions_locked();
            }
            storage.update_native_under_workspace_claim_lock(|rows, _| {
                let row = rows
                    .iter_mut()
                    .find(|row| row.id == persist_id)
                    .ok_or_else(|| {
                        anyhow::anyhow!("session disappeared before archive publication")
                    })?;
                anyhow::ensure!(
                    row.lifecycle_generation == generation.generation(),
                    "archive generation was superseded"
                );
                row.archive();
                Ok(row.clone())
            })
        })
    })
    .await;
    let mut authoritative = match published {
        Ok(Ok(row)) => row,
        Ok(Err(error)) => {
            return api_error(StatusCode::CONFLICT, "archive_failed", error.to_string())
        }
        Err(_) => return persist_failed_response(),
    };
    let mut instances = state.instances.write().await;
    let Some(row) = instances.iter_mut().find(|row| row.id == id) else {
        return crate::server::api::session_gone_after_persist();
    };
    authoritative.merge_runtime_from_reload(row);
    *row = authoritative;
    (
        StatusCode::OK,
        Json(serde_json::json!(SessionResponse::from_instance(
            row,
            crate::claude_settings::read_tui_fullscreen()
        ))),
    )
        .into_response()
}

/// `POST /api/sessions/:id/trash`. Reserve durably before stopping without flocks,
/// then reacquire workspace -> identity -> lifecycle -> storage for relocation and commit.
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

    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return session_not_found();
    };
    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;
    let profile = {
        let instances = state.instances.read().await;
        let Some(instance) = instances.iter().find(|instance| instance.id == id) else {
            return session_not_found();
        };
        instance.source_profile.clone()
    };
    let recovery_profile = profile.clone();

    let reserve_profile = profile.clone();
    let reserve_id = id.clone();
    let file_watch = state.file_watch.clone();
    let (storage, generation, plan) = match tokio::task::spawn_blocking(
        move || -> anyhow::Result<_> {
            let _workspace_claim_lock = crate::session::acquire_session_workspace_claim_lock()?;
            let _identity_lock = crate::session::acquire_session_identity_lock()?;
            let storage = Storage::open(&reserve_profile, file_watch)?;
            let _lifecycle_lock = storage.acquire_instance_lifecycle_lock(&reserve_id)?;
            let snapshot = storage
                .load_strict_for_worktree_ownership_locked()?
                .into_iter()
                .find(|row| row.id == reserve_id)
                .ok_or_else(|| anyhow::anyhow!("session disappeared before trash"))?;
            ensure_trash_paths_unclaimed(&storage, &snapshot)?;
            let (generation, plan) = storage.update(|instances, _groups| {
                let instance = instances
                    .iter_mut()
                    .find(|row| row.id == reserve_id)
                    .ok_or_else(|| anyhow::anyhow!("session disappeared before trash"))?;
                anyhow::ensure!(
                    worktree_transition_plan_unchanged(&snapshot, instance),
                    "trash plan changed before reservation"
                );
                let generation = instance
                    .try_acquire_lifecycle_reservation(
                        LifecycleOperation::Trash,
                        Instance::LIFECYCLE_RESERVATION_TTL,
                        chrono::Utc::now(),
                    )
                    .map_err(anyhow::Error::new)?;
                instance.trash();
                Ok((generation, instance.clone()))
            })?;
            Ok((storage, generation, plan))
        },
    )
    .await
    {
        Ok(Ok(reserved)) => reserved,
        Ok(Err(error)) => {
            tracing::warn!(target: "http.api.sessions", session = %id, "trash reservation failed: {error}");
            return api_error(StatusCode::CONFLICT, "lifecycle_busy", error.to_string());
        }
        Err(error) => {
            tracing::error!(target: "http.api.sessions", session = %id, "trash reservation join failed: {error}");
            return persist_failed_response();
        }
    };

    let was_structured_view = plan.is_structured();
    {
        let mut instances = state.instances.write().await;
        let Some(instance) = instances.iter_mut().find(|instance| instance.id == id) else {
            return crate::server::api::session_gone_after_persist();
        };
        instance.trash();
        instance.lifecycle_generation = generation;
    }

    // Registry absence and daemon memory do not prove runner quiescence.
    let native = match crate::session::runner_journal::OwnedStop::from_claim(
        &storage,
        &plan,
        LifecycleOperation::Trash,
        generation,
    ) {
        Ok(native) => native,
        Err(error) => return api_error(StatusCode::CONFLICT, "lifecycle_busy", error.to_string()),
    };
    let relocation_allowed = match crate::session::runner_journal::settle(native).await {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!(target: "http.api.sessions", session = %id,
                "trash left the checkout in place: runner shutdown is unproven: {error}");
            false
        }
    };

    let work_id = id.clone();
    let kill_pane = body.kill_pane;
    let transition = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let _workspace_claim_lock = crate::session::acquire_session_workspace_claim_lock()?;
        let _identity_lock = crate::session::acquire_session_identity_lock()?;
        let storage = storage.reopen_preserving_watch()?;
        let _lifecycle_lock = storage.acquire_instance_lifecycle_lock(&work_id)?;
        let snapshot = storage
            .load_strict_for_worktree_ownership_locked()?
            .into_iter()
            .find(|row| row.id == work_id)
            .ok_or_else(|| anyhow::anyhow!("session disappeared during trash"))?;
        anyhow::ensure!(
            snapshot.is_trashed()
                && snapshot.lifecycle_reservation_is_owned(LifecycleOperation::Trash, generation)
                && worktree_transition_plan_unchanged(&plan, &snapshot),
            "trash reservation or relocation plan was superseded",
        );
        // Scan all profile ownership before storage.update takes this profile's storage flock.
        if relocation_allowed {
            if let Err(error) = ensure_trash_paths_unclaimed(&storage, &snapshot) {
                storage.update(|instances, _groups| {
                    if let Some(stored) = instances.iter_mut().find(|row| row.id == work_id) {
                        if stored
                            .lifecycle_reservation_is_owned(LifecycleOperation::Trash, generation)
                        {
                            stored.untrash();
                            stored.release_lifecycle_reservation_if_owned(
                                LifecycleOperation::Trash,
                                generation,
                            );
                        }
                    }
                    Ok(())
                })?;
                return Err(error);
            }
        }
        let outcome = storage.update(|instances, _groups| {
            let stored = instances
                .iter()
                .find(|row| row.id == work_id)
                .ok_or_else(|| anyhow::anyhow!("session disappeared before trash relocation"))?;
            anyhow::ensure!(
                stored.is_trashed()
                    && stored.lifecycle_reservation_is_owned(LifecycleOperation::Trash, generation)
                    && worktree_transition_plan_unchanged(&snapshot, stored),
                "trash reservation or relocation plan was superseded",
            );
            let outcome = if relocation_allowed && stored.runner_journal.proves_quiescent() {
                let mut instance = stored.clone();
                instance.source_profile = storage.profile().to_owned();
                if kill_pane {
                    if was_structured_view {
                        instance.kill_ancillary_tmux_sessions_locked();
                    } else {
                        instance.kill_all_tmux_sessions_locked();
                    }
                }
                let outcome = crate::session::trash::prepare_trashed_worktree(&mut instance);
                if matches!(
                    outcome,
                    crate::session::trash::RelocateOutcome::Relocated { .. }
                ) {
                    let relocation = crate::session::trash::TrashRelocation {
                        new_project_path: instance.project_path.clone(),
                        pre_trash_project_path: instance.pre_trash_project_path.clone(),
                    };
                    anyhow::ensure!(
                        crate::session::claim::commit_trash_relocation(
                            instances,
                            &work_id,
                            generation,
                            &relocation,
                        ) == crate::session::claim::RelocationCommit::Persisted,
                        "trash relocation reservation was superseded",
                    );
                }
                outcome
            } else {
                crate::session::trash::RelocateOutcome::Failed {
                    reason: "durable runner history does not prove checkout quiescence".to_string(),
                }
            };
            crate::session::claim::release_trash_reservation(instances, &work_id, generation);
            Ok(outcome)
        })?;
        let durable = storage.load()?.into_iter().find(|row| row.id == work_id);
        Ok((outcome, durable))
    })
    .await;

    let (outcome, durable) = match transition {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            tracing::warn!(target: "http.api.sessions", session = %id, "trash transition failed: {error}");
            let durable = tokio::task::spawn_blocking({
                let profile = recovery_profile.clone();
                let id = id.clone();
                move || {
                    Storage::open_unwatched(&profile)
                        .ok()?
                        .load()
                        .ok()?
                        .into_iter()
                        .find(|instance| instance.id == id)
                }
            })
            .await
            .ok()
            .flatten();
            if let Some(durable) = durable {
                let mut instances = state.instances.write().await;
                if let Some(instance) = instances.iter_mut().find(|instance| instance.id == id) {
                    instance.trashed_at = durable.trashed_at;
                    instance.project_path = durable.project_path;
                    instance.pre_trash_project_path = durable.pre_trash_project_path;
                    instance.lifecycle_generation = durable.lifecycle_generation;
                    instance.lifecycle_reservation = durable.lifecycle_reservation;
                }
            }
            return persist_failed_response();
        }
        Err(error) => {
            tracing::warn!(target: "http.api.sessions", session = %id, "trash transition join failed: {error}");
            return persist_failed_response();
        }
    };
    if let crate::session::trash::RelocateOutcome::Failed { reason } = outcome {
        tracing::warn!(
            target: "http.api.sessions",
            session = %id,
            "trash worktree relocation skipped: {reason}",
        );
    }

    let Some(durable) = durable else {
        return session_not_found();
    };
    let response = {
        let mut instances = state.instances.write().await;
        let Some(instance) = instances.iter_mut().find(|instance| instance.id == id) else {
            return crate::server::api::session_gone_after_persist();
        };
        instance.trashed_at = durable.trashed_at;
        instance.project_path = durable.project_path;
        instance.pre_trash_project_path = durable.pre_trash_project_path;
        instance.lifecycle_generation = durable.lifecycle_generation;
        instance.lifecycle_reservation = durable.lifecycle_reservation;
        SessionResponse::from_instance(instance, crate::claude_settings::read_tui_fullscreen())
    };
    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

/// `POST /api/sessions/:id/restore`. Reserve before any required runner stop,
/// then move and untrash atomically under canonical ownership and storage locks.
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

    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;
    let profile = {
        let instances = state.instances.read().await;
        let Some(instance) = instances.iter().find(|instance| instance.id == id) else {
            return session_not_found();
        };
        instance.source_profile.clone()
    };

    enum RestoreTransitionError {
        NotFound,
        Busy(String),
        Worktree(String),
        Persist(String),
    }

    let restore_profile = profile.clone();
    let restore_id = id.clone();
    let file_watch = state.file_watch.clone();
    let restored: Result<Instance, RestoreTransitionError> = async {
        let (storage, generation, plan) =
            tokio::task::spawn_blocking(move || -> Result<_, RestoreTransitionError> {
                let _workspace_claim_lock = crate::session::acquire_session_workspace_claim_lock()
                    .map_err(|error| RestoreTransitionError::Persist(error.to_string()))?;
                let _identity_lock = crate::session::acquire_session_identity_lock()
                    .map_err(|error| RestoreTransitionError::Persist(error.to_string()))?;
                let storage = Storage::open(&restore_profile, file_watch)
                    .map_err(|error| RestoreTransitionError::Persist(error.to_string()))?;
                let _lifecycle_lock = storage
                    .acquire_instance_lifecycle_lock(&restore_id)
                    .map_err(|error| RestoreTransitionError::Persist(error.to_string()))?;
                let (decision, plan) = storage
                    .update(|instances, _groups| {
                        let decision = crate::session::claim::decide_restore_claim(
                            instances,
                            &restore_id,
                            chrono::Utc::now(),
                        )
                        .map_err(anyhow::Error::new)?;
                        let plan = instances.iter().find(|row| row.id == restore_id).cloned();
                        Ok((decision, plan))
                    })
                    .map_err(|error| RestoreTransitionError::Persist(error.to_string()))?;
                let generation = match decision {
                    crate::session::claim::RestoreClaimDecision::Claimed(generation) => generation,
                    crate::session::claim::RestoreClaimDecision::AlreadyGone => {
                        return Err(RestoreTransitionError::NotFound)
                    }
                    crate::session::claim::RestoreClaimDecision::Busy(holder) => {
                        return Err(RestoreTransitionError::Busy(holder.busy_reason()))
                    }
                };
                let plan = plan.ok_or(RestoreTransitionError::NotFound)?;
                Ok((storage, generation, plan))
            })
            .await
            .map_err(|error| RestoreTransitionError::Persist(error.to_string()))??;
        let needs_move = plan
            .pre_trash_project_path
            .as_ref()
            .is_some_and(|original| original != &plan.project_path);
        let settled = if needs_move {
            let native = crate::session::runner_journal::OwnedStop::from_claim(
                &storage,
                &plan,
                LifecycleOperation::Restore,
                generation,
            )
            .map_err(|error| RestoreTransitionError::Persist(error.to_string()))?;
            crate::session::runner_journal::settle(native).await
        } else {
            Ok(())
        };
        let work_id = id.clone();
        tokio::task::spawn_blocking(move || -> Result<Instance, RestoreTransitionError> {
            let _workspace_claim_lock = crate::session::acquire_session_workspace_claim_lock()
                .map_err(|error| RestoreTransitionError::Persist(error.to_string()))?;
            let _identity_lock = crate::session::acquire_session_identity_lock()
                .map_err(|error| RestoreTransitionError::Persist(error.to_string()))?;
            let storage = storage
                .reopen_preserving_watch()
                .map_err(|error| RestoreTransitionError::Persist(error.to_string()))?;
            let _lifecycle_lock = storage
                .acquire_instance_lifecycle_lock(&work_id)
                .map_err(|error| RestoreTransitionError::Persist(error.to_string()))?;
            if let Err(error) = settled {
                release_restore_claim(&storage, &work_id, generation);
                return Err(RestoreTransitionError::Worktree(format!(
                    "runner shutdown is unproven: {error}"
                )));
            }
            let snapshot = storage
                .load_strict_for_worktree_ownership_locked()
                .map_err(|error| RestoreTransitionError::Persist(error.to_string()))?
                .into_iter()
                .find(|row| row.id == work_id)
                .ok_or(RestoreTransitionError::NotFound)?;
            if !snapshot.lifecycle_reservation_is_owned(LifecycleOperation::Restore, generation)
                || !worktree_transition_plan_unchanged(&plan, &snapshot)
            {
                release_restore_claim(&storage, &work_id, generation);
                return Err(RestoreTransitionError::Busy(
                    crate::session::NEWER_GENERATION_BUSY_REASON.to_string(),
                ));
            }
            // Read the cross-profile inventory outside storage.update to avoid recursive flock.
            if needs_move {
                let paths = vec![
                    PathBuf::from(&snapshot.project_path),
                    PathBuf::from(snapshot.pre_trash_project_path.as_ref().unwrap()),
                ];
                if let Err(error) = crate::session::deletion::ensure_unclaimed_paths(
                    crate::session::deletion::SessionPathOwner {
                        profile: storage.profile(),
                        session_id: &work_id,
                    },
                    &paths,
                ) {
                    release_restore_claim(&storage, &work_id, generation);
                    return Err(RestoreTransitionError::Worktree(format!(
                        "worktree ownership could not be verified: {error}"
                    )));
                }
            }
            let result = storage.update(|instances, _groups| {
                let Some(stored) = instances.iter_mut().find(|row| row.id == work_id) else {
                    return Ok(Err(RestoreTransitionError::NotFound));
                };
                if !stored.lifecycle_reservation_is_owned(LifecycleOperation::Restore, generation)
                    || !worktree_transition_plan_unchanged(&snapshot, stored)
                {
                    stored.release_lifecycle_reservation_if_owned(
                        LifecycleOperation::Restore,
                        generation,
                    );
                    return Ok(Err(RestoreTransitionError::Busy(
                        crate::session::NEWER_GENERATION_BUSY_REASON.to_string(),
                    )));
                }
                let mut instance = stored.clone();
                instance.source_profile = storage.profile().to_owned();
                if let crate::session::trash::RestoreOutcome::Failed { reason } =
                    crate::session::trash::restore_worktree_location(&mut instance)
                {
                    stored.release_lifecycle_reservation_if_owned(
                        LifecycleOperation::Restore,
                        generation,
                    );
                    return Ok(Err(RestoreTransitionError::Worktree(reason)));
                }
                anyhow::ensure!(
                    crate::session::claim::finalize_restore_commit(
                        instances,
                        &work_id,
                        generation,
                        &instance.project_path,
                        &instance.pre_trash_project_path,
                    ) == crate::session::claim::RestoreCommit::Committed,
                    "restore reservation was superseded during worktree move",
                );
                Ok(Ok(instances
                    .iter()
                    .find(|row| row.id == work_id)
                    .unwrap()
                    .clone()))
            });
            if result.is_err() {
                release_restore_claim(&storage, &work_id, generation);
            }
            result.map_err(|error| RestoreTransitionError::Persist(error.to_string()))?
        })
        .await
        .map_err(|error| RestoreTransitionError::Persist(error.to_string()))?
    }
    .await;

    let restored = match restored {
        Ok(instance) => instance,
        Err(RestoreTransitionError::NotFound) => return session_not_found(),
        Err(RestoreTransitionError::Busy(holder)) => {
            return api_error(
                StatusCode::CONFLICT,
                "lifecycle_busy",
                format!("Session is {holder}, so it was not restored"),
            );
        }
        Err(RestoreTransitionError::Worktree(reason)) => {
            return api_error(
                StatusCode::CONFLICT,
                "worktree_restore_failed",
                format!("Could not restore the worktree: {reason}"),
            );
        }
        Err(RestoreTransitionError::Persist(error)) => {
            tracing::warn!(target: "http.api.sessions", session = %id, "restore transition failed: {error}");
            return persist_failed_response();
        }
    };

    let response = {
        let mut instances = state.instances.write().await;
        let Some(instance) = instances.iter_mut().find(|instance| instance.id == id) else {
            return crate::server::api::session_gone_after_persist();
        };
        instance.project_path = restored.project_path;
        instance.pre_trash_project_path = restored.pre_trash_project_path;
        instance.lifecycle_generation = restored.lifecycle_generation;
        instance.lifecycle_reservation = restored.lifecycle_reservation;
        instance.untrash();
        SessionResponse::from_instance(instance, crate::claude_settings::read_tui_fullscreen())
    };
    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

fn worktree_transition_plan_unchanged(snapshot: &Instance, durable: &Instance) -> bool {
    snapshot.is_trashed() == durable.is_trashed()
        && snapshot.project_path == durable.project_path
        && snapshot.pre_trash_project_path == durable.pre_trash_project_path
        && snapshot.worktree_info == durable.worktree_info
        && snapshot.scratch == durable.scratch
        && snapshot
            .workspace_info
            .as_ref()
            .map(|workspace| (&workspace.workspace_dir, &workspace.repos))
            == durable
                .workspace_info
                .as_ref()
                .map(|workspace| (&workspace.workspace_dir, &workspace.repos))
        && snapshot.is_sandboxed() == durable.is_sandboxed()
}

fn ensure_trash_paths_unclaimed(storage: &Storage, instance: &Instance) -> anyhow::Result<()> {
    if !instance.has_managed_worktree_or_workspace() {
        return Ok(());
    }
    let mut paths = vec![PathBuf::from(&instance.project_path)];
    if let Some(workspace) = &instance.workspace_info {
        paths.push(PathBuf::from(&workspace.workspace_dir));
    }
    paths.extend(
        instance
            .all_repos()
            .iter()
            .map(|repo| PathBuf::from(&repo.worktree_path)),
    );
    if let Some(original) = &instance.pre_trash_project_path {
        paths.push(PathBuf::from(original));
    } else if let Some(holding) = crate::session::trash::trash_holding_path(
        std::path::Path::new(&instance.project_path),
        &instance.id,
    ) {
        paths.push(holding);
    }
    crate::session::deletion::ensure_unclaimed_paths(
        crate::session::deletion::SessionPathOwner {
            profile: storage.profile(),
            session_id: &instance.id,
        },
        &paths,
    )
    .map_err(|error| {
        anyhow::anyhow!("trash skipped because worktree ownership is shared or unknown: {error}")
    })
}

// The caller holds workspace -> identity -> lifecycle locks.
fn release_restore_claim(storage: &Storage, id: &str, generation: u64) {
    let _ = storage.update(|instances, _groups| {
        if let Some(stored) = instances.iter_mut().find(|row| row.id == id) {
            stored.release_lifecycle_reservation_if_owned(LifecycleOperation::Restore, generation);
        }
        Ok(())
    });
}

/// `POST /api/sessions/:id/smart-rename`. Manual "Auto-name now" for a
/// structured session: clears the per-session attempted gate and regenerates the
/// title from the first prompt, even over one already chosen. The rename runs
/// detached and best-effort: a `202` means "re-run started", not "renamed".
pub async fn force_smart_rename(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    if let Some(resp) = crate::server::api::read_only_block(&state) {
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
        return session_not_found();
    };

    // Preflight the SAME gate the spawned try_smart_rename re-applies, so this
    // never reports 202 for a session the gate would silently drop. Resolves
    // with the same repo-aware config the worker uses, so a repo-local
    // smart_rename_agent or agent_command_override cannot make the two
    // disagree. `setting_on` and `force` are true because the manual
    // "Auto-name now" action runs even when auto-rename-on-start is off (#3039)
    // and regenerates over any title; the spawned job gets `force = true` too.
    let resolved = crate::session::config::repo_config::resolve_config_with_repo_or_warn(
        &profile,
        std::path::Path::new(&project_path),
    );
    let config = &resolved.session;
    if let Err(reason) = crate::session::smart_rename::check_eligible_resolved(
        structured,
        true,
        true,
        &title,
        &tool,
        &config.smart_rename_agent,
        sandboxed,
        &command,
        &config.agent_command_override,
    ) {
        use crate::session::smart_rename::SkipReason;
        // Wording comes from the shared `user_message` so this response and the
        // TUI dialog cannot drift; only the status code is per-reason.
        let status = match reason {
            SkipReason::NotStructured => StatusCode::BAD_REQUEST,
            _ => StatusCode::CONFLICT,
        };
        return (
            status,
            Json(serde_json::json!({ "message": reason.user_message() })),
        )
            .into_response();
    }

    // A sandboxed session's one-shot runs inside its container, so a stopped
    // container is the remaining way the spawned job would drop the session
    // after the static gate passed. Without probing here this would answer 202
    // while nothing renames. The spawned try_smart_rename re-probes and stays
    // the authority.
    if sandboxed {
        use crate::containers::Probe;
        let sid = id.clone();
        let probe = tokio::task::spawn_blocking(move || {
            crate::containers::DockerContainer::from_session_id(&sid).probe_running()
        })
        .await;
        // A failed inspection is not a stopped container: telling the user to
        // start one that may already be running sends them the wrong way, so the
        // runtime error is its own state. Same split as the TUI preflight.
        let unknown = match probe {
            Ok(Probe::Running) => None,
            Ok(Probe::NotRunning) => {
                return api_error(StatusCode::CONFLICT, "container_not_running", "The session's sandbox container is not running, so its agent cannot be asked for a name. Open the session to start it, then try again.");
            }
            Ok(Probe::Unknown(e)) => Some(e.to_string()),
            Err(e) => Some(e.to_string()),
        };
        if let Some(err) = unknown {
            return api_error(StatusCode::SERVICE_UNAVAILABLE, "container_state_unknown", format!("Couldn't check the session's sandbox container, so its agent cannot be asked for a name: {err}"));
        }
    }

    let Some((first_user_prompt, agent_prose)) = state
        .acp_event_store
        .first_turn_context(&id, crate::session::smart_rename::FIRST_TURN_AGENT_BYTES)
    else {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "message": "No prompt to name this session from yet" })),
        )
            .into_response();
    };
    let context = crate::session::smart_rename::render_first_turn(&first_user_prompt, &agent_prose);

    // Clear the attempted gate so try_smart_rename does not short-circuit on a
    // prior failed attempt. Its inflight guard still prevents a concurrent run.
    {
        let mut attempted = state
            .smart_rename_attempted
            .lock()
            .expect("smart_rename_attempted poisoned");
        attempted.remove(&id);
    }

    tokio::spawn(crate::session::smart_rename::try_smart_rename(
        state.clone(),
        id.clone(),
        crate::session::smart_rename::SmartRenameInput {
            first_user_prompt,
            context,
        },
        // Manual action forces past the smart_rename-disabled gate (#3039).
        true,
    ));
    StatusCode::ACCEPTED.into_response()
}

/// On-demand "summarize the conversation so far" for a structured-view session.
/// Preflights the same eligibility gate the spawned task re-applies, then runs
/// the summary one-shot detached. A `202` means "summary started"; the result
/// arrives later as a `ConversationSummary` event over the structured-view WS.
/// Bypasses the `conversation_summary` setting and the delta threshold, since
/// an explicit request always runs if the session is eligible (#2808).
pub async fn summarize_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    if let Some(resp) = crate::server::api::read_only_block(&state) {
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

    tokio::spawn(
        crate::session::conversation_summary::try_conversation_summary(
            state.clone(),
            id.clone(),
            crate::session::conversation_summary::SummaryTrigger::Manual,
        ),
    );
    StatusCode::ACCEPTED.into_response()
}

/// Stop a session, matching the TUI's `x` keybind: kill the tmux pane and stop
/// (not remove) the Docker container for plain sessions, shut the worker down
/// for structured ones. The record is preserved with status `Stopped`. NOT a
/// delete.
pub async fn stop_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    if state.read_only {
        return crate::server::api::read_only_response();
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

    // Snapshot profile, session type and current status without mutating, so a
    // persist failure leaves disk and memory in agreement.
    let (profile, is_structured, already_stopped, expected) = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return session_not_found();
        };

        let structured = inst.is_structured();
        // Mirror the TUI's `stop_selected` guard: a session already stopped or
        // mid-lifecycle has nothing to stop.
        let already = matches!(
            inst.status,
            Status::Stopped | Status::Deleting | Status::Creating
        );
        (
            inst.source_profile.clone(),
            structured,
            already,
            inst.clone(),
        )
    };

    if already_stopped {
        let instances = state.instances.read().await;
        let response = match instances.iter().find(|i| i.id == id) {
            Some(inst) => {
                SessionResponse::from_instance(inst, crate::claude_settings::read_tui_fullscreen())
            }
            None => {
                return session_not_found();
            }
        };
        return (StatusCode::OK, Json(serde_json::json!(response))).into_response();
    }

    // Structured sessions have no tmux/container teardown transaction, so
    // persist their dormant stop before asking the supervisor to shut down.
    // Plain sessions delegate the full sequence to `Instance::stop` below.
    let native_stop = if is_structured {
        let original = match state.capture_operation_origin(&expected) {
            Ok(original) => original,
            Err(error) => return (StatusCode::CONFLICT, error.to_string()).into_response(),
        };
        let stop = match crate::session::runner_journal::reserve_stop_from_origin(original, false) {
            Ok(stop) => stop,
            Err(error) => return (StatusCode::CONFLICT, error.to_string()).into_response(),
        };
        let owner = stop.clone();
        let saved = tokio::task::spawn_blocking(move || {
            owner.update_projection(|row| {
                row.status = Status::Stopped;
                row.mark_idle_dormant();
                Ok(())
            })
        })
        .await;
        if !matches!(saved, Ok(Ok(()))) {
            return persist_failed_response();
        }
        Some(stop)
    } else {
        None
    };

    let inst_clone = {
        let mut instances = state.instances.write().await;
        let Some(inst) = instances
            .iter_mut()
            .find(|instance| match native_stop.as_ref() {
                Some(stop) => {
                    stop.original().matches_instance(instance)
                        || stop.cancellation_origin().matches_instance(instance)
                }
                None => instance.id == id,
            })
        else {
            tracing::warn!(
                target: "http.api.sessions",
                session = %id,
                "stop session: instance vanished before teardown"
            );
            return crate::server::api::session_gone_after_persist();
        };
        if is_structured {
            inst.status = Status::Stopped;
            inst.mark_idle_dormant();
            // A direct stop bypasses apply_status_intent, which normally releases this on
            // reaching a terminal status; do the same here, on the live in-memory row (the
            // disk-persisted copy above is a fresh load, so this field is always false there).
            inst.plugin_revival_pending = false;
            inst.lifecycle_generation = native_stop
                .as_ref()
                .expect("structured stop reserved its original scope")
                .generation();
        }
        inst.clone()
    };

    if is_structured {
        // Structured view: shut down the worker so the reconciler does not race
        // to respawn it. `shutdown` preserves the transcript.
        let stop = native_stop.expect("structured stop reserved its original scope");
        match state.acp_supervisor.shutdown(stop.clone()).await {
            Ok(()) => {
                if let Err(error) = crate::session::runner_journal::release_owned_stop(&stop) {
                    tracing::warn!(%error, "original stop claim remains protected");
                }
            }
            Err(e) => tracing::warn!(
                target: "acp.supervisor",
                session = %id,
                "shutdown during stop failed: {e}"
            ),
        }
    } else {
        // Plain session: kill the tmux pane and stop (not remove) the Docker
        // container. `Instance::stop` can block ~10s on `docker stop`, so it
        // runs off the async runtime.
        let inst_for_stop = inst_clone.clone();
        let stop_profile = profile.clone();
        let stop_id = id.clone();
        match tokio::task::spawn_blocking(move || {
            let stop_result = inst_for_stop.stop();
            let disk_result = Storage::new_unwatched(&stop_profile)
                .and_then(|storage| storage.load())
                .map(|instances| {
                    instances
                        .into_iter()
                        .find(|instance| instance.id == stop_id)
                });
            (stop_result, disk_result)
        })
        .await
        {
            Ok((stop_result, disk_result)) => {
                if let Err(e) = stop_result {
                    tracing::warn!(target: "http.api.sessions", "Stop: session stop failed: {e}");
                }
                match disk_result {
                    Ok(Some(stopped)) => {
                        let mut instances = state.instances.write().await;
                        if let Some(live) = instances.iter_mut().find(|instance| instance.id == id)
                        {
                            live.merge_post_start(&stopped);
                        }
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!(
                        target: "http.api.sessions",
                        "Stop: failed to reload lifecycle generation: {e}"
                    ),
                }
            }
            Err(e) => tracing::warn!(
                target: "http.api.sessions",
                "Stop: stop join failed: {e}"
            ),
        }
    }

    // Re-read so the response reflects the Stopped status.
    let instances = state.instances.read().await;
    let response = match instances.iter().find(|i| i.id == id) {
        Some(inst) => {
            SessionResponse::from_instance(inst, crate::claude_settings::read_tui_fullscreen())
        }
        None => {
            return session_not_found();
        }
    };
    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

/// Start (resume) a stopped session, the inverse of [`stop_session`]. Plain
/// sessions restart exactly like `ensure_session`; structured sessions are
/// un-parked by clearing the idle-dormant mark so the acp reconciler respawns
/// the worker on its next tick. No-op for a session that is not stopped.
pub async fn start_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    if state.read_only {
        return crate::server::api::read_only_response();
    }

    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    let (profile, is_structured, is_stopped, instance) = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return session_not_found();
        };

        if let Err(blocked) = inst.ensure_startable() {
            return crate::server::api::start_blocked_response(blocked);
        }
        let structured = inst.is_structured();
        (
            inst.source_profile.clone(),
            structured,
            matches!(inst.status, Status::Stopped),
            inst.clone(),
        )
    };

    // Only a stopped session has anything to start; otherwise return current,
    // unless a peer has since shelved or purged the stored row.
    if !is_stopped {
        match crate::server::api::load_persisted_instance(&state, &profile, &id).await {
            Ok(Some(stored)) => {
                if let Err(blocked) = stored.ensure_startable() {
                    return crate::server::api::start_blocked_response(blocked);
                }
            }
            Ok(None) => return session_not_found(),
            Err(resp) => return resp,
        }
        let instances = state.instances.read().await;
        let response = match instances.iter().find(|i| i.id == id) {
            Some(inst) => {
                SessionResponse::from_instance(inst, crate::claude_settings::read_tui_fullscreen())
            }
            None => {
                return session_not_found();
            }
        };
        return (StatusCode::OK, Json(serde_json::json!(response))).into_response();
    }

    if is_structured {
        // Un-park: clear the dormant mark and drop the Stopped status so the
        // reconciler's next tick respawns the worker against the preserved
        // transcript.
        let persist_id = id.clone();
        // Unset when a peer purged the row; `Err` when it archived or trashed it since the
        // memory check.
        let stored = Arc::new(std::sync::OnceLock::new());
        let stored_on_disk = Arc::clone(&stored);
        if persist_session_update(
            profile,
            "start session",
            state.file_watch.clone(),
            move |instances| {
                if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                    let startable = inst.ensure_startable();
                    let _ = stored_on_disk.set(startable);
                    if startable.is_err() {
                        return;
                    }
                    inst.idle_dormant_since = None;
                    inst.status = Status::Idle;
                    inst.last_error = None;
                }
            },
        )
        .await
        .is_err()
        {
            return persist_failed_response();
        }
        match stored.get() {
            None => return session_not_found(),
            Some(Err(refusal)) => return crate::server::api::start_blocked_response(*refusal),
            Some(Ok(())) => {}
        }
        {
            let mut instances = state.instances.write().await;
            if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                inst.idle_dormant_since = None;
                inst.status = Status::Idle;
                inst.last_error = None;
            }
        }
        let instances = state.instances.read().await;
        let response = match instances.iter().find(|i| i.id == id) {
            Some(inst) => {
                SessionResponse::from_instance(inst, crate::claude_settings::read_tui_fullscreen())
            }
            None => {
                return session_not_found();
            }
        };
        return (StatusCode::OK, Json(serde_json::json!(response))).into_response();
    }

    // Plain session: restart the tmux pane, mirroring ensure_session. Show
    // Starting immediately so the status poller does not flip it back while the
    // restart is in flight.
    {
        let mut instances = state.instances.write().await;
        if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
            inst.status = Status::Starting;
            inst.last_error = None;
        }
    }

    let sync_base = instance.clone();
    let restart_result = tokio::task::spawn_blocking(
        move || -> Result<(Instance, crate::session::StartOutcome), Box<(Instance, anyhow::Error)>> {
            let mut inst = instance;
            // Explicit restart endpoint: honor auto_resume_on_restart, same as
            // TUI `e`/`Enter`. The instance-level cascade holds the lifecycle
            // lock across final poller drain, exact-pane OMP capture, kill and
            // relaunch.
            match inst.restart_with_resume_policy(
                None,
                false,
                crate::session::ResumeAttemptPolicy::HonorAutoResumeSetting,
            ) {
                Ok(outcome) => Ok((inst, outcome)),
                Err(e) => Err(Box::new((inst, e))),
            }
        },
    )
    .await;

    match restart_result {
        Ok(Ok((started, outcome))) => {
            let resume_failed_sid = match &outcome {
                crate::session::StartOutcome::ResumeFailed { sid } => Some(sid.clone()),
                _ => None,
            };
            let mut instances = state.instances.write().await;
            let response = match instances.iter_mut().find(|i| i.id == id) {
                Some(inst) => {
                    apply_post_restart_sync(inst, &sync_base, &started);
                    SessionResponse::from_instance(
                        inst,
                        crate::claude_settings::read_tui_fullscreen(),
                    )
                }
                None => {
                    return session_not_found();
                }
            };
            if let Some(sid) = resume_failed_sid {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "error": "resume_failed",
                        "message": format!("Resume failed for sid {sid}; preserved for explicit retry"),
                        "resume_session_id": sid,
                    })),
                )
                    .into_response();
            }
            (StatusCode::OK, Json(response)).into_response()
        }
        Ok(Err(boxed)) => {
            let (started, e) = *boxed;
            let blocked = e.downcast_ref::<crate::session::StartBlocked>().copied();
            let msg = e.to_string();
            tracing::warn!(target: "http.api.sessions", "start_session restart failed for {id}: {msg}");
            let mut instances = state.instances.write().await;
            if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                if apply_post_restart_sync(inst, &sync_base, &started) && blocked.is_none() {
                    inst.status = Status::Error;
                    inst.last_error = Some(msg.clone());
                }
            }
            if let Some(blocked) = blocked {
                return crate::server::api::start_blocked_response(blocked);
            }
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "restart_failed", msg)
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "start_session panicked for {id}: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal"})),
            )
                .into_response()
        }
    }
}

pub async fn update_session_snooze(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdateSnoozeBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    // The TUI dialog presets, CLI and this endpoint share the same bounds; see
    // `crate::session::config::validate_snooze_duration`.
    if let Some(minutes) = body.minutes {
        if let Err(msg) = crate::session::validate_snooze_duration(minutes as u64) {
            return api_error(StatusCode::BAD_REQUEST, "validation_failed", msg);
        }
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

    let (was_structured_view, profile, expected) = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return session_not_found();
        };

        let structured_view = inst.is_structured();
        (structured_view, inst.source_profile.clone(), inst.clone())
    };

    let minutes = body.minutes;
    let native_stop = if was_structured_view && minutes.is_some() {
        let original = match state.capture_operation_origin(&expected) {
            Ok(original) => original,
            Err(error) => return (StatusCode::CONFLICT, error.to_string()).into_response(),
        };
        match crate::session::runner_journal::reserve_stop_from_origin(original, false) {
            Ok(stop) => Some(stop),
            Err(error) => return (StatusCode::CONFLICT, error.to_string()).into_response(),
        }
    } else {
        None
    };

    // Persist first; only mutate memory once disk is durable, and fire the
    // structured teardown below only on a write that landed (#1589).
    let persist_id = id.clone();
    let saved = if let Some(owner) = native_stop.as_ref().cloned() {
        tokio::task::spawn_blocking(move || {
            owner.update_projection(|row| {
                row.snooze(minutes.expect("snooze stop has a duration"));
                Ok(())
            })
        })
        .await
        .map_err(|error| error.to_string())
        .and_then(|result| result.map_err(|error| error.to_string()))
    } else {
        persist_session_update(
            profile,
            "snooze update",
            state.file_watch.clone(),
            move |instances| {
                if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                    match minutes {
                        Some(m) => inst.snooze(m),
                        None => inst.unsnooze(),
                    }
                }
            },
        )
        .await
        .map_err(|()| "snooze update did not commit".to_owned())
    };
    if saved.is_err() {
        return persist_failed_response();
    }

    {
        let mut instances = state.instances.write().await;
        let Some(inst) = instances
            .iter_mut()
            .find(|instance| match native_stop.as_ref() {
                Some(stop) => {
                    stop.original().matches_instance(instance)
                        || stop.cancellation_origin().matches_instance(instance)
                }
                None => {
                    instance.id == id
                        && instance.same_storage_origin(&expected)
                        && instance.created_at == expected.created_at
                        && instance.lifecycle_generation == expected.lifecycle_generation
                }
            })
        else {
            tracing::warn!(
                target: "http.api.sessions",
                session = %id,
                "snooze update: instance vanished after persist"
            );
            return crate::server::api::session_gone_after_persist();
        };
        match minutes {
            Some(m) => inst.snooze(m),
            None => inst.unsnooze(),
        }
        if let Some(stop) = native_stop.as_ref() {
            inst.lifecycle_generation = stop.generation();
        }
    }

    // Snoozing tears a structured worker down the way archive does: snooze is
    // a temporary archive, and the worker is too heavy to keep idle while the
    // row is sunk. The reconciler skips snoozed sessions and re-picks them on
    // the first tick after expiry. `shutdown` preserves the transcript, so that
    // respawn resumes the conversation (#1710).
    if was_structured_view && minutes.is_some() {
        let stop = native_stop.expect("structured snooze reserved its original stop");
        match state.acp_supervisor.shutdown(stop.clone()).await {
            Ok(()) => {
                if let Err(error) = crate::session::runner_journal::release_owned_stop(&stop) {
                    tracing::warn!(%error, "original snooze claim remains protected");
                }
            }
            Err(e) => tracing::warn!(
                target: "acp.supervisor",
                session = %id,
                "shutdown during snooze failed: {e}"
            ),
        }
    }

    let instances = state.instances.read().await;
    let response = match instances.iter().find(|i| i.id == id) {
        Some(inst) => {
            SessionResponse::from_instance(inst, crate::claude_settings::read_tui_fullscreen())
        }
        None => {
            return session_not_found();
        }
    };
    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

/// `PATCH /api/sessions/{id}/unread`: flag a session unread or mark it read.
/// The client computes the target from current state rather than toggling
/// server-side, so an optimistic UI update cannot desync. No-op when
/// `session.unread_indicator` is off. Persist-then-mutate, like snooze.
pub async fn update_session_unread(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdateUnreadBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    let mark_unread = body.unread;

    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    let profile = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return session_not_found();
        };
        inst.source_profile.clone()
    };

    // Feature off: report the current state without mutating, matching the
    // TUI's no-op when `session.unread_indicator` is disabled.
    if crate::session::unread_enabled() {
        let persist_id = id.clone();
        if persist_session_update(
            profile,
            "unread update",
            state.file_watch.clone(),
            move |instances| {
                if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                    if mark_unread {
                        inst.mark_unread();
                    } else {
                        inst.mark_read();
                    }
                }
            },
        )
        .await
        .is_err()
        {
            return persist_failed_response();
        }

        let mut instances = state.instances.write().await;
        let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
            tracing::warn!(
                target: "http.api.sessions",
                session = %id,
                "unread update: instance vanished after persist"
            );
            return crate::server::api::session_gone_after_persist();
        };
        if mark_unread {
            inst.mark_unread();
        } else {
            inst.mark_read();
        }
    }

    let instances = state.instances.read().await;
    let response = match instances.iter().find(|i| i.id == id) {
        Some(inst) => {
            SessionResponse::from_instance(inst, crate::claude_settings::read_tui_fullscreen())
        }
        None => {
            return session_not_found();
        }
    };
    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}
