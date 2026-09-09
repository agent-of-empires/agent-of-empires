//! Pin, color, archive, trash/restore, rename/summarize triggers, and
//! stop/start/snooze/unread endpoints.

use super::*;

#[derive(Deserialize)]
pub struct UpdatePinBody {
    pub pinned: bool,
}

#[derive(Deserialize)]
pub struct UpdateColorBody {
    /// A palette member (`red` / `amber` / `green`) sets the label; `null` (or
    /// a missing field) clears it. Validated against
    /// `crate::session::is_valid_session_color`, matching the CLI.
    #[serde(default)]
    pub color: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateArchiveBody {
    pub archived: bool,
    /// On archive, tear down every tmux session this instance owns. `false`
    /// keeps tmux state alive; structured-view supervisor shutdown is
    /// unconditional. Ignored when `archived = false`. See #1868.
    #[serde(default = "default_kill_pane")]
    pub kill_pane: bool,
}

fn default_kill_pane() -> bool {
    true
}

#[derive(Deserialize)]
pub struct TrashSessionBody {
    /// On trash, tear down every tmux session this instance owns. `false`
    /// keeps tmux state alive; structured-view supervisor shutdown (which
    /// preserves the transcript) is unconditional. Defaults to `true`.
    #[serde(default = "default_kill_pane")]
    pub kill_pane: bool,
}

// A no-body trash request resolves through `unwrap_or_default()`, so `Default`
// must match the serde field default (`true`). The derived `Default` would use
// `bool::default()` (`false`) and silently leave the pane running (#2523).
impl Default for TrashSessionBody {
    fn default() -> Self {
        Self {
            kill_pane: default_kill_pane(),
        }
    }
}

#[derive(Deserialize)]
pub struct UpdateSnoozeBody {
    /// `Some(positive minutes)` snoozes for that duration. `None` (or a
    /// missing field) unsnoozes. Validated against
    /// `crate::session::validate_snooze_duration` so the same bounds the
    /// TUI dialog and CLI use also apply here.
    #[serde(default)]
    pub minutes: Option<u32>,
}

#[derive(Deserialize)]
pub struct UpdateUnreadBody {
    /// `true` flags the session manually unread (a deliberate "flag for
    /// later"); `false` marks it read, clearing both auto and manual markers.
    /// The clear is the explicit one (web "Mark as read"); the auto-clear on
    /// view is driven separately by the client, which only fires it for an
    /// `auto` marker, so a `false` here never silently drops a manual flag the
    /// user meant to keep.
    pub unread: bool,
}

pub async fn update_session_pin(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdatePinBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    // CityHall: only act on structured sessions this mode created; refuse a
    // non-structured (or unknown) target so a locked-down client cannot
    // respawn/destroy/edit an enumerated plain session. See #7.
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
            return crate::server::api::session_not_found();
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
    // CityHall: only act on structured sessions this mode created; refuse a
    // non-structured (or unknown) target so a locked-down client cannot
    // respawn/destroy/edit an enumerated plain session. See #7.
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

    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    let profile = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return crate::server::api::session_not_found();
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
    // CityHall: only act on structured sessions this mode created; refuse a
    // non-structured (or unknown) target so a locked-down client cannot
    // respawn/destroy/edit an enumerated plain session. See #7.
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

    // Worker-stopping barrier: submission guard before `instance_lock`, per
    // `prompt_submission` (#3650).
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return crate::server::api::session_not_found();
    };
    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    // Read the profile without mutating memory yet. Persisting first means
    // a storage failure returns 500 with disk and memory still in
    // agreement, and the tmux/acp teardown below never fires on a write
    // that did not land. See #1589.
    let profile = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return crate::server::api::session_not_found();
        };
        inst.source_profile.clone()
    };

    let archived = body.archived;
    let persist_id = id.clone();
    if persist_session_update(
        profile,
        "archive update",
        state.file_watch.clone(),
        move |instances| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                if archived {
                    inst.archive();
                } else {
                    inst.unarchive();
                }
            }
        },
    )
    .await
    .is_err()
    {
        return persist_failed_response();
    }

    // Disk is durable; apply to memory and snapshot what the side effects
    // need. Clone the instance once so we can call its `kill()` method
    // outside the lock without re-borrowing.
    let (was_structured_view, inst_clone, kill_pane) = {
        let mut instances = state.instances.write().await;
        let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
            tracing::warn!(
                target: "http.api.sessions",
                session = %id,
                "archive update: instance vanished after persist"
            );
            return crate::server::api::session_gone_after_persist();
        };
        if archived {
            inst.archive();
        } else {
            inst.unarchive();
        }
        let response =
            SessionResponse::from_instance(&*inst, crate::claude_settings::read_tui_fullscreen());

        let structured_view = inst.is_structured();
        let inst_snap = inst.clone();
        drop(instances);

        // Snapshot and drop the lock; run side effects below. Unarchive
        // returns here; archive does NOT short-circuit on kill_pane=false
        // because structured-view shutdown is unconditional.
        if !archived {
            return (StatusCode::OK, Json(serde_json::json!(response))).into_response();
        }
        (structured_view, inst_snap, body.kill_pane)
    };

    // Best-effort tmux teardown (helper logs at debug). #1868.
    if was_structured_view {
        // Worker shutdown before ancillary kill so in-flight tool output
        // settles (mirrors acp.rs:1304-1310). shutdown() preserves the
        // transcript (#1710).
        match state.acp_supervisor.shutdown(&id).await {
            Ok(()) | Err(crate::acp::supervisor::SupervisorError::UnknownSession(_)) => {}
            Err(e) => tracing::warn!(
                target: "acp.supervisor",
                session = %id,
                "shutdown during archive failed: {e}"
            ),
        }
        if kill_pane {
            let inst_for_kill = inst_clone.clone();
            if let Err(e) =
                tokio::task::spawn_blocking(move || inst_for_kill.kill_ancillary_tmux_sessions())
                    .await
            {
                tracing::warn!(
                    target: "http.api.sessions",
                    "Archive: ancillary tmux kill join failed: {e}"
                );
            }
        }
    } else if kill_pane {
        let inst_for_kill = inst_clone.clone();
        if let Err(e) =
            tokio::task::spawn_blocking(move || inst_for_kill.kill_all_tmux_sessions()).await
        {
            tracing::warn!(
                target: "http.api.sessions",
                "Archive: tmux kill join failed: {e}"
            );
        }
    }

    // Re-read the in-memory instance so the response reflects the
    // archived flag (the side effects above did not mutate it, but
    // re-reading also picks up any peer write that landed during the
    // unlock window).
    let instances = state.instances.read().await;
    let response = match instances.iter().find(|i| i.id == id) {
        Some(inst) => {
            SessionResponse::from_instance(inst, crate::claude_settings::read_tui_fullscreen())
        }
        None => {
            return crate::server::api::session_not_found();
        }
    };
    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

/// `POST /api/sessions/:id/trash`. The per-instance lifecycle flock is held
/// from the durable Trash reservation through teardown, relocation, and final commit.
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

    // Worker-stopping barrier: submission guard before `instance_lock`, per
    // `prompt_submission` (#3650).
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return crate::server::api::session_not_found();
    };
    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;
    let (profile, snapshot) = {
        let instances = state.instances.read().await;
        let Some(instance) = instances.iter().find(|instance| instance.id == id) else {
            return crate::server::api::session_not_found();
        };
        (instance.source_profile.clone(), instance.clone())
    };

    let reserve_profile = profile.clone();
    let reserve_id = id.clone();
    let file_watch = state.file_watch.clone();
    let (storage, lifecycle_lock, generation) = match tokio::task::spawn_blocking(
        move || -> anyhow::Result<_> {
            let storage = Storage::new(&reserve_profile, file_watch)?;
            let lifecycle_lock = storage.acquire_instance_lifecycle_lock(&reserve_id)?;
            let generation = storage.update(|instances, _groups| {
                let Some(instance) = instances
                    .iter_mut()
                    .find(|instance| instance.id == reserve_id)
                else {
                    anyhow::bail!("session disappeared before trash");
                };
                instance
                    .try_acquire_lifecycle_reservation(
                        LifecycleOperation::Trash,
                        Instance::LIFECYCLE_RESERVATION_TTL,
                        chrono::Utc::now(),
                    )
                    .map_err(anyhow::Error::new)?;
                instance.trash();
                Ok(instance.lifecycle_generation)
            })?;
            Ok((storage, lifecycle_lock, generation))
        },
    )
    .await
    {
        Ok(Ok(reserved)) => reserved,
        Ok(Err(error)) => {
            tracing::warn!(target: "http.api.sessions", session = %id, "trash reservation failed: {error}");
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "lifecycle_busy",
                    "message": error.to_string()
                })),
            )
                .into_response();
        }
        Err(error) => {
            tracing::error!(target: "http.api.sessions", session = %id, "trash reservation join failed: {error}");
            return persist_failed_response();
        }
    };

    let was_structured_view = snapshot.is_structured();
    {
        let mut instances = state.instances.write().await;
        let Some(instance) = instances.iter_mut().find(|instance| instance.id == id) else {
            return crate::server::api::session_gone_after_persist();
        };
        instance.trash();
        instance.lifecycle_generation = generation;
    }

    if was_structured_view {
        match state.acp_supervisor.shutdown(&id).await {
            Ok(()) | Err(crate::acp::supervisor::SupervisorError::UnknownSession(_)) => {}
            Err(error) => tracing::warn!(
                target: "acp.supervisor",
                session = %id,
                "shutdown during trash failed: {error}"
            ),
        }
    }

    let work_id = id.clone();
    let kill_pane = body.kill_pane;
    let transition = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let _lifecycle_lock = lifecycle_lock;
        let mut instance = snapshot;
        if kill_pane {
            if was_structured_view {
                instance.kill_ancillary_tmux_sessions_locked();
            } else {
                instance.kill_all_tmux_sessions_locked();
            }
        }
        let outcome = crate::session::trash::prepare_trashed_worktree(&mut instance);
        let relocation = match &outcome {
            crate::session::trash::RelocateOutcome::Relocated { .. } => {
                Some(crate::session::trash::TrashRelocation {
                    new_project_path: instance.project_path.clone(),
                    pre_trash_project_path: instance.pre_trash_project_path.clone(),
                })
            }
            crate::session::trash::RelocateOutcome::Skipped
            | crate::session::trash::RelocateOutcome::Failed { .. } => None,
        };
        storage.update(|instances, _groups| {
            if let Some(relocation) = &relocation {
                let commit = crate::session::claim::commit_trash_relocation(
                    instances, &work_id, generation, relocation,
                );
                anyhow::ensure!(
                    commit == crate::session::claim::RelocationCommit::Persisted,
                    "trash relocation reservation was superseded"
                );
            } else if let Some(stored) = instances
                .iter_mut()
                .find(|candidate| candidate.id == work_id)
            {
                stored
                    .release_lifecycle_reservation_if_owned(LifecycleOperation::Trash, generation);
            }
            Ok(())
        })?;
        let durable = storage
            .load()?
            .into_iter()
            .find(|candidate| candidate.id == work_id);
        Ok((outcome, durable))
    })
    .await;

    let (outcome, durable) = match transition {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            tracing::warn!(target: "http.api.sessions", session = %id, "trash transition failed: {error}");
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
        return crate::server::api::session_not_found();
    };
    let response = {
        let mut instances = state.instances.write().await;
        let Some(instance) = instances.iter_mut().find(|instance| instance.id == id) else {
            return crate::server::api::session_gone_after_persist();
        };
        instance.project_path = durable.project_path;
        instance.pre_trash_project_path = durable.pre_trash_project_path;
        instance.lifecycle_generation = durable.lifecycle_generation;
        instance.lifecycle_reservation = durable.lifecycle_reservation;
        SessionResponse::from_instance(instance, crate::claude_settings::read_tui_fullscreen())
    };
    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

/// `POST /api/sessions/:id/restore`. The lifecycle flock covers reservation
/// acquisition, worktree restoration, and durable untrash as one transition.
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
            return crate::server::api::session_not_found();
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
    let restored = tokio::task::spawn_blocking(move || {
        let run = || -> Result<Instance, RestoreTransitionError> {
            let storage = Storage::new(&restore_profile, file_watch)
                .map_err(|error| RestoreTransitionError::Persist(error.to_string()))?;
            let _lifecycle_lock = storage
                .acquire_instance_lifecycle_lock(&restore_id)
                .map_err(|error| RestoreTransitionError::Persist(error.to_string()))?;
            let decision = storage
                .update(|instances, _groups| {
                    crate::session::claim::decide_restore_claim(
                        instances,
                        &restore_id,
                        chrono::Utc::now(),
                    )
                    .map_err(anyhow::Error::new)
                })
                .map_err(|error| RestoreTransitionError::Persist(error.to_string()))?;
            let generation = match decision {
                crate::session::claim::RestoreClaimDecision::Claimed(generation) => generation,
                crate::session::claim::RestoreClaimDecision::AlreadyGone => {
                    return Err(RestoreTransitionError::NotFound);
                }
                crate::session::claim::RestoreClaimDecision::Busy(holder) => {
                    return Err(RestoreTransitionError::Busy(holder.busy_reason()));
                }
            };
            let Some(mut instance) = storage
                .load()
                .map_err(|error| RestoreTransitionError::Persist(error.to_string()))?
                .into_iter()
                .find(|candidate| candidate.id == restore_id)
            else {
                return Err(RestoreTransitionError::NotFound);
            };
            if let crate::session::trash::RestoreOutcome::Failed { reason } =
                crate::session::trash::restore_worktree_location(&mut instance)
            {
                let _ = storage.update(|instances, _groups| {
                    if let Some(stored) = instances
                        .iter_mut()
                        .find(|candidate| candidate.id == restore_id)
                    {
                        stored.release_lifecycle_reservation_if_owned(
                            LifecycleOperation::Restore,
                            generation,
                        );
                    }
                    Ok(())
                });
                return Err(RestoreTransitionError::Worktree(reason));
            }
            let restored_path = instance.project_path.clone();
            let restored_pre = instance.pre_trash_project_path.clone();
            let commit = storage
                .update(|instances, _groups| {
                    Ok(crate::session::claim::finalize_restore_commit(
                        instances,
                        &restore_id,
                        generation,
                        &restored_path,
                        &restored_pre,
                    ))
                })
                .map_err(|error| RestoreTransitionError::Persist(error.to_string()))?;
            match commit {
                crate::session::claim::RestoreCommit::Committed => {
                    instance.untrash();
                    instance.lifecycle_reservation = None;
                    Ok(instance)
                }
                crate::session::claim::RestoreCommit::Superseded => {
                    Err(RestoreTransitionError::Busy(
                        crate::session::NEWER_GENERATION_BUSY_REASON.to_string(),
                    ))
                }
                crate::session::claim::RestoreCommit::AlreadyGone => {
                    Err(RestoreTransitionError::NotFound)
                }
            }
        };
        run()
    })
    .await;

    let restored = match restored {
        Ok(Ok(instance)) => instance,
        Ok(Err(RestoreTransitionError::NotFound)) => {
            return crate::server::api::session_not_found()
        }
        Ok(Err(RestoreTransitionError::Busy(holder))) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "lifecycle_busy",
                    "message": format!("Session is {holder}, so it was not restored")
                })),
            )
                .into_response();
        }
        Ok(Err(RestoreTransitionError::Worktree(reason))) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "worktree_restore_failed",
                    "message": format!("Could not restore the worktree: {reason}")
                })),
            )
                .into_response();
        }
        Ok(Err(RestoreTransitionError::Persist(error))) => {
            tracing::warn!(target: "http.api.sessions", session = %id, "restore transition failed: {error}");
            return persist_failed_response();
        }
        Err(error) => {
            tracing::warn!(target: "http.api.sessions", session = %id, "restore transition join failed: {error}");
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
    // CityHall: only act on structured sessions this mode created; refuse a
    // non-structured (or unknown) target so a locked-down client cannot
    // respawn/destroy/edit an enumerated plain session. See #7.
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

    // Preflight the SAME gate the spawned try_smart_rename re-applies, so the
    // action never reports success (202) for a session the gate would silently
    // drop (a resolved rename agent with no one-shot, an overridden command, or
    // a sandboxed session whose rename agent is not its own). Without this, the
    // sidebar would show success while no title job runs. Resolves with the SAME repo-aware config the worker
    // uses (resolve_config_with_repo_or_warn), so a repo-local smart_rename_agent
    // or agent_command_override cannot make the preflight and worker disagree.
    // Passes `setting_on = true` because this is the manual "Auto-name now"
    // action, which runs on demand even when auto-rename-on-start is disabled
    // (#3039); the spawned try_smart_rename gets `force = true` below to match.
    let resolved = crate::session::config::repo_config::resolve_config_with_repo_or_warn(
        &profile,
        std::path::Path::new(&project_path),
    );
    let config = &resolved.session;
    if let Err(reason) = crate::session::smart_rename::check_eligible_resolved(
        structured,
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
        // TUI's dialog cannot drift; only the status code is per-reason.
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
    // prior failed attempt. The inflight guard inside try_smart_rename still
    // prevents a concurrent one-shot for the same session.
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

    tokio::spawn(
        crate::session::conversation_summary::try_conversation_summary(
            state.clone(),
            id.clone(),
            crate::session::conversation_summary::SummaryTrigger::Manual,
        ),
    );
    StatusCode::ACCEPTED.into_response()
}

/// Stop a session, matching the TUI's `x` keybind: kill the tmux pane and
/// stop (but do not remove) the Docker container for plain sessions; shut down
/// the worker for structured-view sessions. The session record is preserved
/// with status `Stopped` so it can be resumed later. This is NOT delete.
pub async fn stop_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // CityHall: only act on structured sessions this mode created; refuse a
    // non-structured (or unknown) target so a locked-down client cannot
    // respawn/destroy/edit an enumerated plain session. See #7.
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    if state.read_only {
        return crate::server::api::read_only_response();
    }

    // Worker-stopping barrier: submission guard before `instance_lock`, per
    // `prompt_submission` (#3650).
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return crate::server::api::session_not_found();
    };
    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    // Snapshot profile, session type, and current status without mutating yet
    // so a persist failure leaves disk and memory in agreement (mirrors the
    // archive handler).
    let (profile, is_structured, already_stopped) = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return crate::server::api::session_not_found();
        };

        let structured = inst.is_structured();
        // Mirror the TUI's `stop_selected` guard: a session that is already
        // stopped or mid-lifecycle has nothing to stop.
        let already = matches!(
            inst.status,
            Status::Stopped | Status::Deleting | Status::Creating
        );
        (inst.source_profile.clone(), structured, already)
    };

    if already_stopped {
        let instances = state.instances.read().await;
        let response = match instances.iter().find(|i| i.id == id) {
            Some(inst) => {
                SessionResponse::from_instance(inst, crate::claude_settings::read_tui_fullscreen())
            }
            None => {
                return crate::server::api::session_not_found();
            }
        };
        return (StatusCode::OK, Json(serde_json::json!(response))).into_response();
    }

    // Structured sessions have no tmux/container teardown transaction, so
    // persist their dormant stop before asking the supervisor to shut down.
    // Plain sessions delegate the full reserve/teardown/commit sequence to
    // `Instance::stop` below.
    if is_structured {
        let persist_id = id.clone();
        if persist_session_update(
            profile.clone(),
            "stop session",
            state.file_watch.clone(),
            move |instances| {
                if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
                    inst.status = Status::Stopped;
                    inst.mark_idle_dormant();
                }
            },
        )
        .await
        .is_err()
        {
            return persist_failed_response();
        }
    }

    let inst_clone = {
        let mut instances = state.instances.write().await;
        let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
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
        }
        inst.clone()
    };

    if is_structured {
        // Structured view: shut down the worker so the reconciler does not
        // race to respawn it. `shutdown` preserves the transcript, so the
        // session resumes the conversation when reopened.
        match state.acp_supervisor.shutdown(&id).await {
            Ok(()) | Err(crate::acp::supervisor::SupervisorError::UnknownSession(_)) => {}
            Err(e) => tracing::warn!(
                target: "acp.supervisor",
                session = %id,
                "shutdown during stop failed: {e}"
            ),
        }
    } else {
        // Plain session: kill the tmux pane and stop (not remove) the Docker
        // container. `Instance::stop` can block ~10s on `docker stop`, so run
        // it off the async runtime. Mirrors the TUI's StopPoller.
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
            return crate::server::api::session_not_found();
        }
    };
    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

/// Start (resume) a stopped session, the inverse of [`stop_session`]. Plain
/// sessions are restarted exactly like `ensure_session` (kill any corpse pane,
/// then `start_with_resume_fallback`); structured sessions are un-parked by
/// clearing the idle-dormant mark so the acp reconciler respawns the worker on
/// its next tick (mirrors unarchive). No-op for a session that isn't stopped.
pub async fn start_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // CityHall: only act on structured sessions this mode created; refuse a
    // non-structured (or unknown) target so a locked-down client cannot
    // respawn/destroy/edit an enumerated plain session. See #7.
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
            return crate::server::api::session_not_found();
        };

        let structured = inst.is_structured();
        (
            inst.source_profile.clone(),
            structured,
            matches!(inst.status, Status::Stopped),
            inst.clone(),
        )
    };

    // Only a stopped session has anything to start; otherwise return current.
    if !is_stopped {
        let instances = state.instances.read().await;
        let response = match instances.iter().find(|i| i.id == id) {
            Some(inst) => {
                SessionResponse::from_instance(inst, crate::claude_settings::read_tui_fullscreen())
            }
            None => {
                return crate::server::api::session_not_found();
            }
        };
        return (StatusCode::OK, Json(serde_json::json!(response))).into_response();
    }

    if is_structured {
        // Un-park: clear the dormant mark and drop the Stopped status so the
        // reconciler's next tick treats it as a resume target and respawns the
        // worker (the transcript was preserved by stop's shutdown).
        let persist_id = id.clone();
        if persist_session_update(
            profile,
            "start session",
            state.file_watch.clone(),
            move |instances| {
                if let Some(inst) = instances.iter_mut().find(|i| i.id == persist_id) {
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
                return crate::server::api::session_not_found();
            }
        };
        return (StatusCode::OK, Json(serde_json::json!(response))).into_response();
    }

    // Plain session: restart the tmux pane, mirroring ensure_session. Show
    // Starting immediately so the status poller doesn't flip it back while the
    // restart (which can block) is in flight.
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
            // Explicit restart endpoint (web dashboard Restart button):
            // honor auto_resume_on_restart, same as TUI `e`/`Enter`. The
            // instance-level cascade holds the lifecycle lock across final
            // poller drain, exact-pane OMP capture, kill, and relaunch.
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
                    return crate::server::api::session_not_found();
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
            (StatusCode::OK, Json(serde_json::json!(response))).into_response()
        }
        Ok(Err(boxed)) => {
            let (started, e) = *boxed;
            let msg = e.to_string();
            tracing::warn!(target: "http.api.sessions", "start_session restart failed for {id}: {msg}");
            let mut instances = state.instances.write().await;
            if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                if apply_post_restart_sync(inst, &sync_base, &started) {
                    inst.status = Status::Error;
                    inst.last_error = Some(msg.clone());
                }
            }
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "restart_failed", "message": msg})),
            )
                .into_response()
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
    // CityHall: only act on structured sessions this mode created; refuse a
    // non-structured (or unknown) target so a locked-down client cannot
    // respawn/destroy/edit an enumerated plain session. See #7.
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

    // Validate the duration up front. The TUI dialog presets, CLI, and
    // this endpoint all share the same bounds (1..=43200 minutes); see
    // `crate::session::config::validate_snooze_duration`.
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

    // Worker-stopping barrier: submission guard before `instance_lock`, per
    // `prompt_submission` (#3650).
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return crate::server::api::session_not_found();
    };
    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    let (was_structured_view, profile) = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return crate::server::api::session_not_found();
        };

        let structured_view = inst.is_structured();
        (structured_view, inst.source_profile.clone())
    };

    let minutes = body.minutes;

    // Persist first; only mutate memory once disk is durable, and only fire
    // the structured view teardown below on a write that landed. See #1589.
    let persist_id = id.clone();
    if persist_session_update(
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
    .is_err()
    {
        return persist_failed_response();
    }

    {
        let mut instances = state.instances.write().await;
        let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
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
    }

    // For structured view-mode sessions, snoozing tears down the worker the
    // same way archive does. Snooze is a "temporary archive" in the
    // data model and the structured view worker (claude-agent-acp subprocess)
    // is heavy enough that keeping it idle while the row is sunk is a
    // resource hog. The reconciler skips snoozed sessions, so the
    // worker stays down until the snooze expires; the next reconciler
    // tick after expiry brings it back. Unsnooze just lets the
    // reconciler re-pick the session naturally, no explicit respawn.
    // `shutdown` preserves the agent transcript (no session/delete), so
    // that respawn resumes the conversation instead of resetting it
    // (#1710).
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

    let instances = state.instances.read().await;
    let response = match instances.iter().find(|i| i.id == id) {
        Some(inst) => {
            SessionResponse::from_instance(inst, crate::claude_settings::read_tui_fullscreen())
        }
        None => {
            return crate::server::api::session_not_found();
        }
    };
    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

/// `PATCH /api/sessions/{id}/unread` — flag a session unread (`{"unread":true}`)
/// or mark it read (`{"unread":false}`). Mirrors the TUI's `u` toggle, but the
/// client computes the target from the current state rather than toggling
/// server-side, so an optimistic UI update can't desync. No-op when the
/// `session.unread_indicator` feature is off (the client hides the control
/// then, but guard here too). Persist-then-mutate, like snooze.
pub async fn update_session_unread(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdateUnreadBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    // CityHall: only act on structured sessions this mode created; refuse a
    // non-structured (or unknown) target so a locked-down client cannot
    // respawn/destroy/edit an enumerated plain session. See #7.
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
            return crate::server::api::session_not_found();
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
            return crate::server::api::session_not_found();
        }
    };
    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}
