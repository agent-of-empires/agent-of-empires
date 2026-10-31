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
    tracing::info!(target: "session.sync", session = %id, field = "pinned", value = pinned, "session updated");
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
    let favorited = body.favorited;
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
    tracing::info!(target: "session.sync", session = %id, field = "favorite", value = favorited, "session updated");
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

    let color = new_color.clone();
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
    tracing::info!(target: "session.sync", session = %id, field = "color", value = ?color, "session updated");
    drop(_guard);
    drop(namespace);
    crate::server::runtime::session_mutation_response(&state, &id, None::<()>).await
}

#[derive(Debug, thiserror::Error)]
#[error("archive outcome is unconfirmed")]
struct ArchiveOutcomeUnconfirmed;

struct ArchiveReservationRelease {
    native: Option<crate::server::session_store::NativeSessionStore>,
    id: String,
    generation: Option<u64>,
}

impl Drop for ArchiveReservationRelease {
    fn drop(&mut self) {
        if let (Some(native), Some(generation)) = (self.native.take(), self.generation.take()) {
            crate::session::SessionStore::defer_lifecycle_release(
                native,
                std::mem::take(&mut self.id),
                LifecycleOperation::Stop,
                generation,
                None,
            );
        }
    }
}

#[cfg(test)]
mod archive_reservation_tests {
    use super::*;

    #[tokio::test]
    #[serial_test::serial]
    async fn abandoned_archive_releases_only_its_bound_reservation_during_degraded_health(
    ) -> anyhow::Result<()> {
        for case in ["owned", "new-generation", "replaced-profile"] {
            let _home = crate::session::test_support::isolate_app_dir();
            let mut row = Instance::new("original archive", "/tmp");
            row.source_profile = "default".into();
            row.archive();
            let generation = row.try_acquire_lifecycle_reservation(
                LifecycleOperation::Stop,
                Instance::LIFECYCLE_RESERVATION_TTL,
                chrono::Utc::now(),
            )?;
            crate::server::test_support::seed_instances_on_disk_for_test(
                "default",
                vec![row.clone()],
            );
            let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
            crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
            let opening_state = state.clone();
            let native = tokio::task::spawn_blocking(move || {
                crate::server::session_store::NativeSessionStore::open(
                    opening_state,
                    "default",
                    None,
                )
            })
            .await??;
            *state.canonical_health.write().await = crate::daemon::RuntimeHealth::Degraded {
                code: crate::daemon::ReloadFailureCode::Metadata,
                profiles: vec!["default".into()],
            };
            let namespace = state.profile_namespace.clone().write_owned().await;
            let storage = Storage::open_unwatched("default")?;
            let original_bytes = std::fs::read(storage.sessions_path())?;
            drop(ArchiveReservationRelease {
                native: Some(native),
                id: row.id.clone(),
                generation: Some(generation),
            });
            let mut expected_generation = generation;
            let mut retired_file = None;
            if case == "new-generation" {
                storage.update(|rows, _| {
                    rows[0].release_lifecycle_reservation_if_owned(
                        LifecycleOperation::Stop,
                        generation,
                    );
                    expected_generation = rows[0].try_acquire_lifecycle_reservation(
                        LifecycleOperation::Stop,
                        Instance::LIFECYCLE_RESERVATION_TTL,
                        chrono::Utc::now(),
                    )?;
                    Ok(())
                })?;
                assert_ne!(expected_generation, generation);
            } else if case == "replaced-profile" {
                let retired = crate::session::get_app_dir()?.join("retired-original-profile");
                let directory = storage.sessions_path().parent().unwrap();
                std::fs::rename(directory, &retired)?;
                retired_file = Some(retired.join(storage.sessions_path().file_name().unwrap()));
                let replacement = Storage::new_unwatched("default")?;
                row.title = "replacement must retain its matching numeric token".into();
                row.created_at += chrono::Duration::seconds(1);
                replacement.update(|rows, _| {
                    *rows = vec![row.clone()];
                    Ok(())
                })?;
            }
            let selected_before_release = std::fs::read(storage.sessions_path())?;
            drop(namespace);
            tokio::time::timeout(
                std::time::Duration::from_secs(10),
                state.runtime.work.drain(),
            )
            .await?;
            let rows = Storage::open_unwatched("default")?.load()?;
            assert!(rows[0].is_archived());
            if case == "owned" {
                assert!(rows[0].lifecycle_reservation.is_none());
            } else {
                assert!(rows[0]
                    .lifecycle_reservation_is_owned(LifecycleOperation::Stop, expected_generation));
                assert_eq!(
                    std::fs::read(storage.sessions_path())?,
                    selected_before_release
                );
            }
            if let Some(retired_file) = retired_file {
                assert_eq!(std::fs::read(retired_file)?, original_bytes);
            }
        }
        Ok(())
    }
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
    let (archived, kill_pane) = (body.archived, body.kill_pane);
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let native = crate::server::session_store::NativeSessionStore::open(
            worker_state.clone(), &profile, Some(worker_id.clone()),
        )?;
        let store: &dyn crate::session::SessionStore = &native;
        let _title = crate::session::acquire_session_title_lock(&worker_id)?;
        let _lifecycle = store.storage().acquire_instance_lifecycle_lock(&worker_id)?;
        let mut candidate = None;
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
            candidate = Some((generation, row.created_at));
            Ok(Some((generation, row.clone())))
        });
        let reserved = match reserved {
            Ok(reserved) => reserved,
            Err(error) => {
                if let Some((generation, created_at)) = candidate {
                    let owned = if error.is::<crate::session::SessionCommitApplied>() {
                        true
                    } else {
                        let proof = (|| -> anyhow::Result<bool> {
                            store.storage().verify_bound_profile()?;
                            let (rows, _) = store.storage().load_complete_with_groups()?;
                            Ok(rows.iter().any(|row| row.id == worker_id
                                && row.created_at == created_at
                                && row.is_archived()
                                && row.lifecycle_reservation_is_owned(LifecycleOperation::Stop, generation)))
                        })();
                        match proof {
                            Ok(owned) => owned,
                            Err(proof_error) => return Err(error
                                .context(format!("Archive reservation ownership is unavailable: {proof_error:#}"))
                                .context(ArchiveOutcomeUnconfirmed)),
                        }
                    };
                    if owned {
                        let _release = ArchiveReservationRelease {
                            native: Some(native), id: worker_id.clone(), generation: Some(generation),
                        };
                        return Err(error.context(ArchiveOutcomeUnconfirmed));
                    }
                    return Err(error.context(ArchiveOutcomeUnconfirmed));
                }
                return Err(error);
            }
        };
        let Some((generation, mut instance)) = reserved else {
            return Ok(());
        };
        let mut release = ArchiveReservationRelease {
            native: Some(native), id: worker_id.clone(), generation: Some(generation),
        };
        let native = release.native.as_ref().expect("owned archive store");
        let store: &dyn crate::session::SessionStore = native;
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
        let finalized = store.update(|rows, _| {
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
        });
        if let Err(error) = finalized {
            if error.is::<crate::session::SessionCommitApplied>() {
                release.generation = None;
            }
            return Err(error.context(ArchiveOutcomeUnconfirmed));
        }
        release.generation = None;
        if instance.is_structured() || body.kill_pane {
            native.refresh_pane_observations(&instance)
                .map_err(|error| error.context(ArchiveOutcomeUnconfirmed))?;
        }
        stopped.map_err(|error| error.context(ArchiveOutcomeUnconfirmed))
    }).await.map_err(anyhow::Error::from).and_then(|result| result);
    if let Err(error) = result {
        if error.is::<ArchiveOutcomeUnconfirmed>()
            || error.is::<crate::session::SessionCommitApplied>()
        {
            return crate::server::api::api_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "outcome_unconfirmed",
                "Archive may have committed, but completion is unconfirmed. Check canonical state before retrying.",
            );
        }
        if let Some(response) = lifecycle_rejection(&state, &error) {
            return response;
        }
        if error.is::<crate::session::NativeStoreUnavailable>() {
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        tracing::warn!(target: "http.api.sessions", session = %id, %error, "archive commit failed");
        return persist_failed_response();
    }
    if archived {
        tracing::info!(target: "session.lifecycle", session = %id, kill_pane, "session archived");
    } else {
        tracing::info!(target: "session.lifecycle", session = %id, kill_pane, "session unarchived");
    }
    drop(guard);
    drop(submission);
    drop(namespace);
    crate::server::runtime::session_mutation_response(&state, &id, None::<()>).await
}

/// Commit the engagement timestamp and unsink an archived or snoozed row together.
pub async fn touch_session_access(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
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
    if let Some(response) = cityhall_block_non_structured(&state, &id).await {
        return response;
    }
    let profile = {
        let instances = state.instances.read().await;
        let Some(row) = instances.iter().find(|row| row.id == id) else {
            return crate::server::api::session_not_found();
        };
        if row.is_trashed() {
            // A trashed row is not mid-lifecycle: it is shelved, and the rest
            // of the surface answers that with its own code and a message the
            // caller can act on rather than a generic busy.
            return crate::server::api::start_blocked_response(
                crate::session::StartBlocked::Trashed,
            );
        }
        if matches!(row.status, Status::Creating | Status::Deleting) {
            return lifecycle_rejection(&state, &LifecycleTargetError::Busy.into())
                .unwrap_or_else(|| StatusCode::CONFLICT.into_response());
        }
        row.source_profile.clone()
    };
    let persist_id = id.clone();
    if let Err(response) = commit_profile_update(
        &state,
        profile,
        "session access",
        move |rows| {
            if let Some(row) = rows.iter_mut().find(|row| row.id == persist_id) {
                row.touch_last_accessed();
            }
        },
        None,
    )
    .await
    {
        return response;
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
        Ok((outcome, stopped.err().map(|error| error.to_string())))
    }).await.map_err(anyhow::Error::from).and_then(|result| result);
    if result.is_ok() {
        tracing::info!(target: "session.trash", session = %id, "session trashed");
    }
    let relocation = match result {
        Ok((_, Some(reason))) => {
            tracing::warn!(target: "http.api.sessions", session = %id, %reason, "trash tmux teardown failed");
            crate::daemon::TrashRelocationOutcome::TeardownFailed { reason }
        }
        Ok((crate::session::trash::RelocateOutcome::Failed { reason }, None)) => {
            tracing::warn!(target: "http.api.sessions", session = %id, %reason, "trash worktree relocation skipped");
            crate::daemon::TrashRelocationOutcome::Failed { reason }
        }
        Ok((crate::session::trash::RelocateOutcome::Skipped, None)) => {
            crate::daemon::TrashRelocationOutcome::Skipped
        }
        Ok((crate::session::trash::RelocateOutcome::Relocated { warning, .. }, None)) => {
            if let Some(warning) = &warning {
                tracing::warn!(target: "http.api.sessions", session = %id, %warning, "trashed worktree is degraded");
            }
            crate::daemon::TrashRelocationOutcome::Relocated { warning }
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
    let result =
        tokio::task::spawn_blocking(move || -> anyhow::Result<crate::daemon::RestoreOutcome> {
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
                        return Err(row.lifecycle_reservation.as_ref().map_or(
                            LifecycleTargetError::Busy.into(),
                            |reservation| {
                                crate::session::LifecycleReservationError::Busy(reservation.op)
                                    .into()
                            },
                        ));
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
                    crate::session::claim::RestoreClaimDecision::Busy(holder) => {
                        Err(crate::session::LifecycleReservationError::Busy(holder).into())
                    }
                }
            })?;
            let Some((generation, mut instance)) = claimed else {
                return Ok(crate::daemon::RestoreOutcome::AlreadyRestored);
            };
            instance.source_profile = profile;
            instance.file_watch = Some(worker_state.file_watch.clone());
            let warning = match crate::session::trash::restore_worktree_location(&mut instance) {
                crate::session::trash::RestoreOutcome::Failed { reason } => {
                store.update(|rows, _| {
                    let row = rows
                        .iter_mut()
                        .find(|row| row.id == worker_id)
                        .ok_or(LifecycleTargetError::Missing)?;
                    if !row.release_lifecycle_reservation_if_owned(
                        LifecycleOperation::Restore,
                        generation,
                    ) {
                        return Err(LifecycleTargetError::Busy.into());
                    }
                    Ok(())
                })?;
                return Err(WorktreeFailure(reason).into());
                }
                crate::session::trash::RestoreOutcome::Restored { warning, .. } => warning,
                crate::session::trash::RestoreOutcome::NoChange => None,
            };
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
                        Err(crate::session::LifecycleReservationError::Superseded.into())
                    }
                }
            })?;
            if let Some(warning) = &warning {
                tracing::warn!(target: "http.api.sessions", session = %worker_id, %warning, "restored worktree is degraded");
            }
            Ok(crate::daemon::RestoreOutcome::Restored { warning })
        })
        .await
        .map_err(anyhow::Error::from)
        .and_then(|result| result);
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(error) => {
            if let Some(response) = lifecycle_rejection(&state, &error) {
                return response;
            }
            if let Some(WorktreeFailure(reason)) = error.downcast_ref::<WorktreeFailure>() {
                tracing::warn!(target: "session.restore", session = %id, %reason, "restore rejected: worktree conflict");
                return api_error(
                    StatusCode::CONFLICT,
                    "worktree_restore_failed",
                    format!("Could not restore the worktree: {reason}"),
                );
            }
            if error.is::<crate::session::NativeStoreUnavailable>() {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            tracing::warn!(target: "http.api.sessions", session = %id, %error, "restore transition failed");
            return persist_failed_response();
        }
    };
    tracing::info!(target: "session.restore", session = %id, ?outcome, "session restored");
    drop(guard);
    drop(submission);
    drop(namespace);
    crate::server::runtime::session_mutation_response(&state, &id, Some(outcome)).await
}

/// `POST /api/sessions/:id/smart-rename`. Manual "Auto-name now" for a
/// structured session: clears the per-session attempted gate and regenerates the
/// title from the first prompt, even over one already chosen. Waits for the
/// rename: `200` once the title is saved, `409` when nothing could be applied,
/// `502` with the agent's reason on failure, `504` past the rename deadline.
pub async fn force_smart_rename(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    // CityHall rejects terminal targets before admitting any work.
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
        return crate::server::api::session_not_found();
    };

    // Manual requests bypass the setting and current-title gates; the worker revalidates all others.
    let resolved = crate::session::config::repo_config::resolve_config_with_repo_or_warn(
        &profile,
        std::path::Path::new(&project_path),
    );
    let config = &resolved.session;
    if let Err(reason) = crate::session::smart_rename::check_eligible_resolved(
        true,
        true,
        true,
        &title,
        &tool,
        &config.smart_rename_agent,
        sandboxed,
        &command,
        &config.agent_command_override,
    ) {
        return api_error(StatusCode::CONFLICT, reason.as_str(), reason.user_message());
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

    tracing::info!(target: "smart_rename", session = %id, structured, "manual rename requested");
    if !structured {
        state.runtime.work.spawn(
            "server.terminal_smart_rename",
            crate::session::smart_rename::try_terminal_smart_rename(
                state.clone(),
                profile,
                id.clone(),
                true,
            ),
        );
        return match state.runtime.publish(&state).await {
            Ok(snapshot) => crate::server::runtime::mutation_response(
                &snapshot.value.cursor,
                StatusCode::ACCEPTED,
            ),
            Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        };
    }

    let Some((first_user_prompt, agent_prose)) = state
        .acp_event_store
        .first_turn_context(&id, crate::session::smart_rename::FIRST_TURN_AGENT_BYTES)
    else {
        return api_error(
            StatusCode::CONFLICT,
            "no_prompt",
            "No prompt to name this session from yet",
        );
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

    use crate::session::smart_rename::SmartRenameError;
    let Err(err) = crate::session::smart_rename::try_smart_rename(
        state.clone(),
        id.clone(),
        crate::session::smart_rename::SmartRenameInput {
            first_user_prompt,
            context,
        },
        // Manual action forces past the smart_rename-disabled gate (#3039).
        true,
    )
    .await
    else {
        return crate::server::runtime::session_mutation_response(&state, &id, None::<()>).await;
    };
    use crate::daemon::ApiErrorCode;
    let (code, message) = match err {
        SmartRenameError::Skipped(_) => (
            ApiErrorCode::SmartRenameSkipped,
            "No session title could be applied",
        ),
        SmartRenameError::Failed(_) => (ApiErrorCode::SmartRenameFailed, "The naming agent failed"),
        SmartRenameError::TimedOut => (
            ApiErrorCode::SmartRenameTimeout,
            "The naming agent timed out",
        ),
    };
    crate::server::api::api_error_with_code_header(code, message)
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

    tracing::info!(target: "session.lifecycle", session = %id, "summary requested");
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
    #[error("session is not in structured view")]
    NotStructured,
    /// An archived or trashed session refuses to launch or resume (#4116).
    #[error(transparent)]
    Blocked(#[from] crate::session::StartBlocked),
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
        Some(LifecycleTargetError::NotStructured) => {
            return Some(api_error(
                StatusCode::CONFLICT,
                "not_structured",
                "Switch the session to structured view before stopping an ACP worker",
            ))
        }
        _ => {}
    }
    if let Some(blocked) = error
        .downcast_ref::<LifecycleTargetError>()
        .and_then(|error| match error {
            LifecycleTargetError::Blocked(blocked) => Some(*blocked),
            _ => None,
        })
        .or_else(|| {
            error
                .downcast_ref::<crate::session::StartBlocked>()
                .copied()
        })
    {
        return Some(crate::server::api::start_blocked_response(blocked));
    }
    // A row a peer purged between the memory check and this write: the caller
    // is talking to a session that no longer exists, not to a busy one.
    if error.is::<crate::session::SessionGone>() {
        return Some(crate::server::api::session_not_found());
    }
    let (code, operation, message) = match error
        .downcast_ref::<crate::session::LifecycleReservationError>()
    {
        Some(crate::session::LifecycleReservationError::Busy(operation)) => (
            "lifecycle_busy",
            Some(*operation),
            format!(
                "Session is busy with {}; nothing changed, retry when it finishes",
                serde_json::json!(operation)
                    .as_str()
                    .unwrap_or("another operation")
            ),
        ),
        Some(crate::session::LifecycleReservationError::Superseded) => (
            "lifecycle_superseded",
            None,
            "Session changed after it was selected; refresh and check before retrying".to_owned(),
        ),
        _ if matches!(
            error.downcast_ref::<LifecycleTargetError>(),
            Some(LifecycleTargetError::Busy)
        ) =>
        {
            (
                "lifecycle_busy",
                None,
                "Session lifecycle is busy".to_owned(),
            )
        }
        _ => return None,
    };
    let header = if code == "lifecycle_superseded" {
        crate::daemon::ApiErrorCode::LifecycleSuperseded.header()
    } else {
        crate::daemon::ApiErrorCode::LifecycleLocked.header()
    };
    Some(
        (
            StatusCode::CONFLICT,
            header,
            Json(serde_json::json!({"error": code, "operation": operation, "message": message})),
        )
            .into_response(),
    )
}

async fn adopt_lifecycle_commit<R>(
    state: &Arc<AppState>,
    profile: String,
    id: &str,
    result: anyhow::Result<(R, Vec<Instance>, Vec<crate::session::Group>)>,
    reload: &tokio::sync::OwnedRwLockWriteGuard<()>,
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
    super::update::adopt_profile_update(
        state,
        profile,
        result,
        |row_id| row_id == id,
        reload,
        publication,
    )
    .await
}

fn structured_lifecycle_error(state: &AppState, error: anyhow::Error) -> axum::response::Response {
    if error.is::<crate::session::SessionCommitApplied>() {
        return api_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "outcome_unconfirmed",
            "The change committed, but runtime publication is unconfirmed. Check canonical state before retrying.",
        );
    }
    if let Some(response) = lifecycle_rejection(state, &error) {
        return response;
    }
    tracing::warn!(target: "http.api.sessions", %error, "structured lifecycle failed");
    StatusCode::SERVICE_UNAVAILABLE.into_response()
}

fn reserve_structured_user_stop(row: &mut Instance) -> anyhow::Result<u64> {
    if !row.is_structured() {
        return Err(LifecycleTargetError::NotStructured.into());
    }
    if row.status == Status::Deleting || row.terminal_handoff.is_some() {
        return Err(LifecycleTargetError::Busy.into());
    }
    // Stop supersedes an unpublished launch and an older stop, not unrelated work.
    if let Some((operation, generation)) = row
        .lifecycle_reservation
        .as_ref()
        .map(|reservation| (reservation.op, reservation.generation))
    {
        if matches!(
            operation,
            LifecycleOperation::Launch | LifecycleOperation::Stop
        ) {
            row.release_lifecycle_reservation_if_owned(operation, generation);
        }
    }
    let generation = row.try_acquire_lifecycle_reservation(
        LifecycleOperation::Stop,
        Instance::LIFECYCLE_RESERVATION_TTL,
        chrono::Utc::now(),
    )?;
    // Even a row already presented as Stopped can own a live or pending worker.
    row.status = Status::Stopped;
    row.mark_idle_dormant();
    Ok(generation)
}

struct StructuredStopCommit {
    native: crate::server::session_store::NativeSessionStore,
    generation: u64,
    target: crate::acp::supervisor::RetirementTarget,
    publication: anyhow::Result<()>,
}

fn commit_structured_user_stop(
    state: Arc<AppState>,
    profile: &str,
    id: &str,
) -> anyhow::Result<StructuredStopCommit> {
    use crate::session::SessionStore;
    let native = crate::server::session_store::NativeSessionStore::open(
        state.clone(),
        profile,
        Some(id.to_owned()),
    )?;
    let _title = crate::session::acquire_session_title_lock(id)?;
    let _lifecycle = native.storage().acquire_instance_lifecycle_lock(id)?;
    // Capture after the birth flock: a child born before this fence is ours to settle.
    let target = state.acp_supervisor.capture_retirement(id)?;
    let mut generation = None;
    let publication = match (&native as &dyn SessionStore).update(|rows, _| {
        let row = rows
            .iter_mut()
            .find(|row| row.id == id)
            .ok_or(crate::session::SessionGone)?;
        generation = Some(reserve_structured_user_stop(row)?);
        Ok(())
    }) {
        Ok(()) => Ok(()),
        Err(error) if error.is::<crate::session::SessionCommitApplied>() => Err(error),
        Err(error) => return Err(error),
    };
    let generation = generation.ok_or_else(|| anyhow::anyhow!("stop commit has no generation"))?;
    // Both flocks drop here, before a late constructor's admission or cleanup.
    Ok(StructuredStopCommit {
        native,
        generation,
        target,
        publication,
    })
}

async fn finish_structured_user_stop(
    state: &Arc<AppState>,
    id: &str,
    committed: StructuredStopCommit,
) -> Result<(), axum::response::Response> {
    use crate::session::SessionStore;
    let StructuredStopCommit {
        native,
        generation,
        target,
        publication,
    } = committed;
    let retirement = state
        .acp_supervisor
        .shutdown_captured_and_wait(id, &target, std::time::Duration::from_secs(5))
        .await;
    let released = if retirement.is_ok() {
        let release_id = id.to_owned();
        tokio::task::spawn_blocking(move || {
            let lifecycle = native
                .storage()
                .acquire_instance_lifecycle_lock(&release_id)?;
            native.storage().verify_bound_profile()?;
            // A newer stop or explicit resume owns its own hook status files.
            let rows = native.storage().load()?;
            if let Some(row) = rows
                .iter()
                .find(|row| row.id == release_id && row.lifecycle_generation == generation)
            {
                crate::hooks::cleanup_hook_status_dir(&release_id);
                if let Err(error) = crate::session::worktree_edit::stop_pinned_sandbox_container(row) {
                    tracing::warn!(target: "containers.runtime", session = %release_id, %error, "sandbox container stop after structured stop failed");
                }
            }
            native.release_lifecycle_reservation(
                &release_id,
                LifecycleOperation::Stop,
                generation,
                Some(lifecycle),
            )
        })
        .await
        .map_err(anyhow::Error::from)
        .and_then(|result| result)
    } else {
        Ok(())
    };
    // A committed stop must still settle its captured runner when adoption fails.
    publication.map_err(|error| structured_lifecycle_error(state, error))?;
    if let Err(error) = retirement {
        tracing::warn!(target: "http.api.sessions", session = %id, %error, "structured stop retirement unproven");
        return Err(api_error(
            StatusCode::CONFLICT,
            "teardown_pending",
            "The original structured worker is still stopping",
        ));
    }
    released.map_err(|error| structured_lifecycle_error(state, error))?;
    {
        let _publication = state.publication.write().await;
        if let Some(row) = state
            .instances
            .write()
            .await
            .iter_mut()
            .find(|row| row.id == id && row.lifecycle_generation == generation)
        {
            row.plugin_revival_pending = false;
        }
    }
    Ok(())
}

/// The caller retains namespace, submission and instance exclusion.
pub(crate) async fn stop_structured_with_commit(
    state: &Arc<AppState>,
    profile: String,
    id: &str,
) -> Result<(), axum::response::Response> {
    let worker_state = state.clone();
    let stop_id = id.to_owned();
    let committed = tokio::task::spawn_blocking(move || {
        commit_structured_user_stop(worker_state, &profile, &stop_id)
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result)
    .map_err(|error| structured_lifecycle_error(state, error))?;
    finish_structured_user_stop(state, id, committed).await
}

/// Explicit resume consumes only the stop observed before original retirement.
pub(crate) async fn resume_structured_stop(
    state: &Arc<AppState>,
    expected: Instance,
) -> Result<Instance, axum::response::Response> {
    use crate::process::worker_registry::{self, ResumeAuthority};
    use crate::session::SessionStore;
    if expected.status != Status::Stopped && !expected.is_idle_dormant() {
        return Ok(expected);
    }
    let probe_state = state.clone();
    let probe_profile = expected.source_profile.clone();
    let probe_id = expected.id.clone();
    let expected_generation = expected.lifecycle_generation;
    let (native, observed_stop) = tokio::task::spawn_blocking(move || {
        let native = crate::server::session_store::NativeSessionStore::open(
            probe_state,
            &probe_profile,
            Some(probe_id.clone()),
        )?;
        let _lifecycle = native
            .storage()
            .acquire_instance_lifecycle_lock(&probe_id)?;
        let row = native
            .load()?
            .into_iter()
            .find(|row| row.id == probe_id)
            .ok_or(crate::session::SessionGone)?;
        row.ensure_startable()?;
        anyhow::ensure!(row.is_structured(), LifecycleTargetError::NotStructured);
        if row.terminal_handoff.is_some()
            || matches!(row.status, Status::Creating | Status::Deleting)
            || (row.has_fresh_lifecycle_reservation(chrono::Utc::now())
                && !row
                    .lifecycle_reservation_is_owned(LifecycleOperation::Stop, expected_generation))
        {
            return Err(LifecycleTargetError::Busy.into());
        }
        anyhow::ensure!(
            row.lifecycle_generation == expected_generation,
            crate::session::LifecycleReservationError::Superseded
        );
        let observed_stop = worker_registry::stop_intent(&probe_id)?;
        Ok::<_, anyhow::Error>((native, observed_stop))
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result)
    .map_err(|error| structured_lifecycle_error(state, error))?;
    let target = state
        .acp_supervisor
        .capture_retirement(&expected.id)
        .map_err(|error| structured_lifecycle_error(state, error.into()))?;
    state.acp_supervisor.shutdown_captured_and_wait(
        &expected.id,
        &target,
        std::time::Duration::from_secs(5),
    ).await.map_err(|error| {
        tracing::warn!(target: "http.api.sessions", session = %expected.id, %error, "resume requires original retirement");
        api_error(StatusCode::CONFLICT, "teardown_pending", "The original structured worker is still stopping")
    })?;
    tokio::task::spawn_blocking(move || {
        let _title = crate::session::acquire_session_title_lock(&expected.id)?;
        let _lifecycle = native
            .storage()
            .acquire_instance_lifecycle_lock(&expected.id)?;
        (&native as &dyn SessionStore).update(|rows, _| {
            let row = rows
                .iter_mut()
                .find(|row| row.id == expected.id)
                .ok_or(crate::session::SessionGone)?;
            row.ensure_startable()?;
            anyhow::ensure!(row.is_structured(), LifecycleTargetError::NotStructured);
            anyhow::ensure!(
                row.lifecycle_generation == expected.lifecycle_generation,
                crate::session::LifecycleReservationError::Superseded
            );
            // Retirement above, plus exact row/token comparison, excludes newer stops.
            worker_registry::admit_resume(&expected.id, ResumeAuthority::Explicit, &observed_stop)?;
            row.resume_structured_after_retirement(expected.lifecycle_generation)?;
            Ok(row.clone())
        })
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result)
    .map_err(|error| structured_lifecycle_error(state, error))
}

// The caller retains namespace, submission, and instance exclusion.
async fn stop_with_commit(
    state: &Arc<AppState>,
    profile: String,
    id: &str,
) -> Result<(), axum::response::Response> {
    if state
        .instances
        .read()
        .await
        .iter()
        .any(|row| row.id == id && row.is_structured())
    {
        return stop_structured_with_commit(state, profile, id).await;
    }
    let stop_profile = profile.clone();
    let stop_id = id.to_owned();
    let file_watch = state.file_watch.clone();
    let opened = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let storage = Storage::open(&stop_profile, file_watch)?;
        let title = crate::session::acquire_session_title_lock(&stop_id)?;
        let lifecycle = storage.acquire_instance_lifecycle_lock(&stop_id)?;
        Ok((storage, title, lifecycle))
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result);
    let reload = state.session_service.disk_reload_guard().await;
    let (storage, title, lifecycle) = match opened {
        Ok(opened) => opened,
        Err(error) => {
            let publication = state.publication.write().await;
            return adopt_lifecycle_commit(state, profile, id, Err(error), &reload, &publication)
                .await;
        }
    };
    let prepared = tokio::task::spawn_blocking(move || {
        let transition = storage.acquire_write_transition();
        (storage, title, lifecycle, reload, transition)
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())?;
    let (storage, title, lifecycle, reload, transition) = prepared;
    let transition = transition.map_err(|error| {
        tracing::warn!(target: "http.api.sessions", %error, "stop storage transition failed");
        StatusCode::SERVICE_UNAVAILABLE.into_response()
    })?;

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
            // A peer changed the view after the caller selected the terminal stop path.
            if row.is_structured() {
                return Err(LifecycleTargetError::Busy.into());
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
            Ok(Some(generation))
        });
        (storage, lifecycle, reload, transition, result)
    })
    .await;
    let (storage, lifecycle, reload, transition, result) = match reserved {
        Ok(reserved) => reserved,
        Err(error) => {
            tracing::warn!(target: "http.api.sessions", %error, "stop reservation task failed");
            return Err(StatusCode::SERVICE_UNAVAILABLE.into_response());
        }
    };
    let generation =
        adopt_lifecycle_commit(state, profile.clone(), id, result, &reload, &publication).await?;
    drop(publication);
    drop(transition);
    drop(reload);
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

    let (effect, pi_update) = match tokio::task::spawn_blocking(move || {
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
        Ok((storage, lifecycle, panes))
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result);
    let reload = state.session_service.disk_reload_guard().await;
    let (storage, lifecycle, panes) = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            let publication = state.publication.write().await;
            return adopt_lifecycle_commit(state, profile, id, Err(error), &reload, &publication)
                .await;
        }
    };
    let (storage, lifecycle, panes, reload, transition) = tokio::task::spawn_blocking(move || {
        let transition = storage.acquire_write_transition()?;
        Ok::<_, anyhow::Error>((storage, lifecycle, panes, reload, transition))
    })
    .await
    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE.into_response())?
    .map_err(|_| StatusCode::SERVICE_UNAVAILABLE.into_response())?;
    let publication = state.publication.write().await;
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
        Ok::<_, anyhow::Error>(((storage, lifecycle, reload, transition), result))
    })
    .await;
    let (prepared, result) = match finished {
        Ok(Ok(finished)) => finished,
        Ok(Err(error)) => {
            tracing::warn!(target: "http.api.sessions", %error, "stop finalization failed");
            return Err(StatusCode::SERVICE_UNAVAILABLE.into_response());
        }
        Err(error) => {
            tracing::warn!(target: "http.api.sessions", %error, "stop finalization task failed");
            return Err(StatusCode::SERVICE_UNAVAILABLE.into_response());
        }
    };
    adopt_lifecycle_commit(state, profile, id, result, &prepared.2, &publication).await?;
    {
        let metadata = state.canonical_metadata.read().await;
        let mut rows = state.instances.write().await;
        let legacy_scope = crate::server::pane::legacy_tool_scope_for_panes(
            &rows,
            &metadata.raw_tool_names,
            panes.as_ref(),
        );
        let row = rows
            .iter_mut()
            .find(|row| row.id == id)
            .ok_or_else(crate::server::api::session_gone_after_persist)?;
        let tools = metadata
            .auxiliary_tools
            .get(&row.source_profile)
            .map(Vec::as_slice)
            .unwrap_or_default();
        crate::server::pane::sample_panes(row, tools, panes.as_ref(), legacy_scope.as_ref(), None);
        // Direct stop bypasses the status-intent path that clears revival debt.
        row.plugin_revival_pending = false;
        state
            .mutation_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    drop(publication);
    drop(prepared);
    drop(title);
    if let Err(error) = effect {
        tracing::warn!(target: "http.api.sessions", session = %id, "stop resources failed: {error}");
        return Err(api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "stop_failed",
            "Failed to stop session resources",
        ));
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
    let (profile, view) = {
        let rows = state.instances.read().await;
        let Some(row) = rows.iter().find(|row| row.id == id) else {
            return crate::server::api::session_not_found();
        };
        let view = if row.is_structured() {
            "structured"
        } else {
            "terminal"
        };
        (row.source_profile.clone(), view)
    };
    if let Err(response) = stop_with_commit(&state, profile.clone(), &id).await {
        return response;
    }
    tracing::info!(target: "session.stop", session = %id, %profile, view, "session stopped");
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
    // The restart body sets the same spawn fields the create route accepts, so
    // it answers with the same gate and the same `400 validation_failed`.
    if let Err(message) = super::validate_shell_fields(&[(
        body.extra_args.as_deref().unwrap_or_default(),
        "extra_args",
    )]) {
        return api_error(StatusCode::BAD_REQUEST, "validation_failed", message);
    }
    prepare_agent_session(
        state,
        id,
        Ok(None),
        AgentPreparation::Restart,
        Some(Arc::new(body)),
    )
    .await
}

/// Restart a sandboxed session on a new container, created from the current local image. The
/// container's filesystem outside its mounts is lost; the conversation resumes as on a restart.
pub async fn recreate_container(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    if let Some(response) = crate::server::api::read_only_block(&state)
        .or_else(|| crate::server::api::cityhall_block(&state))
    {
        return response;
    }
    let sandboxed = {
        let instances = state.instances.read().await;
        let Some(instance) = instances.iter().find(|row| row.id == id) else {
            return crate::server::api::session_not_found();
        };
        instance.is_sandboxed()
    };
    if !sandboxed {
        return api_error(
            StatusCode::CONFLICT,
            "not_sandboxed",
            "Only a sandboxed session has a container to recreate".to_string(),
        );
    }
    prepare_agent_session(
        state,
        id,
        Ok(None),
        AgentPreparation::Restart,
        Some(Arc::new(crate::daemon::RestartSessionBody {
            discard_sandbox_container: true,
            ..Default::default()
        })),
    )
    .await
}

/// Restart a session whose agent binary was replaced after launch (#4214). Eligibility is
/// re-checked here because the list that offered it may be several seconds old.
pub async fn restart_for_update(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    let instance = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return crate::server::api::session_not_found();
        };
        inst.clone()
    };
    let structured_ready = state
        .acp_supervisor
        .worker_states_snapshot()
        .await
        .get(&id)
        .is_some_and(|(worker, _)| *worker == crate::daemon::AcpWorkerState::Running)
        && state
            .acp_event_store
            .pending_approval_requests(&id)
            .is_empty();
    let resume_available = update_restart_resumable(&instance, &context_resume_for(&instance));
    if !crate::session::update_restart::offer_pending(&instance, resume_available, structured_ready)
    {
        return api_error(
            StatusCode::CONFLICT,
            "not_eligible",
            "Session is busy or its agent is not out of date".to_string(),
        );
    }
    apply_update_restart(&state, id, instance).await
}

/// Restart an eligible session onto the updated agent.
pub(super) async fn apply_update_restart(
    state: &Arc<AppState>,
    id: String,
    instance: Instance,
) -> axum::response::Response {
    if instance.is_structured() {
        // The reconciler terminates the worker once its turn drains and respawns it against the
        // stored conversation.
        state.acp_supervisor.mark_build_respawn_pending(&id);
        let response = SessionResponse::from_instance(
            &instance,
            crate::claude_settings::read_tui_fullscreen(),
        );
        return (StatusCode::ACCEPTED, Json(response)).into_response();
    }
    // The offer promises the conversation survives, and a restart that honors
    // `auto_resume_on_restart` would start a new one when it is off.
    let auto_resume = crate::session::resolve_config(&instance.effective_profile())
        .map(|config| config.session.auto_resume_on_restart)
        .unwrap_or(true);
    if !auto_resume {
        return api_error(
            StatusCode::CONFLICT,
            "auto_resume_disabled",
            "auto_resume_on_restart is off, so a restart would start a new conversation"
                .to_string(),
        );
    }
    let body = crate::daemon::RestartSessionBody {
        wake_message: Some(String::new()),
        only_if_idle: true,
        ..Default::default()
    };
    prepare_agent_session(
        state.clone(),
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
    account_swap: bool,
) -> anyhow::Result<()> {
    let now = chrono::Utc::now();
    if let Err(blocked) = row.ensure_startable() {
        return Err(LifecycleTargetError::Blocked(blocked).into());
    }
    if matches!(row.status, Status::Creating | Status::Deleting)
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
            if account_swap {
                row.swap_account(tool);
            } else {
                row.swap_tool(tool);
            }
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

fn reserve_structured_restart(
    state: Arc<AppState>,
    instance: &Instance,
    expected_generation: Option<u64>,
    body: &crate::daemon::RestartSessionBody,
    restart_origin: RestartOrigin,
) -> anyhow::Result<(crate::server::session_store::NativeSessionStore, Instance)> {
    use crate::session::SessionStore;
    let native = crate::server::session_store::NativeSessionStore::open(
        state.clone(),
        &instance.source_profile,
        Some(instance.id.clone()),
    )?;
    native.configuration(Some(&instance.source_profile))?;
    let _identity = crate::session::acquire_session_identity_lock()?;
    let _title = crate::session::acquire_session_title_lock(&instance.id)?;
    let _lifecycle = native
        .storage()
        .acquire_instance_lifecycle_lock(&instance.id)?;
    let mut probe = native
        .load()?
        .into_iter()
        .find(|row| row.id == instance.id)
        .ok_or(crate::session::SessionGone)?;
    anyhow::ensure!(
        probe.lifecycle_generation == instance.lifecycle_generation
            && expected_generation.is_none_or(|expected| expected == probe.lifecycle_generation),
        crate::session::LifecycleReservationError::Superseded
    );
    if matches!(restart_origin, RestartOrigin::StartupRecovery) {
        // Preserve archived/trashed refusals.
        probe
            .ensure_startable()
            .map_err(LifecycleTargetError::Blocked)?;
        return Err(LifecycleTargetError::Busy.into());
    }
    admit_restart(&mut probe, body, false)?;
    if let Some(target) = body
        .profile
        .as_deref()
        .filter(|target| *target != instance.source_profile)
    {
        let target_native = crate::server::session_store::NativeSessionStore::open(
            state.clone(),
            target,
            Some(instance.id.clone()),
        )?;
        (&target_native as &dyn SessionStore)
            .launch_configuration(std::path::Path::new(&instance.project_path))?;
        let rows = target_native.load()?;
        if rows.iter().any(|row| row.id == instance.id)
            || is_duplicate_session(rows.iter(), &probe.title, &probe.project_path, None)
        {
            return Err(duplicate_session_error(&probe.title));
        }
    } else {
        (&native as &dyn SessionStore)
            .launch_configuration(std::path::Path::new(&instance.project_path))?;
    }
    let reserved = (&native as &dyn SessionStore).update(|rows, _| {
        let row = rows
            .iter_mut()
            .find(|row| row.id == instance.id)
            .ok_or(crate::session::SessionGone)?;
        row.ensure_startable()?;
        anyhow::ensure!(
            row.lifecycle_generation == instance.lifecycle_generation,
            crate::session::LifecycleReservationError::Superseded
        );
        if matches!(row.status, Status::Creating | Status::Deleting)
            || row.has_fresh_lifecycle_reservation(chrono::Utc::now())
        {
            return Err(LifecycleTargetError::Busy.into());
        }
        row.try_acquire_lifecycle_reservation(
            LifecycleOperation::Launch,
            Instance::LIFECYCLE_RESERVATION_TTL,
            chrono::Utc::now(),
        )?;
        row.status = Status::Starting;
        Ok(row.clone())
    })?;
    Ok((native, reserved))
}

struct StructuredRestartLaunch {
    generation: u64,
    started: Instance,
    native: crate::server::session_store::NativeSessionStore,
    hooks: anyhow::Result<()>,
    carry: Option<crate::session::conversation_carry::ConversationCarry>,
    wake: Option<String>,
    reason: &'static str,
}

async fn finish_structured_restart(
    state: &Arc<AppState>,
    id: &str,
    launch: StructuredRestartLaunch,
) -> axum::response::Response {
    let reason = launch.reason;
    let result = async {
        use crate::session::SessionStore;
        let namespace = state.profile_namespace.read().await;
        let submission = state.session_service.prompt_submission(id).await;
        let lock = state.instance_lock(id).await;
        let guard = lock.lock().await;
        let supervisor = state.acp_supervisor.clone();
        let StructuredRestartLaunch {
            generation,
            mut started,
            native,
            hooks,
            carry,
            wake,
            reason: _,
        } = launch;
        let (started, native, wake, reservation) = tokio::task::spawn_blocking(move || {
            let sandbox_info = started.sandbox_info.take();
            let ownership =
                match started.reacquire_launch_locks_after_hooks(&native, generation, hooks) {
                    Ok(ownership) => ownership,
                    Err(error) => {
                        native.adopt_runtime_fields(&started)?;
                        return Err(error);
                    }
                };
            started.sandbox_info = sandbox_info;
            let prepared = (|| {
                let reservation =
                    match tokio::runtime::Handle::current().block_on(supervisor.begin_resume(
                        &started.id,
                        crate::acp::supervisor::ResumeKind::Spawn,
                        crate::process::worker_registry::ResumeAuthority::Explicit,
                    ))? {
                        crate::acp::supervisor::ResumeReservationOutcome::Reserved(lease) => lease,
                        crate::acp::supervisor::ResumeReservationOutcome::AlreadyPresent => {
                            return Err(crate::acp::supervisor::SupervisorError::AlreadyRunning(
                                started.id.clone(),
                            )
                            .into());
                        }
                    };
                if let Some(carry) = carry {
                    carry.run_for(&mut started)?;
                }
                let wake = match wake {
                    Some(wake) => wake,
                    None => {
                        native
                            .configuration(Some(&started.source_profile))?
                            .session
                            .restart_wake_message
                    }
                };
                (&native as &dyn SessionStore).update(|rows, _| {
                    let row = rows
                        .iter_mut()
                        .find(|row| row.id == started.id)
                        .ok_or(crate::session::SessionGone)?;
                    anyhow::ensure!(
                        row.lifecycle_reservation_is_owned(LifecycleOperation::Launch, generation),
                        crate::session::LifecycleReservationError::Superseded
                    );
                    row.sandbox_info.clone_from(&started.sandbox_info);
                    row.agent_session_binding
                        .clone_from(&started.agent_session_binding);
                    row.resume_binding.clone_from(&started.resume_binding);
                    row.status = Status::Starting;
                    row.release_lifecycle_reservation_if_owned(
                        LifecycleOperation::Launch,
                        generation,
                    );
                    Ok(())
                })?;
                Ok::<_, anyhow::Error>((reservation, wake))
            })();

            let (reservation, wake) = match prepared {
                Ok(prepared) => prepared,
                Err(error) => {
                    drop(ownership);
                    let refusal =
                        started.reacquire_launch_locks_after_hooks(&native, generation, Err(error));
                    native.adopt_runtime_fields(&started)?;
                    return Err(refusal.err().expect("a failed launch is refused"));
                }
            };
            started.status = Status::Starting;
            started.lifecycle_reservation = None;
            native.adopt_runtime_fields(&started)?;
            Ok::<_, anyhow::Error>((started, Arc::new(native), wake, reservation))
        })
        .await??;
        let agent = state
            .acp_supervisor
            .pick_agent_for_tool(
                &started.tool,
                started.agent_name.as_deref(),
                &started.source_profile,
                std::path::Path::new(&started.project_path),
            )
            .await;
        let mut request = crate::server::api::acp::spawn_request_for(
            &started,
            agent,
            started.sandbox_info.clone(),
        );
        request.launch_admission = Some(crate::acp::supervisor::LaunchAdmission {
            store: native.clone(),
            generation,
            namespace: Some(state.profile_namespace.clone()),
        });
        let profile = started.source_profile.clone();
        drop(guard);
        drop(submission);
        drop(namespace);
        let spawned = state
            .acp_supervisor
            .spawn_inner(request, reservation)
            .await
            .map_err(anyhow::Error::new);
        crate::server::session_spawn::finish_structured_launch(
            state, native, id, generation, spawned,
        )
        .await?;
        if !wake.is_empty() {
            let state = state.clone();
            let id = id.to_owned();
            let profile = profile.clone();
            state
                .session_service
                .work
                .clone()
                .spawn("acp.restart_wake", async move {
                    let _submission = state.session_service.prompt_submission(&id).await;
                    let matches = state.instances.read().await.iter().any(|row| {
                        row.id == id
                            && row.lifecycle_generation == generation
                            && row.source_profile == profile
                            && row.launch_is_finalized()
                            && row.ensure_startable().is_ok()
                    });
                    let dispatch = state
                        .session_service
                        .prompt_dispatch_under_submission(&id, &wake, false, true)
                        .await;
                    if matches
                        && !matches!(
                            dispatch,
                            crate::acp::dispatch::PromptDispatch::Queued { .. }
                        )
                    {
                        let result = state
                            .session_service
                            .send_turn(
                                &crate::server::session_service::SessionCaller::User,
                                &id,
                                crate::server::session_service::SendTurnRequest {
                                    resume_authority:
                                        crate::process::worker_registry::ResumeAuthority::Automatic,
                                    text: &wake,
                                    attachments: &[],
                                    woke_idle_dormant: false,
                                    prompt_id: None,
                                    synthesized: true,
                                    no_revive: true,
                                },
                            )
                            .await;
                        if let Err(error) = result {
                            tracing::warn!(target: "http.api.sessions", session = %id, %error, "structured restart wake failed");
                        }
                    }
                });
        }
        Ok::<_, anyhow::Error>((generation, profile))
    }
    .await;
    match result {
        Ok((generation, profile)) => {
            tracing::info!(target: "session.restart", session = %id, view = "structured", reason, "session restarted");
            crate::server::runtime::session_mutation_response(
                state,
                id,
                Some(crate::daemon::RestartOutcome {
                    lifecycle_generation: generation,
                    profile,
                    target: None,
                }),
            )
            .await
        }
        Err(error) => {
            if let Some(response) = lifecycle_rejection(state, &error) {
                return response;
            }
            tracing::warn!(target: "http.api.sessions", session = %id, %error, "structured restart failed");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "start_failed",
                "Structured restart failed",
            )
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum AgentPreparation {
    Start,
    Restart,
    Ensure,
}

#[derive(Clone, Copy)]
enum RestartOrigin {
    Explicit,
    StartupRecovery,
}

fn restart_reason(
    origin: RestartOrigin,
    body: Option<&crate::daemon::RestartSessionBody>,
) -> &'static str {
    match (origin, body) {
        (RestartOrigin::StartupRecovery, _) => "startup_recovery",
        (_, Some(body)) if body.discard_sandbox_container => "recreate_container",
        (_, Some(body)) if body.only_if_idle => "agent_update",
        _ => "user",
    }
}

fn select_restart_wake_message(
    origin: RestartOrigin,
    requested: Option<String>,
    profile: &str,
) -> String {
    requested.unwrap_or_else(|| {
        let session = crate::session::resolve_config(profile)
            .map(|config| config.session)
            .unwrap_or_default();
        if matches!(origin, RestartOrigin::StartupRecovery) && !session.startup_recovery_wake {
            String::new()
        } else {
            session.restart_wake_message
        }
    })
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
    prepare_agent_session_inner(
        state,
        id,
        body,
        preparation,
        restart,
        None,
        RestartOrigin::Explicit,
    )
    .await
}

pub(crate) async fn recovery_restart(
    state: Arc<AppState>,
    id: String,
    expected_generation: u64,
) -> axum::response::Response {
    prepare_agent_session_inner(
        state,
        id,
        Ok(None),
        AgentPreparation::Restart,
        Some(Arc::new(crate::daemon::RestartSessionBody {
            bound_hooks: true,
            ..Default::default()
        })),
        Some(expected_generation),
        RestartOrigin::StartupRecovery,
    )
    .await
}

async fn prepare_agent_session_inner(
    state: Arc<AppState>,
    id: String,
    body: Result<
        Option<Json<crate::daemon::StartSessionBody>>,
        axum::extract::rejection::JsonRejection,
    >,
    preparation: AgentPreparation,
    restart: Option<Arc<crate::daemon::RestartSessionBody>>,
    expected_generation: Option<u64>,
    restart_origin: RestartOrigin,
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
    let mut instance = {
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
    // The eligibility check that asked for this ran before these locks, and a prompt can land
    // while it waited, so the idle promise is enforced on the row this request reloaded.
    if restart.as_ref().is_some_and(|body| body.only_if_idle) && instance.status != Status::Idle {
        return api_error(
            StatusCode::CONFLICT,
            "not_eligible",
            "Session is busy or its agent is not out of date".to_string(),
        );
    }
    // #4116: a shelved row refuses every start path with its own code, decided on
    // the row this request reloaded. It answers before the lifecycle reservation,
    // before the store write that would report it missing, and before the
    // pre-launch hooks, so a shelved row never comes back `lifecycle_busy` or
    // `not_found`. The write below still rechecks the row under the lock, for a
    // peer that shelves it while this request waits.
    if let Err(blocked) = instance.ensure_startable() {
        return crate::server::api::start_blocked_response(blocked);
    }
    let body = match body {
        Ok(body) => body.map(|Json(body)| body).unwrap_or_default(),
        Err(rejection) => return rejection.into_response(),
    };
    if let Some(target) = restart.as_ref().and_then(|body| body.profile.as_deref()) {
        if !state
            .canonical_metadata
            .read()
            .await
            .profiles
            .iter()
            .any(|profile| profile.name == target)
        {
            return api_error(
                StatusCode::BAD_REQUEST,
                "profile_not_found",
                format!("Profile '{target}' does not exist"),
            );
        }
    }
    if instance.is_structured()
        && matches!(restart_origin, RestartOrigin::Explicit)
        && (instance.status == Status::Stopped || instance.is_idle_dormant())
    {
        instance = match resume_structured_stop(&state, instance).await {
            Ok(instance) => instance,
            Err(response) => return response,
        };
    }
    let structured = instance.is_structured();
    if structured && restart.is_some() {
        let worker_state = state.clone();
        let candidate = instance.clone();
        let restart_body = restart.clone().expect("restart body");
        let reserved = tokio::task::spawn_blocking(move || {
            reserve_structured_restart(
                worker_state,
                &candidate,
                expected_generation,
                &restart_body,
                restart_origin,
            )
        })
        .await;
        let (native, reserved) = match reserved {
            Ok(Ok(reserved)) => reserved,
            Ok(Err(error)) => {
                return lifecycle_rejection(&state, &error)
                    .unwrap_or_else(|| StatusCode::INTERNAL_SERVER_ERROR.into_response());
            }
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        let stopped = state
            .acp_supervisor
            .shutdown_and_wait(&id, std::time::Duration::from_secs(5))
            .await;
        let released = tokio::task::spawn_blocking(move || {
            (&native as &dyn crate::session::SessionStore).update(|rows, _| {
                let row = rows
                    .iter_mut()
                    .find(|row| row.id == reserved.id)
                    .ok_or(crate::session::SessionGone)?;
                anyhow::ensure!(
                    row.lifecycle_reservation_is_owned(
                        LifecycleOperation::Launch,
                        reserved.lifecycle_generation
                    ),
                    crate::session::LifecycleReservationError::Superseded
                );
                row.release_lifecycle_reservation_if_owned(
                    LifecycleOperation::Launch,
                    reserved.lifecycle_generation,
                );
                row.idle_dormant_since = Some(chrono::Utc::now());
                Ok(row.clone())
            })
        })
        .await;
        instance = match released {
            Ok(Ok(instance)) => instance,
            _ => return StatusCode::CONFLICT.into_response(),
        };
        if let Err(error) = stopped {
            tracing::warn!(target: "http.api.sessions", session = %id, %error, "structured restart did not prove teardown");
            return StatusCode::CONFLICT.into_response();
        }
    }
    let size = restart
        .as_ref()
        .and_then(|body| body.size.as_ref())
        .or(body.size.as_ref())
        .map(|size| (size.cols.get(), size.rows.get()));
    let expected_restart_generation = (instance.is_structured() && restart.is_some())
        .then_some(instance.lifecycle_generation)
        .or(expected_generation);
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
        if worker_restart.is_some() {
            // A row missing from the store and a reservation this row no longer
            // owns are one error here; only the first is a session that is gone,
            // so a peer that purged it while this request waited must not be
            // told the lifecycle is busy. The extra read runs on that path
            // alone. Mirrors `acquire_auxiliary_locks_in`.
            if let Err(error) = outgoing.reconcile_from_store(&native) {
                if matches!(
                    error.downcast_ref::<crate::session::LifecycleReservationError>(),
                    Some(crate::session::LifecycleReservationError::Superseded)
                ) && native.load()?.iter().all(|row| row.id != instance.id)
                {
                    return Err(crate::session::SessionGone.into());
                }
                return Err(error);
            }
            if let Some(expected) = expected_restart_generation {
                anyhow::ensure!(
                    outgoing.lifecycle_generation == expected,
                    crate::session::LifecycleReservationError::Superseded
                );
            }
        }
        let (account_swap, mut conversation_carry) = match worker_restart
            .as_ref()
            .and_then(|body| body.tool.as_deref())
        {
            Some(tool) if tool != outgoing.tool => {
                match crate::session::conversation_carry::classify(
                    &outgoing,
                    target.unwrap_or(&profile),
                    tool,
                ) {
                    crate::session::conversation_carry::ToolSwap::KeepConversation(carry) => {
                        (true, carry)
                    }
                    crate::session::conversation_carry::ToolSwap::Park => (false, None),
                }
            }
            _ => (false, None),
        };
        if let Some(body) = &worker_restart {
            let mut probe = outgoing.clone();
            if let Some(target) = target {
                probe.source_profile = target.into();
            }
            admit_restart(&mut probe, body, account_swap)?;
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
                    admit_restart(candidate, body, account_swap)?;
                    moved = Some(candidate.clone());
                    Ok(())
                },
            )?;
            if let (Some(carry), Some(moved)) = (conversation_carry.as_mut(), moved.as_ref()) {
                carry.retarget(crate::session::conversation_carry::conversation_ids(moved));
            }
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
                    admit_restart(row, body, account_swap)?;
                    return Ok(Some(row.clone()));
                }
                let now = chrono::Utc::now();
                if row.is_structured() {
                    anyhow::ensure!(
                        row.lifecycle_generation == instance.lifecycle_generation,
                        crate::session::LifecycleReservationError::Superseded
                    );
                }
                // Re-read from the store under the lifecycle lock, so a peer
                // that archived or trashed the row while this request waited
                // is refused with its own code, not a generic busy (#4116).
                if let Err(blocked) = row.ensure_startable() {
                    return Err(LifecycleTargetError::Blocked(blocked).into());
                }
                if matches!(row.status, Status::Creating | Status::Deleting)
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
        Ok(Some((generation, started, native, conversation_carry)))
    })
    .await;
    drop(guard);
    drop(submission);
    drop(namespace);
    let reason = restart_reason(restart_origin, restart.as_deref());
    let requested_wake_message = restart.as_ref().and_then(|body| body.wake_message.clone());
    let hooked = match result {
        Ok(Ok(Some((generation, mut started, native, conversation_carry)))) => {
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
                Ok(Some((
                    generation,
                    started,
                    native,
                    hooks,
                    conversation_carry,
                )))
            })
            .await
        }
        Ok(Ok(None)) => Ok(Ok(None)),
        Ok(Err(error)) => Ok(Err(error)),
        Err(error) => Err(error),
    };
    let hooked = match hooked {
        Ok(Ok(Some((generation, started, native, hooks, carry)))) if started.is_structured() => {
            return finish_structured_restart(
                &state,
                &id,
                StructuredRestartLaunch {
                    generation,
                    started,
                    native,
                    hooks,
                    carry,
                    wake: requested_wake_message,
                    reason,
                },
            )
            .await;
        }
        hooked => hooked,
    };
    let namespace = state.profile_namespace.read().await;
    let submission = state
        .session_service
        .prompt_submission_for_session(&id)
        .await;
    let guard = lock.lock().await;
    let mut restart_identity = None;
    let result = match hooked {
        Ok(Ok(Some((generation, mut started, native, hooks, conversation_carry)))) => {
            tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                use crate::session::SessionStore;
                let outcome = started.finish_reserved_launch(
                    &native,
                    size,
                    crate::session::ResumeLaunchOptions {
                        resume_policy: if preparation == AgentPreparation::Ensure {
                            crate::session::ResumeAttemptPolicy::Allow
                        } else {
                            crate::session::ResumeAttemptPolicy::HonorAutoResumeSetting
                        },
                        restart: true,
                        conversation_carry,
                    },
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
                let metadata = state.canonical_metadata.read().await;
                let mut instances = state.instances.write().await;
                let legacy_scope = crate::server::pane::legacy_tool_scope_for_panes(
                    &instances,
                    &metadata.raw_tool_names,
                    panes.as_ref(),
                );
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
                        let tools = metadata
                            .auxiliary_tools
                            .get(&row.source_profile)
                            .map(Vec::as_slice)
                            .unwrap_or_default();
                        crate::server::pane::sample_panes(
                            row,
                            tools,
                            panes.as_ref(),
                            legacy_scope.as_ref(),
                            None,
                        );
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
            let response = if let Some(response) = lifecycle_rejection(&state, &error) {
                response
            } else if error.is::<crate::session::NativeStoreUnavailable>() {
                StatusCode::SERVICE_UNAVAILABLE.into_response()
            } else if let Some(refusal) =
                error.downcast_ref::<crate::session::ManagedResumeRefused>()
            {
                let mut response = crate::server::api::api_error_with_code_header(
                    crate::daemon::ApiErrorCode::ManagedResumeRefused(refusal.0),
                    refusal.0.message(),
                );
                response.headers_mut().insert(
                    crate::daemon::MANAGED_RESUME_CAUSE_HEADER,
                    axum::http::HeaderValue::from_static(refusal.0.as_str()),
                );
                response
            } else {
                tracing::warn!(target: "http.api.sessions", session = %id, %error, "session start failed");
                api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "start_failed",
                    "Session start failed",
                )
            };
            if restart_identity.is_some() {
                drop(guard);
                drop(submission);
                drop(namespace);
                return match state.runtime.publish(&state).await {
                    Ok(snapshot) => {
                        crate::server::runtime::mutation_response(&snapshot.value.cursor, response)
                    }
                    Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
                };
            }
            return response;
        }
        Ok(Some(crate::session::StartOutcome::Indeterminate { cause })) => {
            let response = super::ensure::indeterminate_launch_response(cause);
            drop(guard);
            drop(submission);
            drop(namespace);
            return match state.runtime.publish(&state).await {
                Ok(snapshot) => {
                    crate::server::runtime::mutation_response(&snapshot.value.cursor, response)
                }
                Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
            };
        }
        Ok(Some(crate::session::StartOutcome::ResumeFailed { sid })) => {
            let response = (StatusCode::CONFLICT, crate::daemon::ApiErrorCode::ResumeFailed.header(), Json(serde_json::json!({
                "error": "resume_failed", "message": "Resume failed; preserved for explicit retry", "resume_session_id": sid,
            }))).into_response();
            drop(guard);
            drop(submission);
            drop(namespace);
            return match state.runtime.publish(&state).await {
                Ok(snapshot) => {
                    crate::server::runtime::mutation_response(&snapshot.value.cursor, response)
                }
                Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
            };
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
                lifecycle_generation: generation,
                profile: profile.clone(),
            })
        };
        if matches!(
            outcome,
            Some(
                crate::session::StartOutcome::Fresh
                    | crate::session::StartOutcome::Resumed
                    | crate::session::StartOutcome::FreshAfterFailedResume { .. }
            )
        ) {
            let wake_message =
                select_restart_wake_message(restart_origin, requested_wake_message, &profile);
            if !wake_message.is_empty() {
                crate::session::restart::spawn_wake_worker(
                    id.clone(),
                    row.title.clone(),
                    row.tool.clone(),
                    wake_message,
                    Some((profile.clone(), generation)),
                );
            }
        }
        drop(rows);
        tracing::info!(target: "session.restart", session = %id, view = "terminal", reason, "session restarted");
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
        let response = super::ensure::agent_target_response(&state, &id, outcome).await;
        if !response.status().is_success() && restart_identity.is_some() {
            match state.runtime.publish(&state).await {
                Ok(snapshot) => {
                    crate::server::runtime::mutation_response(&snapshot.value.cursor, response)
                }
                Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
            }
        } else {
            response
        }
    } else {
        if outcome.is_some() || structured {
            tracing::info!(target: "session.start", session = %id, ?outcome, "session started");
        }
        crate::server::runtime::session_mutation_response(&state, &id, None::<()>).await
    }
}

#[cfg(all(test, unix))]
mod durable_stop_tests {
    use super::*;
    use crate::process::worker_registry::{self, WorkerRecord};

    #[tokio::test]
    #[serial_test::serial]
    async fn managed_resume_refusal_publishes_safe_recovery_without_replacing_the_pane() {
        use crate::tmux::test_helpers::{
            only_pane_id, pane_field, tmux_available, wait_for_pane_command, TmuxTestSession,
        };
        if !tmux_available() {
            return;
        }
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        let _env = crate::session::test_support::EnvGuard::unset(&[
            "TMUX",
            "TMUX_PANE",
            "OMP_SESSION_ID",
            "PI_SESSION_ID",
            "AOE_SESSION_ID",
            "AOE_PROFILE",
            "AOE_DAEMON_URL",
            "AOE_DAEMON_TOKEN",
            "PRIME_AGENT_SESSION_DIR",
            "PRIME_AGENT_CODING_AGENT_SESSION_DIR",
            "PRIME_AGENT_CODING_AGENT_DIR",
        ])
        .and_set("AOE_TMUX_SOCKET", root.path().join("tmux.sock"));
        let _omp = crate::session::test_support::install_login_shell_path_command(
            root.path(),
            "omp",
            "#!/bin/sh\nexit 97\n",
        );
        let _prime = crate::session::test_support::install_login_shell_path_command(
            root.path(),
            "prime-agent",
            "#!/bin/sh\nexit 97\n",
        );
        crate::session::config::update_app_state(|state| {
            state.has_acknowledged_agent_hooks = true;
        })
        .unwrap();
        crate::session::config::update_config(|config| {
            config.systemd.enabled = false;
            config.session.smart_rename = false;
            config.session.auto_resume_on_restart = true;
        })
        .unwrap();
        let project = root.path().join("private-project");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir_all(project.join(".prime/agent")).unwrap();
        std::fs::write(
            project.join(".prime/agent/settings.json"),
            r#"{"sessionDir":7}"#,
        )
        .unwrap();
        let transcript = root.path().join("private-transcript.jsonl");
        std::fs::write(&transcript, "private-conversation-body\n").unwrap();
        let sid = "11111111-2222-4333-8444-555555555555";
        for tool in ["omp", "prime-agent"] {
            let mut row = Instance::new("managed refusal", project.to_str().unwrap());
            row.tool = tool.into();
            row.command = tool.into();
            row.source_profile = "default".into();
            row.status = Status::Stopped;
            row.agent_session_id = Some(sid.into());
            let mut binding = crate::session::ConversationBinding::unknown(sid);
            binding.transcript_path = Some(transcript.clone());
            row.agent_session_binding = Some(binding);
            row.omp_capture_generation = Some("original-generation".into());
            if tool == "omp" {
                assert!(row.supports_native_resume());
            } else {
                let agent = crate::agents::get_agent("prime-agent").unwrap();
                assert!(row.launch_invokes_resolved_agent_directly(agent));
                assert_eq!(row.command, "prime-agent");
                assert_eq!(row.agent_session_id.as_deref(), Some(sid));
                assert_eq!(
                    std::fs::read_to_string(project.join(".prime/agent/settings.json")).unwrap(),
                    r#"{"sessionDir":7}"#
                );
            }
            assert_eq!(row.resume_intent, crate::session::ResumeIntent::Default);
            let conversation = row.conversation_state();
            let name = crate::tmux::Session::generate_name(&row.id, &row.title);
            let _pane = TmuxTestSession::from_name(&name);
            assert!(crate::tmux::tmux_command()
                .args([
                    "-f",
                    "/dev/null",
                    "new-session",
                    "-d",
                    "-s",
                    &name,
                    "exec /bin/sleep 60"
                ])
                .output()
                .unwrap()
                .status
                .success());
            let pane = only_pane_id(&name);
            wait_for_pane_command(&pane, "sleep");
            let pid = pane_field(&pane, "#{pane_pid}");
            row.agent_pane.tmux_session = Some(name.clone());
            row.agent_pane.state = crate::session::PanePresence::Alive;
            crate::server::test_support::seed_instances_on_disk_for_test(
                "default",
                vec![row.clone()],
            );
            let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
            crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
            let before = state.runtime.publish(&state).await.unwrap();
            let response = restart_session(
                State(state.clone()),
                Path(row.id.clone()),
                Ok(Some(Json(crate::daemon::RestartSessionBody {
                    skip_on_launch: true,
                    wake_message: Some(String::new()),
                    ..Default::default()
                }))),
            )
            .await;
            let status = response.status();
            let headers = response.headers().clone();
            let body = axum::body::to_bytes(response.into_body(), 8192)
                .await
                .unwrap();
            let stored = Storage::open_unwatched("default")
                .unwrap()
                .load()
                .unwrap()
                .remove(0);
            assert_eq!(stored.conversation_state(), conversation);
            assert_eq!(stored.omp_capture_generation, row.omp_capture_generation);
            assert_eq!(stored.resume_probe_failed_sid, row.resume_probe_failed_sid);
            assert_eq!(stored.id, row.id);
            assert_eq!(stored.tool, row.tool);
            assert_eq!(stored.command, row.command);
            assert_eq!(stored.project_path, row.project_path);
            assert!(stored.lifecycle_reservation.is_none());
            assert_eq!(
                std::fs::read_to_string(&transcript).unwrap(),
                "private-conversation-body\n"
            );
            assert_eq!(only_pane_id(&name), pane);
            assert_eq!(pane_field(&pane, "#{pane_pid}"), pid);
            assert_eq!(pane_field(&pane, "#{pane_dead}"), "0");
            assert_eq!(
                status,
                StatusCode::CONFLICT,
                "{}",
                String::from_utf8_lossy(&body)
            );
            assert_eq!(
                headers[crate::daemon::ERROR_CODE_HEADER],
                "managed_resume_refused"
            );
            let expected_cause = crate::session::ManagedResumeCause::ExecutionContextUnattested;
            assert_eq!(
                headers[crate::daemon::MANAGED_RESUME_CAUSE_HEADER],
                expected_cause.as_str(),
            );
            let code = crate::daemon::ApiErrorCode::from_headers(status, &headers, false).unwrap();
            assert_eq!(code.as_str(), "managed_resume_refused");
            assert_eq!(
                headers[crate::daemon::RUNTIME_EPOCH_HEADER],
                before.value.cursor.epoch.as_str()
            );
            let revision = headers[crate::daemon::RUNTIME_REVISION_HEADER]
                .to_str()
                .unwrap()
                .parse::<u64>()
                .unwrap();
            assert!(revision > before.value.cursor.revision);
            let published = state.runtime.publish(&state).await.unwrap();
            assert_eq!(published.value.cursor.revision, revision);
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"], "managed_resume_refused");
            let message = body["message"].as_str().unwrap();
            assert!(message.contains("aoe session set-session-id"), "{message}");
            assert!(message.contains("--store"), "{message}");
            assert!(
                message.contains("aoe session set-session-id <session> \"\""),
                "{message}"
            );
            let public_body = body.to_string();
            for private in [
                sid,
                row.id.as_str(),
                project.to_str().unwrap(),
                transcript.to_str().unwrap(),
                "private-conversation-body",
            ] {
                assert!(!public_body.contains(private), "{public_body}");
            }
        }
    }

    async fn live_runner(row: &Instance) -> (tokio::process::Child, WorkerRecord) {
        let child = tokio::process::Command::new("sleep")
            .arg("60")
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let record = WorkerRecord::new(
            row.id.clone(),
            child.id().unwrap(),
            worker_registry::socket_path_for(&row.id).unwrap(),
            "sleep".into(),
            "claude".into(),
            std::path::PathBuf::from(&row.project_path),
            None,
            Vec::new(),
            Vec::new(),
            None,
            Some(row.source_profile.clone()),
        )
        .with_generation(81);
        assert!(record.runner_identity.is_some());
        worker_registry::save(&record).unwrap();
        (child, record)
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn explicit_generic_restart_consumes_a_stopped_native_generation_before_birth() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let _home = crate::session::test_support::isolate_app_dir();
        crate::session::config::update_app_state(|state| {
            state.has_acknowledged_agent_hooks = true;
        })
        .unwrap();
        let project = tempfile::tempdir().unwrap();
        let mut row = Instance::new("stopped generic restart", project.path().to_str().unwrap());
        row.source_profile = "default".into();
        row.view = crate::session::View::Structured;
        row.status = Status::Stopped;
        row.tool = "claude".into();
        row.agent_name = Some("claude".into());
        row.mark_idle_dormant();
        let observed = row
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Stop,
                Instance::LIFECYCLE_RESERVATION_TTL,
                chrono::Utc::now(),
            )
            .unwrap();
        crate::server::test_support::seed_instances_on_disk_for_test("default", vec![row.clone()]);
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
        let response = restart_session(
            State(state.clone()),
            Path(row.id.clone()),
            Ok(Some(Json(crate::daemon::RestartSessionBody {
                skip_on_launch: true,
                wake_message: Some(String::new()),
                ..Default::default()
            }))),
        )
        .await;
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), 8192)
            .await
            .unwrap();
        assert!(!status.is_success());
        assert_eq!(
            births.load(Ordering::SeqCst),
            1,
            "{status}: {}",
            String::from_utf8_lossy(&body)
        );
        let stored = Storage::open_unwatched("default")
            .unwrap()
            .load()
            .unwrap()
            .remove(0);
        assert!(stored.lifecycle_generation > observed);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn generic_restart_reservation_cannot_consume_a_newer_native_stop() {
        let _home = crate::session::test_support::isolate_app_dir();
        let project = tempfile::tempdir().unwrap();
        let mut row = Instance::new("stale restart", project.path().to_str().unwrap());
        row.source_profile = "default".into();
        row.view = crate::session::View::Structured;
        row.status = Status::Running;
        row.tool = "claude".into();
        crate::server::test_support::seed_instances_on_disk_for_test("default", vec![row.clone()]);
        let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        stop_structured_with_commit(&state, "default".into(), &row.id)
            .await
            .unwrap_or_else(|response| panic!("stop failed: {}", response.status()));
        let stop = Storage::open_unwatched("default")
            .unwrap()
            .load()
            .unwrap()
            .remove(0);
        let worker_state = state.clone();
        let result = tokio::task::spawn_blocking(move || {
            reserve_structured_restart(
                worker_state,
                &row,
                None,
                &crate::daemon::RestartSessionBody::default(),
                RestartOrigin::Explicit,
            )
        })
        .await
        .unwrap();
        let error = match result {
            Ok(_) => panic!("stale restart must be refused"),
            Err(error) => error,
        };
        assert!(error.is::<crate::session::LifecycleReservationError>());
        let retained = Storage::open_unwatched("default")
            .unwrap()
            .load()
            .unwrap()
            .remove(0);
        assert_eq!(retained.lifecycle_generation, stop.lifecycle_generation);
        assert_eq!(retained.status, Status::Stopped);
        assert!(retained.is_idle_dormant());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn structured_stop_stops_the_pinned_container_only_for_the_committed_generation() {
        for superseded in [false, true] {
            let _home = crate::session::test_support::isolate_app_dir();
            let project = tempfile::tempdir().unwrap();
            let runtime = tempfile::tempdir().unwrap();
            let (pin, calls) =
                crate::session::test_support::fake_sandbox_pin(runtime.path(), "c1", 0, "");
            let mut row = Instance::new("sandboxed stop", project.path().to_str().unwrap());
            row.source_profile = "default".into();
            row.view = crate::session::View::Structured;
            row.status = Status::Running;
            row.tool = "claude".into();
            row.sandbox_info = Some(crate::session::SandboxInfo {
                provider: None,
                enabled: true,
                container_id: Some("c1".into()),
                image: "img".into(),
                container_name: "aoe-sandboxed-stop".into(),
                extra_env: None,
                custom_instruction: None,
                before_start_env: Vec::new(),
                before_start_hook_env: Vec::new(),
                container_workdir: None,
            });
            row.sandbox_pin = Some(pin);
            crate::server::test_support::seed_instances_on_disk_for_test(
                "default",
                vec![row.clone()],
            );
            let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
            crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
            let worker_state = state.clone();
            let id = row.id.clone();
            let mut committed = tokio::task::spawn_blocking(move || {
                commit_structured_user_stop(worker_state, "default", &id)
            })
            .await
            .unwrap()
            .unwrap();
            if superseded {
                committed.generation += 1;
            }
            let _ = finish_structured_user_stop(&state, &row.id, committed).await;
            let log = std::fs::read_to_string(&calls).unwrap_or_default();
            assert_eq!(
                log.trim(),
                if superseded { "" } else { "stop c1" },
                "superseded={superseded}"
            );
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn structured_stop_leaves_the_pinned_container_when_retirement_is_unproven() {
        let _home = crate::session::test_support::isolate_app_dir();
        let project = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let (pin, calls) =
            crate::session::test_support::fake_sandbox_pin(runtime.path(), "c1", 0, "");
        let mut row = Instance::new("unproven retirement", project.path().to_str().unwrap());
        row.source_profile = "default".into();
        row.view = crate::session::View::Structured;
        row.status = Status::Running;
        row.tool = "claude".into();
        row.sandbox_info = Some(crate::session::SandboxInfo {
            provider: None,
            enabled: true,
            container_id: Some("c1".into()),
            image: "img".into(),
            container_name: "aoe-unproven".into(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            before_start_hook_env: Vec::new(),
            container_workdir: None,
        });
        row.sandbox_pin = Some(pin);
        crate::server::test_support::seed_instances_on_disk_for_test("default", vec![row.clone()]);
        let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        // An unattested live runner cannot be proven retired.
        let (_runner, mut record) = live_runner(&row).await;
        record.runner_identity = None;
        worker_registry::save(&record).unwrap();
        let response = stop_structured_with_commit(&state, "default".into(), &row.id)
            .await
            .expect_err("unproven retirement must not report success");
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(
            std::fs::read_to_string(&calls)
                .unwrap_or_default()
                .is_empty(),
            "the container must not be stopped before retirement is proven"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn native_stop_keeps_commit_failure_and_adoption_failure_distinct() {
        for adoption_failure in [false, true] {
            let _home = crate::session::test_support::isolate_app_dir();
            let project = tempfile::tempdir().unwrap();
            let mut row = Instance::new("stop commit boundary", project.path().to_str().unwrap());
            row.source_profile = "default".into();
            row.view = crate::session::View::Structured;
            row.status = Status::Running;
            if !adoption_failure {
                row.lifecycle_generation = u64::MAX;
            }
            crate::server::test_support::seed_instances_on_disk_for_test(
                "default",
                vec![row.clone()],
            );
            let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
            crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
            let (mut runner, record) = live_runner(&row).await;
            let worker_state = state.clone();
            let id = row.id.clone();
            let committed = tokio::task::spawn_blocking(move || {
                if adoption_failure {
                    let failing_state = worker_state.clone();
                    crate::server::session_store::before_next_adoption_for_test(move || {
                        failing_state
                            .canonical_metadata
                            .blocking_write()
                            .profiles
                            .clear();
                    });
                }
                commit_structured_user_stop(worker_state, "default", &id)
            })
            .await
            .unwrap();
            assert!(
                runner.try_wait().unwrap().is_none(),
                "admission commit cannot retire before its outcome"
            );
            if adoption_failure {
                let committed =
                    committed.expect("committed stop must retain physical retirement authority");
                assert!(committed
                    .publication
                    .as_ref()
                    .unwrap_err()
                    .is::<crate::session::SessionCommitApplied>());
                let response = finish_structured_user_stop(&state, &row.id, committed)
                    .await
                    .expect_err("unconfirmed adoption cannot report success");
                assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
                let body = axum::body::to_bytes(response.into_body(), 8192)
                    .await
                    .unwrap();
                let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(body["error"], "outcome_unconfirmed");
                tokio::time::timeout(std::time::Duration::from_secs(5), runner.wait())
                    .await
                    .unwrap()
                    .unwrap();
                assert!(worker_registry::load(&row.id).unwrap().is_none());
                let stored = Storage::open_unwatched("default")
                    .unwrap()
                    .load()
                    .unwrap()
                    .remove(0);
                assert_eq!(stored.status, Status::Stopped);
                assert!(stored.is_idle_dormant());
                assert!(stored.lifecycle_reservation.is_none());
            } else {
                let error = match committed {
                    Ok(_) => panic!("overflowed stop generation must not commit"),
                    Err(error) => error,
                };
                assert!(error.is::<crate::session::LifecycleReservationError>());
                assert_eq!(
                    worker_registry::load(&row.id).unwrap().unwrap().pid,
                    record.pid
                );
                let stored = Storage::open_unwatched("default")
                    .unwrap()
                    .load()
                    .unwrap()
                    .remove(0);
                assert_eq!(stored.status, Status::Running);
                assert_eq!(stored.lifecycle_generation, u64::MAX);
                assert!(!stored.is_idle_dormant());
                runner.kill().await.unwrap();
                worker_registry::retire_selected(&record).unwrap();
            }
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn internal_shutdown_idle_and_detach_do_not_commit_user_stop_admission() {
        for reason in ["provider_switch", "idle", "detach"] {
            let _home = crate::session::test_support::isolate_app_dir();
            let project = tempfile::tempdir().unwrap();
            let mut row = Instance::new(reason, project.path().to_str().unwrap());
            row.source_profile = "default".into();
            row.view = crate::session::View::Structured;
            row.status = Status::Running;
            crate::server::test_support::seed_instances_on_disk_for_test(
                "default",
                vec![row.clone()],
            );
            let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
            crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
            let (mut runner, record) = live_runner(&row).await;
            state
                .acp_supervisor
                .test_install_attached(
                    &row.id,
                    crate::acp::runner_lifecycle::RunnerIdentity::from_record(&record),
                )
                .await;
            match reason {
                "provider_switch" => state.acp_supervisor.shutdown(&row.id).await.unwrap(),
                "idle" => state
                    .acp_supervisor
                    .shutdown_idle(&row.id, None)
                    .await
                    .unwrap(),
                "detach" => state.acp_supervisor.detach_all().await,
                _ => unreachable!(),
            }
            let stored = Storage::open_unwatched("default")
                .unwrap()
                .load()
                .unwrap()
                .remove(0);
            assert_eq!(stored.status, Status::Running, "{reason}");
            assert_eq!(
                stored.lifecycle_generation, row.lifecycle_generation,
                "{reason}"
            );
            assert!(!stored.is_idle_dormant(), "{reason}");
            assert!(
                worker_registry::stop_intent(&row.id).unwrap().is_none(),
                "{reason}"
            );
            let admission = crate::acp::supervisor::LaunchAdmission {
                store: Arc::new(Storage::open_unwatched("default").unwrap()),
                generation: stored.lifecycle_generation,
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
            if runner.try_wait().unwrap().is_none() {
                runner.kill().await.unwrap();
            }
            worker_registry::retire_selected(&record).unwrap();
        }
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
            return api_error(StatusCode::BAD_REQUEST, "validation_failed", msg);
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
    if minutes.is_some() {
        tracing::info!(target: "session.lifecycle", session = %id, ?minutes, structured = was_structured_view, "session snoozed");
    } else {
        tracing::info!(target: "session.lifecycle", session = %id, structured = was_structured_view, "session woken");
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

#[cfg(test)]
mod restart_wake_tests {
    use super::{select_restart_wake_message, RestartOrigin};

    #[test]
    #[serial_test::serial]
    fn restart_wake_selection_respects_origin_and_profile_policy() -> anyhow::Result<()> {
        let _home = crate::session::test_support::isolate_app_dir();
        let config_path = crate::session::get_app_dir()?.join("config.toml");
        let profile = "wake-policy";
        let default_message = "wake up: pick up what you were doing";
        for (global, overrides, recovered, explicit) in [
            ("", "", "", default_message),
            (
                "startup_recovery_wake = false\nrestart_wake_message = '  custom text  '",
                "",
                "",
                "  custom text  ",
            ),
            (
                "startup_recovery_wake = true",
                "",
                default_message,
                default_message,
            ),
            (
                "startup_recovery_wake = true\nrestart_wake_message = '  custom text  '",
                "",
                "  custom text  ",
                "  custom text  ",
            ),
            (
                "startup_recovery_wake = true\nrestart_wake_message = ''",
                "",
                "",
                "",
            ),
            (
                "startup_recovery_wake = false",
                "startup_recovery_wake = true\nrestart_wake_message = 'profile text'",
                "profile text",
                "profile text",
            ),
            (
                "startup_recovery_wake = true",
                "startup_recovery_wake = false",
                "",
                default_message,
            ),
        ] {
            std::fs::write(&config_path, format!("[session]\n{global}\n"))?;
            let profile_config = toml::from_str(&format!("[session]\n{overrides}\n"))?;
            crate::session::config::profile_config::save_profile_config(profile, &profile_config)?;
            for (origin, expected) in [
                (RestartOrigin::StartupRecovery, recovered),
                (RestartOrigin::Explicit, explicit),
            ] {
                assert_eq!(
                    select_restart_wake_message(origin, None, profile),
                    expected,
                    "global={global:?}, profile={overrides:?}"
                );
            }
        }
        Ok(())
    }
}
