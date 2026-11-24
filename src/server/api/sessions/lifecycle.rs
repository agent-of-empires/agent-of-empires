//! Pin, color, archive, trash/restore, rename/summarize triggers, and
//! stop/start/snooze/unread endpoints.

use super::*;
use crate::session::SessionStore;

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

/// 409 for ignore refusing a sunk row. The codes stay the ones clients already
/// switch on; the message names ignore instead of a start.
fn ignore_blocked_response(blocked: crate::session::StartBlocked) -> axum::response::Response {
    use crate::daemon::ApiErrorCode;
    let (code, message) = match blocked {
        crate::session::StartBlocked::Archived => (
            ApiErrorCode::SessionArchived,
            "An archived session cannot be ignored; unarchive it first",
        ),
        crate::session::StartBlocked::Trashed => (
            ApiErrorCode::SessionTrashed,
            "A session in trash cannot be ignored; restore it first",
        ),
    };
    crate::server::api::api_error_with_code_header(code, message)
}

/// Ignore or unignore a session managed by another session. The row stays
/// live and present; archive, snooze, and trash are untouched. Ignoring
/// requires a manager and refuses an archived or trashed session because
/// those states outrank it.
pub async fn update_session_ignored(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<UpdateIgnoredBody>, axum::extract::rejection::JsonRejection>,
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
    let ignored = body.ignored;
    let persist_id = id.clone();
    let committed = commit_profile_update_with_refusal(
        &state,
        profile,
        "ignore update",
        move |instances| {
            let Some(instance) = instances
                .iter_mut()
                .find(|instance| instance.id == persist_id)
            else {
                return Err(crate::server::api::session_not_found());
            };
            if ignored {
                // Archive and trash outrank ignore; ignoring a sunk row would persist a
                // state the rest of the dashboard reads as archived or trashed.
                if instance.is_trashed() {
                    return Err(ignore_blocked_response(
                        crate::session::StartBlocked::Trashed,
                    ));
                }
                if instance.is_archived() {
                    return Err(ignore_blocked_response(
                        crate::session::StartBlocked::Archived,
                    ));
                }
                // Only a managed row can be ignored; otherwise the marker would outlive
                // the manager link that gives it meaning.
                if instance.parent_session_id.is_none() {
                    return Err(crate::server::api::api_error(
                        axum::http::StatusCode::BAD_REQUEST,
                        "session_not_managed",
                        "Only a session managed by another session can be ignored",
                    ));
                }
                instance.ignore();
            } else {
                instance.unignore();
            }
            Ok(())
        },
        None,
    )
    .await;
    if let Err(response) = committed {
        return response;
    }
    tracing::info!(target: "session.sync", session = %id, field = "ignored", value = ignored, "session updated");
    drop(guard);
    drop(namespace);
    crate::server::runtime::session_mutation_response(&state, &id, None::<()>).await
}

/// Take ownership of a managed session: clear its manager link and ignore
/// marker while preserving the row, conversation, worktree, and every
/// unrelated triage timestamp.
pub async fn reclaim_session_ownership(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<ReclaimOwnershipBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(rejection) => return rejection.into_response(),
    };
    if !body.reclaim {
        return api_error(
            StatusCode::BAD_REQUEST,
            "validation_failed",
            "reclaim must be true",
        );
    }

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
        "reclaim ownership",
        move |instances| {
            if let Some(instance) = instances
                .iter_mut()
                .find(|instance| instance.id == persist_id)
            {
                instance.reclaim_ownership();
            }
        },
        None,
    )
    .await;
    if let Err(response) = committed {
        return response;
    }
    tracing::info!(target: "session.sync", session = %id, field = "ownership", value = "reclaimed", "session updated");
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
                crate::session::LifecycleRelease::Reservation(LifecycleOperation::Stop),
                generation,
                None,
            );
        }
    }
}

struct TrashReservationRetirement<'a> {
    native: crate::server::session_store::NativeSessionStore,
    id: &'a str,
    generation: Option<u64>,
}

impl Drop for TrashReservationRetirement<'_> {
    fn drop(&mut self) {
        if let Some(generation) = self.generation {
            if let Err(error) = self.native.retire_trash_reservation(self.id, generation) {
                tracing::warn!(target: "session.trash", session = %self.id, %error, "unfinished trash lease retirement failed");
            }
        }
    }
}

#[cfg(test)]
mod trash_reservation_tests {
    use super::*;

    #[tokio::test]
    #[serial_test::serial]
    async fn failed_trash_claim_retires_only_its_durable_owner() {
        for failure in [
            "returned",
            "claim_adoption",
            "terminal_adoption",
            "superseded",
        ] {
            let _home = crate::session::test_support::isolate_app_dir();
            let mut row = Instance::new("trash claim boundary", "/tmp/trash-claim-boundary");
            row.source_profile = "default".into();
            let id = row.id.clone();
            let initial_generation = row.lifecycle_generation;
            crate::server::test_support::seed_instances_on_disk_for_test(
                "default",
                vec![row.clone()],
            );
            let state = crate::server::test_support::build_test_app_state(vec![row]);
            crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
            let worker_state = state.clone();
            let worker_id = id.clone();
            let peer_at = crate::time::Timestamp::now()
                + Instance::LIFECYCLE_RESERVATION_TTL
                + crate::time::Duration::seconds(1);
            let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                let native = crate::server::session_store::NativeSessionStore::open(
                    worker_state.clone(),
                    "default",
                    Some(worker_id.clone()),
                )?;
                let mut retirement = TrashReservationRetirement {
                    native,
                    id: &worker_id,
                    generation: None,
                };
                let store: &dyn crate::session::SessionStore = &retirement.native;
                let _lifecycle = store
                    .storage()
                    .acquire_instance_lifecycle_lock(&worker_id)?;
                if failure == "claim_adoption" {
                    let state = worker_state.clone();
                    crate::server::session_store::before_next_adoption_for_test(move || {
                        state.canonical_metadata.blocking_write().profiles.clear();
                    });
                }
                let generation = store.update(|rows, _| {
                    let row = rows.iter_mut().find(|row| row.id == worker_id).unwrap();
                    let generation = row.try_acquire_lifecycle_reservation(
                        LifecycleOperation::Trash,
                        Instance::LIFECYCLE_RESERVATION_TTL,
                        crate::time::Timestamp::now(),
                    )?;
                    retirement.generation = Some(generation);
                    row.trash();
                    Ok(generation)
                })?;
                if failure == "returned" {
                    anyhow::bail!("post-claim operation failed");
                }
                if failure == "terminal_adoption" {
                    let state = worker_state.clone();
                    crate::server::session_store::before_next_adoption_for_test(move || {
                        state.canonical_metadata.blocking_write().profiles.clear();
                    });
                }
                store.update(|rows, _| {
                    let row = rows.iter_mut().find(|row| row.id == worker_id).unwrap();
                    if failure == "superseded" {
                        row.try_acquire_lifecycle_reservation(
                            LifecycleOperation::Trash,
                            Instance::LIFECYCLE_RESERVATION_TTL,
                            peer_at,
                        )?;
                    } else {
                        row.release_lifecycle_reservation_if_owned(
                            LifecycleOperation::Trash,
                            generation,
                        );
                    }
                    Ok(())
                })?;
                Ok(())
            })
            .await
            .unwrap();
            assert_eq!(result.is_ok(), failure == "superseded");
            let stored = crate::session::Storage::open_unwatched("default")
                .unwrap()
                .load()
                .unwrap()
                .into_iter()
                .find(|row| row.id == id)
                .unwrap();
            assert!(stored.is_trashed());
            assert_eq!(stored.project_path, "/tmp/trash-claim-boundary");
            match failure {
                "terminal_adoption" => {
                    assert!(stored.lifecycle_reservation.is_none());
                    assert_eq!(stored.lifecycle_generation, initial_generation + 1);
                }
                "superseded" => {
                    assert_eq!(stored.lifecycle_generation, initial_generation + 2);
                    assert_eq!(stored.lifecycle_reservation.unwrap().at, peer_at);
                }
                _ => {
                    assert_eq!(stored.lifecycle_generation, initial_generation + 1);
                    assert!(stored.has_pending_trash());
                    assert!(!stored.has_fresh_lifecycle_reservation(crate::time::Timestamp::now()));
                    let mut retry = stored;
                    assert_eq!(
                        retry
                            .try_acquire_lifecycle_reservation(
                                LifecycleOperation::Trash,
                                Instance::LIFECYCLE_RESERVATION_TTL,
                                crate::time::Timestamp::now(),
                            )
                            .unwrap(),
                        initial_generation + 2
                    );
                }
            }
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
                crate::time::Timestamp::now(),
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
                        crate::time::Timestamp::now(),
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
                row.created_at += crate::time::Duration::seconds(1);
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
        let mut lifecycle = Some(store.storage().acquire_instance_lifecycle_lock(&worker_id)?);
        let mut candidate = None;
        let mut binding = None;
        let reserved = store.update(|rows, _| {
            let row = rows.iter_mut().find(|row| row.id == worker_id)
                .ok_or(LifecycleTargetError::Missing)?;
            if worker_state.cityhall_mode && !row.is_structured() {
                return Err(LifecycleTargetError::CityHall.into());
            }
            if row.has_pending_trash() {
                return Err(crate::session::LifecycleReservationError::Busy(LifecycleOperation::Trash).into());
            }
            if row.is_trashed() {
                return Err(LifecycleTargetError::Blocked(crate::session::StartBlocked::Trashed).into());
            }
            let now = crate::time::Timestamp::now();
            if matches!(row.status, Status::Creating | Status::Deleting)
                || row.has_fresh_lifecycle_reservation(now) {
                return Err(LifecycleTargetError::Busy.into());
            }
            if !body.archived {
                row.unarchive();
                return Ok(None);
            }
            if row.is_structured() {
                binding = Some(crate::server::acp_events::BindingRetirement::prepare(
                    &worker_state, row, store.storage(),
                )?);
            }
            let generation = row.try_acquire_lifecycle_reservation(
                LifecycleOperation::Stop, Instance::LIFECYCLE_RESERVATION_TTL, now,
            )?;
            row.archive();
            row.rebase_acp_generation(store.storage())?;
            if let Some(binding) = binding.as_mut() { binding.promote(row, store.storage())?; }
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
                        if let Some(binding) = &binding { binding.activate(); }
                        if let Some(binding) = &binding {
                            drop(lifecycle.take());
                            drop(_title);
                            if let Err(cleanup) = tokio::runtime::Handle::current().block_on(
                                worker_state.acp_supervisor.shutdown_captured_and_wait(
                                    &worker_id, &binding.target, std::time::Duration::from_secs(5),
                                ),
                            ) {
                                tracing::warn!(session = %worker_id, %cleanup, "applied archive retirement remains pending");
                            }
                        }
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
        if let Some(binding) = &binding { binding.activate(); }
        let mut physical_retired = false;
        let mut release = ArchiveReservationRelease {
            native: Some(native), id: worker_id.clone(), generation: Some(generation),
        };
        let native = release.native.as_ref().expect("owned archive store");
        let store: &dyn crate::session::SessionStore = native;
        instance.source_profile = profile;
        instance.file_watch = Some(worker_state.file_watch.clone());
        let stopped = if instance.is_structured() {
            drop(lifecycle.take());
            let retirement = tokio::runtime::Handle::current().block_on(
                worker_state.acp_supervisor.shutdown_captured_and_wait(
                    &worker_id, &binding.as_ref().expect("structured archive capture").target,
                    std::time::Duration::from_secs(5),
                ),
            );
            lifecycle = Some(store.storage().acquire_instance_lifecycle_lock(&worker_id)?);
            physical_retired = retirement.is_ok();
            retirement.map_err(anyhow::Error::from).and_then(|_| {
                if body.kill_pane { instance.kill_ancillary_tmux_sessions_locked() } else { Ok(()) }
            })
        } else if body.kill_pane {
            instance.kill_all_tmux_sessions_locked()
        } else { Ok(()) };
        if physical_retired {
            if let Some(binding) = &binding {
                binding.commit_under_native_lock(store)?;
            }
        }
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
            if let Err(error) = &stopped { row.last_error.set(error.to_string()); }
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
        drop(lifecycle);
        if physical_retired {
            worker_state.acp_supervisor.acknowledge_retirement(
                &binding.as_ref().expect("structured archive capture").target,
            )?;
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

/// Completion bookkeeping is not another explicit Access/wake gesture.
pub async fn touch_session_input_recency(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<crate::daemon::InputRecencyBody>,
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
    let current = {
        let rows = state.instances.read().await;
        let Some(row) = rows.iter().find(|row| row.id == id) else {
            return crate::server::api::session_not_found();
        };
        row.created_at == body.created_at
            && row.lifecycle_generation == body.lifecycle_generation
            && row.source_profile == body.profile
    };
    if current {
        let persist_id = id.clone();
        let worker_state = state.clone();
        let persisted = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            use crate::session::SessionStore;
            let native = crate::server::session_store::NativeSessionStore::open(
                worker_state,
                &body.profile,
                None,
            )?;
            if body.profile_identity.is_none()
                || body.profile_identity != Some(native.storage().bound_profile_identity()?)
            {
                return Ok(());
            }
            (&native as &dyn SessionStore).update(|rows, _| {
                let row = rows
                    .iter_mut()
                    .find(|row| {
                        row.id == persist_id
                            && row.created_at == body.created_at
                            && row.lifecycle_generation == body.lifecycle_generation
                    })
                    .ok_or(crate::session::LifecycleReservationError::Superseded)?;
                row.last_accessed_at = Some(crate::time::Timestamp::now());
                Ok(())
            })
        })
        .await;
        match persisted {
            Ok(Ok(())) => {}
            Ok(Err(error)) if error.is::<crate::session::SessionCommitApplied>() => {
                return api_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "outcome_unconfirmed",
                    "The recency update committed, but runtime publication is unconfirmed.",
                );
            }
            Ok(Err(error))
                if error.downcast_ref::<crate::session::LifecycleReservationError>()
                    == Some(&crate::session::LifecycleReservationError::Superseded) => {}
            Ok(Err(error)) if error.is::<crate::session::NativeStoreUnavailable>() => {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            Ok(Err(error)) => {
                tracing::warn!(target: "session.store", %error, "native input recency commit failed");
                return persist_failed_response();
            }
            Err(error) => {
                tracing::warn!(target: "session.store", %error, "native input recency worker failed");
                return persist_failed_response();
            }
        }
    }
    // Superseded completion is a deliberate no-op, not permission to wake the replacement.
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
        let mut retirement = TrashReservationRetirement {
            native,
            id: &worker_id,
            generation: None,
        };
        let native = &retirement.native;
        let store: &dyn crate::session::SessionStore = native;
        let title = crate::session::acquire_session_title_lock(&worker_id)?;
        let mut lifecycle = Some(store.storage().acquire_instance_lifecycle_lock(&worker_id)?);
        let mut binding = None;
        let updated = store.update(|rows, _| {
            let row = rows.iter_mut().find(|row| row.id == worker_id)
                .ok_or(LifecycleTargetError::Missing)?;
            if worker_state.cityhall_mode && !row.is_structured() {
                return Err(LifecycleTargetError::CityHall.into());
            }
            if body.expected_profile.as_deref().is_some_and(|profile| profile != store.storage().profile()) {
                return Err(crate::session::LifecycleReservationError::Superseded.into());
            }
            if body.expected_generation.is_some_and(|generation| generation != row.lifecycle_generation) {
                return Err(crate::session::LifecycleReservationError::Superseded.into());
            }
            let retrying = row.is_trashed();
            if retrying {
                if row.has_fresh_lifecycle_reservation(crate::time::Timestamp::now()) {
                    return Err(LifecycleTargetError::Busy.into());
                }
                if !row.lifecycle_reservation_is_owned(LifecycleOperation::Trash, row.lifecycle_generation) {
                    return Ok(None);
                }
            }
            if row.is_structured() {
                binding = Some(crate::server::acp_events::BindingRetirement::prepare(
                    &worker_state, row, store.storage(),
                )?);
            }
            row.try_acquire_lifecycle_reservation(
                LifecycleOperation::Trash, Instance::LIFECYCLE_RESERVATION_TTL, crate::time::Timestamp::now(),
            )?;
            retirement.generation = Some(row.lifecycle_generation);
            row.trash();
            row.rebase_acp_generation(store.storage())?;
            if let Some(binding) = binding.as_mut() { binding.promote(row, store.storage())?; }
            Ok(Some((row.clone(), retrying)))
        });
        let instance = match updated {
            Ok(instance) => instance,
            Err(error) => {
                if error.is::<crate::session::SessionCommitApplied>() {
                    if let Some(binding) = &binding { binding.activate(); }
                    if let Some(binding) = &binding {
                        drop(lifecycle.take());
                        drop(title);
                        if let Err(cleanup) = tokio::runtime::Handle::current().block_on(
                            worker_state.acp_supervisor.shutdown_captured_and_wait(
                                &worker_id, &binding.target, std::time::Duration::from_secs(5),
                            ),
                        ) {
                            tracing::warn!(session = %worker_id, %cleanup, "applied trash retirement remains pending");
                        }
                    }
                }
                return Err(error);
            }
        };
        let Some((mut instance, retrying)) = instance else {
            return Ok((crate::session::trash::RelocateOutcome::Skipped, None));
        };
        if let Some(binding) = &binding { binding.activate(); }
        let mut physical_retired = false;
        instance.source_profile = profile;
        instance.file_watch = Some(worker_state.file_watch.clone());
        let generation = instance.lifecycle_generation;
        let stopped = if instance.is_structured() {
            drop(lifecycle.take());
            let retirement = tokio::runtime::Handle::current().block_on(
                worker_state.acp_supervisor.shutdown_captured_and_wait(
                    &worker_id, &binding.as_ref().expect("structured trash capture").target,
                    std::time::Duration::from_secs(5),
                ),
            );
            lifecycle = Some(store.storage().acquire_instance_lifecycle_lock(&worker_id)?);
            physical_retired = retirement.is_ok();
            retirement.map_err(anyhow::Error::from).and_then(|_| {
                if body.kill_pane { instance.kill_ancillary_tmux_sessions_locked() } else { Ok(()) }
            })
        } else if body.kill_pane {
            instance.kill_all_tmux_sessions_locked()
        } else { Ok(()) };
        if instance.is_structured() || body.kill_pane {
            native.refresh_pane_observations(&instance)?;
        }
        drop(lifecycle);
        drop(title);
        let _identity = crate::session::acquire_session_identity_lock()?;
        let _lifecycle = store.storage().acquire_instance_lifecycle_lock(&worker_id)?;
        if physical_retired {
            if let Some(binding) = &binding {
                binding.commit_under_native_lock(store)?;
            }
        }
        instance = store.update(|rows, _| {
            let row = rows.iter_mut().find(|row| row.id == worker_id)
                .ok_or(LifecycleTargetError::Missing)?;
            if !row.is_trashed() || !row.lifecycle_reservation_is_owned(LifecycleOperation::Trash, generation) {
                return Err(LifecycleTargetError::Busy.into());
            }
            Ok(row.clone())
        })?;
        instance.source_profile = store.storage().profile().to_owned();
        instance.file_watch = Some(worker_state.file_watch.clone());
        let outcome = match &stopped {
            Ok(()) => {
                let outcome = crate::session::trash::prepare_trashed_worktree(&mut instance);
                if retrying && !matches!(outcome, crate::session::trash::RelocateOutcome::Relocated { .. }) {
                    crate::session::trash::recover_trashed_worktree_move(&mut instance).unwrap_or(outcome)
                } else {
                    outcome
                }
            }
            Err(error) => {
                let recovered = retrying.then(|| crate::session::trash::recover_trashed_worktree_move(&mut instance)).flatten();
                recovered.unwrap_or_else(|| crate::session::trash::RelocateOutcome::Failed {
                    reason: format!("Resource teardown failed; worktree retained: {error}"),
                })
            }
        };
        let terminal = !retrying
            || matches!(outcome, crate::session::trash::RelocateOutcome::Relocated { .. })
            || crate::session::trash::trashed_worktree_location_is_verified(&instance)
            || crate::session::trash::trashed_worktree_paths_are_missing(&instance);
        let outcome = if retrying && !terminal && matches!(outcome, crate::session::trash::RelocateOutcome::Skipped) {
            crate::session::trash::RelocateOutcome::Failed {
                reason: "Retained worktree location is unproven; Trash transition remains unfinished".into(),
            }
        } else {
            outcome
        };
        let relocation = matches!(outcome, crate::session::trash::RelocateOutcome::Relocated { .. })
            .then(|| crate::session::trash::TrashRelocation {
                new_project_path: instance.project_path.clone(),
                pre_trash_project_path: instance.pre_trash_project_path.clone(),
            });
        let mut superseded = false;
        let committed = store.update(|rows, _| {
            if let Some(relocation) = &relocation {
                match crate::session::claim::commit_trash_relocation(rows, &worker_id, generation, relocation) {
                    crate::session::claim::RelocationCommit::Persisted => {}
                    crate::session::claim::RelocationCommit::AlreadyGone => return Err(LifecycleTargetError::Missing.into()),
                    crate::session::claim::RelocationCommit::Superseded => {
                        superseded = true;
                        return Ok(());
                    }
                }
            } else {
                let row = rows.iter_mut().find(|row| row.id == worker_id)
                    .ok_or(LifecycleTargetError::Missing)?;
                if !row.lifecycle_reservation_is_owned(LifecycleOperation::Trash, generation) {
                    return Err(LifecycleTargetError::Busy.into());
                }
                if terminal {
                    row.release_lifecycle_reservation_if_owned(LifecycleOperation::Trash, generation);
                } else {
                    row.retire_trash_reservation_if_owned(generation, crate::time::Timestamp::now());
                }
            }
            if let Err(error) = &stopped {
                let row = rows.iter_mut().find(|row| row.id == worker_id)
                    .ok_or(LifecycleTargetError::Missing)?;
                row.status = Status::Error;
                row.last_error.set(error.to_string());
            }
            Ok(())
        });
        if let Some(relocation) = relocation.filter(|_| superseded) {
            if let crate::session::trash::RestoreOutcome::Failed { reason } =
                crate::session::trash::undo_raced_relocation(&instance, &relocation)
            {
                tracing::warn!(target: "session.trash", session = %worker_id, %reason, "could not move superseded trash relocation back");
            }
            committed?;
            return Err(LifecycleTargetError::Busy.into());
        }
        committed?;
        retirement.generation = None;
        drop(_lifecycle);
        if physical_retired {
            worker_state.acp_supervisor.acknowledge_retirement(
                &binding.as_ref().expect("structured trash capture").target,
            )?;
        }
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
                let now = crate::time::Timestamp::now();
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
/// A terminal session's rename runs detached, so it answers `202` with the row.
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

    let Some((profile, tool, command, config_path, sandboxed, title, structured)) = ({
        let instances = state.instances.read().await;
        instances.iter().find(|i| i.id == id).map(|i| {
            (
                i.source_profile.clone(),
                i.tool.clone(),
                i.command.clone(),
                i.repo_config_path().to_path_buf(),
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
        &config_path,
    );
    let config = &resolved.session;
    if let Err(reason) = crate::session::smart_rename::check_eligible_with_declarations(
        true,
        true,
        true,
        &title,
        &tool,
        &config.smart_rename_agent,
        sandboxed,
        &command,
        config,
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
        return crate::server::runtime::accepted_session_mutation_response(&state, &id).await;
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
        &config.session,
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
    /// A requested account swap that would not keep the conversation.
    #[error("{0}")]
    AccountSwapRefused(String),
    /// An archived or trashed session refuses to launch or resume (#4116).
    #[error(transparent)]
    Blocked(#[from] crate::session::StartBlocked),
}

pub(crate) fn lifecycle_rejection(
    state: &AppState,
    error: &anyhow::Error,
) -> Option<axum::response::Response> {
    if let Some(cause) = error.downcast_ref::<crate::session::LaunchIndeterminateCause>() {
        return Some(super::ensure::indeterminate_launch_response(*cause));
    }
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
        Some(LifecycleTargetError::AccountSwapRefused(message)) => {
            return Some(crate::server::api::api_error_with_code_header(
                crate::daemon::ApiErrorCode::AccountSwapUnavailable,
                message.clone(),
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
    if let Some(login) = error.downcast_ref::<crate::session::SandboxLoginRequired>() {
        return Some(crate::server::api::sandbox_login_required_response(login));
    }
    if let Some(unsupported) = error.downcast_ref::<crate::session::SandboxLoginUnsupported>() {
        return Some(crate::server::api::sandbox_login_unsupported_response(
            unsupported,
        ));
    }
    if let Some(hook) = error.downcast_ref::<crate::session::BeforeStartHookError>() {
        return Some(crate::server::api::before_start_hook_failed_response(hook));
    }
    // A row a peer purged between the memory check and this write: the caller
    // is talking to a session that no longer exists, not to a busy one.
    if error.is::<crate::session::SessionGone>() {
        return Some(crate::server::api::session_not_found());
    }
    let (code, operation, message) = match error
        .downcast_ref::<crate::session::LifecycleReservationError>()
    {
        Some(crate::session::LifecycleReservationError::Busy(LifecycleOperation::Trash)) => (
            "trash_incomplete",
            Some(LifecycleOperation::Trash),
            "Trash is in progress or unfinished; if interrupted, retry Trash before another lifecycle action".to_owned(),
        ),
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
    let header = match code {
        "lifecycle_superseded" => crate::daemon::ApiErrorCode::LifecycleSuperseded.header(),
        "trash_incomplete" => crate::daemon::ApiErrorCode::TrashIncomplete.header(),
        _ => crate::daemon::ApiErrorCode::LifecycleLocked.header(),
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
    if let Some(refusal) = error.downcast_ref::<crate::acp::establishment::EstablishmentRefusal>() {
        return api_error(
            StatusCode::CONFLICT,
            "conversation_establishment_refused",
            refusal.to_string(),
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
        crate::time::Timestamp::now(),
    )?;
    // Even a row already presented as Stopped can own a live or pending worker.
    row.status = Status::Stopped;
    row.mark_idle_dormant();
    Ok(generation)
}

struct StructuredStopCommit {
    native: crate::server::session_store::NativeSessionStore,
    generation: u64,
    binding: crate::server::acp_events::BindingRetirement,
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
    // The original producer can have persisted its binding before the async
    // listener obtains instance exclusion. Preserve that qualified observation
    // while we still own its native incarnation, before Stop supersedes it.
    let through = state.acp_event_store.highest_seq(id)?;
    let observed = state
        .acp_event_store
        .handoff_context_event(id, 0, through)?;
    let mut generation = None;
    let mut binding = None;
    let publication = match (&native as &dyn SessionStore).update(|rows, _| {
        let row = rows
            .iter_mut()
            .find(|row| row.id == id)
            .ok_or(crate::session::SessionGone)?;
        let mut prepared = crate::server::acp_events::BindingRetirement::prepare_captured(
            &state,
            target.clone(),
            row,
            native.storage(),
        )?;
        if let Some((_, event @ crate::acp::Event::AcpSessionAssigned { .. })) = &observed {
            let context = crate::acp::state::NativeEventContext::capture(row, native.storage())?;
            let captured_scope = target
                .lease
                .as_ref()
                .and_then(|lease| lease.native_scope().ok().flatten());
            if event.native_subject().is_some_and(|subject| {
                subject.context == context
                    && subject.owner == target.identity
                    && captured_scope
                        .as_ref()
                        .is_some_and(|scope| scope.context == context)
                    && matches!(
                        subject.kind,
                        crate::acp::state::NativeSubjectEvent::BindingObserved
                            | crate::acp::state::NativeSubjectEvent::ChannelReady
                    )
            }) {
                let change = crate::server::acp_events::derive_acp_session_change(event);
                crate::server::acp_events::apply_acp_session_change(
                    row,
                    id,
                    change.as_ref(),
                    false,
                    false,
                );
            }
        }
        generation = Some(reserve_structured_user_stop(row)?);
        row.rebase_acp_generation(native.storage())?;
        prepared.promote(row, native.storage())?;
        binding = Some(prepared);
        Ok(())
    }) {
        Ok(()) => Ok(()),
        Err(error) if error.is::<crate::session::SessionCommitApplied>() => Err(error),
        Err(error) => return Err(error),
    };
    let generation = generation.ok_or_else(|| anyhow::anyhow!("stop commit has no generation"))?;
    let binding = binding.ok_or_else(|| anyhow::anyhow!("stop binding checkpoint missing"))?;
    binding.activate();
    // Both flocks drop here, before a late constructor's admission or cleanup.
    Ok(StructuredStopCommit {
        native,
        generation,
        binding,
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
        binding,
        publication,
    } = committed;
    let target = binding.target.clone();
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
            let checkpoint = native
                .storage()
                .verify_bound_profile()
                .and_then(|()| binding.commit_under_native_lock(&native));
            // Hook cleanup and metadata publication must not prevent the exact,
            // health-independent release of an already physically retired Stop.
            let cleanup = native
                .storage()
                .verify_bound_profile()
                .and_then(|()| native.storage().load())
                .map(|rows| {
                    if let Some(row) = rows
                        .iter()
                        .find(|row| row.id == release_id && row.lifecycle_generation == generation)
                    {
                        crate::hooks::cleanup_hook_status_dir(&release_id);
                        if let Err(error) =
                            crate::session::worktree_edit::stop_pinned_sandbox_container(row)
                        {
                            tracing::warn!(target: "containers.runtime", session = %release_id, %error, "sandbox container stop after structured stop failed");
                        }
                    }
                });
            let released = native.release_lifecycle_reservation(
                &release_id,
                crate::session::LifecycleRelease::Reservation(LifecycleOperation::Stop),
                generation,
                Some(lifecycle),
            );
            checkpoint?;
            cleanup?;
            released
        })
        .await
        .map_err(anyhow::Error::from)
        .and_then(|result| result)
    } else {
        Ok(())
    };
    if retirement.is_ok() && released.is_ok() && publication.is_ok() {
        state
            .acp_supervisor
            .acknowledge_retirement(&target)
            .map_err(|error| structured_lifecycle_error(state, error.into()))?;
    }
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
    kind: Option<crate::acp::supervisor::ResumeKind>,
) -> Result<(Instance, Option<crate::acp::supervisor::ResumeReservation>), axum::response::Response>
{
    resume_structured_stop_with_restart(state, expected, kind, None, None).await
}

pub(crate) async fn resume_structured_stop_with_restart(
    state: &Arc<AppState>,
    expected: Instance,
    kind: Option<crate::acp::supervisor::ResumeKind>,
    restart: Option<&crate::acp::supervisor::RestartRequestGuard>,
    establishment: Option<Arc<crate::acp::establishment::EstablishmentAttempt>>,
) -> Result<(Instance, Option<crate::acp::supervisor::ResumeReservation>), axum::response::Response>
{
    resume_structured_stop_inner(state, expected, kind, restart, establishment, true).await
}

/// Retire the exact source before a route-changing launch selects its destination.
pub(crate) async fn resume_structured_stop_deferred(
    state: &Arc<AppState>,
    expected: Instance,
    kind: Option<crate::acp::supervisor::ResumeKind>,
) -> Result<(Instance, Option<crate::acp::supervisor::ResumeReservation>), axum::response::Response>
{
    resume_structured_stop_inner(state, expected, kind, None, None, false).await
}

async fn resume_structured_stop_inner(
    state: &Arc<AppState>,
    expected: Instance,
    kind: Option<crate::acp::supervisor::ResumeKind>,
    restart: Option<&crate::acp::supervisor::RestartRequestGuard>,
    establishment: Option<Arc<crate::acp::establishment::EstablishmentAttempt>>,
    arm_selection: bool,
) -> Result<(Instance, Option<crate::acp::supervisor::ResumeReservation>), axum::response::Response>
{
    use crate::process::worker_registry::{self, ResumeAuthority};
    use crate::session::SessionStore;
    if establishment.is_none()
        && matches!(
            expected.require_resolved_acp_selection(),
            Err(crate::acp::establishment::EstablishmentRefusal::Unresolved)
        )
    {
        return Err(structured_lifecycle_error(
            state,
            crate::acp::establishment::EstablishmentRefusal::Unresolved.into(),
        ));
    }
    if expected.status != Status::Stopped && !expected.is_idle_dormant() {
        return Ok((expected, None));
    }
    let probe_state = state.clone();
    let probe_profile = expected.source_profile.clone();
    let probe_id = expected.id.clone();
    let expected_generation = expected.lifecycle_generation;
    let expected_created_at = expected.created_at;
    let expected_choice = crate::acp::state::NativeBindingState::capture(&expected);
    let expected_authority = expected.acp_establishment.clone();
    let probe_establishment = establishment.clone();
    let (native, observed_stop, stopped_record, context, expected, observed_choice, observed_authority) =
        tokio::task::spawn_blocking(move || {
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
                || (row.has_fresh_lifecycle_reservation(crate::time::Timestamp::now())
                    && !row.lifecycle_reservation_is_owned(
                        LifecycleOperation::Stop,
                        expected_generation,
                    ))
            {
                return Err(LifecycleTargetError::Busy.into());
            }
            anyhow::ensure!(
                row.lifecycle_generation == expected_generation
                    && row.created_at == expected_created_at,
                crate::session::LifecycleReservationError::Superseded
            );
            anyhow::ensure!(
                expected_choice.matches(&row) && expected_authority == row.acp_establishment,
                crate::acp::establishment::EstablishmentRefusal::Superseded
            );
            let registry = worker_registry::try_registry_lock(&probe_id)?
                .ok_or(worker_registry::RegistryBusy)?;
            let observed_stop = registry.stop()?;
            let stopped_record = registry.record()?;
            let context = crate::acp::state::NativeEventContext::capture(&row, native.storage())?;
            drop(registry);
            if let Some(original) = &probe_establishment {
                anyhow::ensure!(
                    matches!(&row.acp_establishment,
                        Some(crate::acp::establishment::AcpEstablishment::Pending { attempt })
                            if attempt.same_source(original) && attempt.matches_pending(&row, native.storage())?),
                    crate::acp::establishment::EstablishmentRefusal::Superseded
                );
            }
            Ok::<_, anyhow::Error>((
                native,
                observed_stop,
                stopped_record,
                context,
                row,
                expected_choice,
                expected_authority,
            ))
        })
        .await
        .map_err(anyhow::Error::from)
        .and_then(|result| result)
        .map_err(|error| structured_lifecycle_error(state, error))?;
    let target = state
        .acp_supervisor
        .capture_retirement(&expected.id)
        .map_err(|error| structured_lifecycle_error(state, error.into()))?;
    let binding = crate::server::acp_events::BindingRetirement::prepare_captured(
        state,
        target,
        &expected,
        native.storage(),
    )
    .map_err(|error| structured_lifecycle_error(state, error))?;
    let target = binding.target.clone();
    binding.activate();
    let retirement = match (observed_stop.as_ref(), stopped_record.as_ref()) {
        (Some(intent), Some(record)) => {
            state
                .acp_supervisor
                .shutdown_retained_stop_and_wait(
                    &expected.id,
                    &target,
                    Some((intent, record)),
                    std::time::Duration::from_secs(5),
                )
                .await
        }
        (Some(_), None) if target.identity.is_some() => Err(
            crate::acp::supervisor::SupervisorError::TeardownPending(expected.id.clone()),
        ),
        _ => {
            state
                .acp_supervisor
                .retire_captured_for_resume_and_wait(
                    &expected.id,
                    &target,
                    std::time::Duration::from_secs(5),
                )
                .await
        }
    };
    retirement.map_err(|error| {
        tracing::warn!(target: "http.api.sessions", session = %expected.id, %error, "resume requires original retirement");
        api_error(StatusCode::CONFLICT, "teardown_pending", "The original structured worker is still stopping")
    })?;
    let resume_id = expected.id.clone();
    let (native, expected) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        crate::server::acp_events::commit_retired_binding(&native, &resume_id, &binding)?;
        let refreshed = native
            .load()?
            .into_iter()
            .find(|row| row.id == resume_id)
            .ok_or(crate::session::SessionGone)?;
        Ok((native, refreshed))
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result)
    .map_err(|error| structured_lifecycle_error(state, error))?;
    state
        .acp_supervisor
        .acknowledge_retirement(&target)
        .map_err(|error| structured_lifecycle_error(state, error.into()))?;
    let kind = kind.unwrap_or(if expected.acp_session_id.is_some() {
        crate::acp::supervisor::ResumeKind::Spawn
    } else {
        crate::acp::supervisor::ResumeKind::SpawnFirst
    });
    let mut reservation = state
        .acp_supervisor
        .reserve_spawn_with_restart(&expected.id, kind, ResumeAuthority::Explicit, restart)
        .await
        .map_err(|error| {
            if restart.is_some()
                && matches!(
                    error,
                    crate::acp::supervisor::SupervisorError::PurgeInProgress(_)
                )
            {
                crate::server::api::acp::restart_purge_conflict()
            } else {
                crate::server::api::acp::supervisor_error_response("resume failed", &error)
            }
        })?;
    if reservation.observed_stop() != Some(&observed_stop) {
        return Err(structured_lifecycle_error(
            state,
            crate::session::LifecycleReservationError::Superseded.into(),
        ));
    }
    let supervisor = state.acp_supervisor.clone();
    tokio::task::spawn_blocking(move || {
        let _title = crate::session::acquire_session_title_lock(&expected.id)?;
        let _lifecycle = native
            .storage()
            .acquire_instance_lifecycle_lock(&expected.id)?;
        let registry = worker_registry::try_registry_lock(&expected.id)?
            .ok_or(worker_registry::RegistryBusy)?;
        registry.verify_resume(ResumeAuthority::Explicit, &observed_stop)?;
        let resumed = (&native as &dyn SessionStore).update(|rows, _| {
            let row = rows
                .iter_mut()
                .find(|row| row.id == expected.id)
                .ok_or(crate::session::SessionGone)?;
            row.ensure_startable()?;
            anyhow::ensure!(row.is_structured(), LifecycleTargetError::NotStructured);
            if row.terminal_handoff.is_some()
                || matches!(row.status, Status::Creating | Status::Deleting)
                || (row.has_fresh_lifecycle_reservation(crate::time::Timestamp::now())
                    && !row.lifecycle_reservation_is_owned(
                        LifecycleOperation::Stop,
                        expected_generation,
                    ))
            {
                return Err(LifecycleTargetError::Busy.into());
            }
            anyhow::ensure!(
                row.lifecycle_generation == expected_generation
                    && row.created_at == expected_created_at
                    && crate::acp::state::NativeEventContext::capture(row, native.storage())?
                        == context
                    && (row.status == Status::Stopped || row.is_idle_dormant()),
                crate::session::LifecycleReservationError::Superseded
            );
            anyhow::ensure!(
                observed_choice.matches(row) && observed_authority == row.acp_establishment,
                crate::acp::establishment::EstablishmentRefusal::Superseded
            );
            supervisor.validate_reserved_resume(&reservation)?;
            if let Some(original) = &establishment {
                anyhow::ensure!(
                    matches!(&row.acp_establishment,
                        Some(crate::acp::establishment::AcpEstablishment::Pending { attempt })
                            if attempt.same_source(original) && attempt.matches_pending(row, native.storage())?),
                    crate::acp::establishment::EstablishmentRefusal::Superseded
                );
            } else if arm_selection {
                row.arm_acp_establishment(native.storage(), None)?;
            }
            row.resume_structured_after_retirement(expected_generation)?;
            row.rebase_acp_generation(native.storage())?;
            Ok(row.clone())
        })?;
        registry
            .admit_resume(ResumeAuthority::Explicit, &observed_stop)
            .map_err(|error| error.context(crate::session::SessionCommitApplied))?;
        reservation.stop_committed();
        reservation.expected_lifecycle_generation = Some(resumed.lifecycle_generation);
        reservation.establishment = resumed
            .acp_establishment
            .as_ref()
            .and_then(crate::acp::establishment::AcpEstablishment::attempt)
            .cloned()
            .map(Arc::new);
        Ok((resumed, Some(reservation)))
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
                crate::time::Timestamp::now(),
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
                row.last_error.set("Failed to stop session resources");
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
        let response = session_response_from_instance(
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

/// `GET /api/sessions/{id}/account-targets`: the configured tool names this session can swap to
/// without losing its conversation. Names only; `agent_config_dir` paths never leave the host.
pub async fn session_account_targets(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> axum::response::Response {
    if let Some(response) = crate::server::api::read_only_block(&state)
        .or_else(|| crate::server::api::cityhall_block(&state))
    {
        return response;
    }
    let Some(row) = crate::server::api::find_instance(&state, &id).await else {
        return crate::server::api::session_not_found();
    };
    match tokio::task::spawn_blocking(move || account_targets(&row)).await {
        Ok(targets) => Json(serde_json::json!({ "targets": targets })).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

pub(super) fn account_targets(row: &Instance) -> Vec<String> {
    if !row.is_structured() {
        return Vec::new();
    }
    let profile = row.effective_profile();
    let config = crate::session::config::profile_config::resolve_config_or_warn(&profile);
    let mut names: Vec<&str> = crate::agents::agent_names();
    names.extend(config.session.custom_agents.keys().map(String::as_str));
    names.extend(
        config
            .session
            .agent_config_dir
            .iter()
            .filter(|(_, dir)| crate::session::config::is_resolvable_agent_config_dir(dir))
            .map(|(name, _)| name.as_str()),
    );
    names.sort_unstable();
    names.dedup();
    names
        .into_iter()
        .filter(|name| *name != row.tool)
        .filter(|name| {
            let body = crate::daemon::RestartSessionBody {
                tool: Some((*name).to_owned()),
                require_account_carry: true,
                ..Default::default()
            };
            account_swap_refusal(row, &body).is_none()
        })
        .map(str::to_owned)
        .collect()
}

/// Why a restart that demands an account swap (`require_account_carry`) is malformed: the
/// tool to switch to, or what makes the swap impossible whatever the configuration says.
fn account_swap_precondition<'a>(
    row: &Instance,
    body: &'a crate::daemon::RestartSessionBody,
) -> Result<Option<&'a str>, String> {
    if !body.require_account_carry {
        return Ok(None);
    }
    let Some(tool) = body.tool.as_deref() else {
        return Err("An account swap needs the tool to switch to".to_owned());
    };
    if tool == row.tool {
        return Err(format!("Session already runs as '{tool}'"));
    }
    if body.command_override.is_some() {
        return Err("An account swap cannot also change the launch command".to_owned());
    }
    Ok(Some(tool))
}

/// A plain tool swap would park the ids and start a fresh conversation instead.
fn park_refusal(tool: &str) -> String {
    format!(
        "Cannot switch to '{tool}' without losing the conversation. Both tools must run the \
         same agent with a known transcript layout (Claude), the session needs a recorded \
         conversation, and a sandboxed session must not have a store move pending. Nothing changed."
    )
}

/// Why a restart that demands an account swap cannot keep the conversation, decided on `row`
/// alone so it can run before anything mutates.
pub(super) fn account_swap_refusal(
    row: &Instance,
    body: &crate::daemon::RestartSessionBody,
) -> Option<String> {
    let tool = match account_swap_precondition(row, body) {
        Ok(Some(tool)) => tool,
        Ok(None) => return None,
        Err(message) => return Some(message),
    };
    let target_profile = body.profile.as_deref().unwrap_or(&row.source_profile);
    match crate::session::conversation_carry::classify(row, target_profile, tool) {
        // The launch later attests its destination; one whose retained command or arguments
        // name another agent fails there, after the worker stopped, so attest the swapped row now.
        crate::session::conversation_carry::ToolSwap::KeepConversation(_) => {
            let mut swapped = row.clone();
            swapped.source_profile = target_profile.to_owned();
            swapped.swap_account(tool);
            if let Some(extra_args) = &body.extra_args {
                swapped.extra_args.clone_from(extra_args);
            }
            swapped.attested_carry_destination().err().map(|error| {
                format!(
                    "Cannot switch to '{tool}': this session's launch cannot carry the \
                     conversation ({error:#}). Nothing changed."
                )
            })
        }
        crate::session::conversation_carry::ToolSwap::Park => Some(park_refusal(tool)),
    }
}

fn admit_restart(
    row: &mut Instance,
    body: &crate::daemon::RestartSessionBody,
    account_swap: bool,
) -> anyhow::Result<()> {
    let now = crate::time::Timestamp::now();
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
    row.last_error.clear();
    row.last_error_evidence = None;
    row.last_error_check = None;
    row.status = Status::Starting;
    Ok(())
}

/// Whether a restart into `tool` (under `target`, when it moves profile) swaps the account of
/// the same agent and so keeps the row's conversation and provider, as the restart itself
/// decides.
fn keeps_account(row: &Instance, target: Option<&str>, tool: Option<&str>) -> bool {
    let Some(tool) = tool.filter(|tool| *tool != row.tool) else {
        return false;
    };
    matches!(
        crate::session::conversation_carry::classify(
            row,
            target.unwrap_or(&row.source_profile),
            tool
        ),
        crate::session::conversation_carry::ToolSwap::KeepConversation(_)
    )
}

/// The restart's profile, tool and command over `row`, as the restart itself applies them:
/// an account of the same agent keeps the conversation (`swap_account`), any other tool
/// parks it (`swap_tool`). The launch the pre-lock admission decides on.
pub(super) fn apply_restart_settings(
    row: &mut Instance,
    target: Option<&str>,
    tool: Option<&str>,
    command: Option<&str>,
) {
    let account_swap = keeps_account(row, target, tool);
    if let Some(target) = target {
        row.source_profile = target.to_owned();
    }
    if let Some(tool) = tool.filter(|tool| *tool != row.tool) {
        if account_swap {
            row.swap_account(tool);
        } else {
            row.swap_tool(tool);
        }
    }
    if let Some(command) = command {
        row.command = command.to_owned();
    }
}

/// The sandbox login admission of a restart, before it takes any lock or stops or discards
/// anything: a refusal leaves the running agent and its container as they are. It resolves a
/// `before_start` hook the cheap decision cannot see, here and not under the locks (a hook may
/// call `aoe` on this session), and the strict re-checks under the locks then find the result.
/// `None` admits the restart, or leaves a refusal it cannot decide (a store that will not
/// open) to the restart's own steps.
async fn admit_restart_sandbox_login(
    state: &Arc<AppState>,
    id: &str,
    body: &crate::daemon::RestartSessionBody,
) -> Option<axum::response::Response> {
    let mut row = state
        .instances
        .read()
        .await
        .iter()
        .find(|row| row.id == id && row.is_sandboxed())
        .cloned()?;
    // A restart a cheaper check refuses never runs the hook: those checks answer first.
    if row.ensure_startable().is_err()
        || matches!(row.status, Status::Creating | Status::Deleting)
        || (body.only_if_idle && row.status != Status::Idle)
        || (state.cityhall_mode && !row.is_structured())
        || *state.canonical_health.read().await != crate::daemon::RuntimeHealth::Healthy
    {
        return None;
    }
    let target = body
        .profile
        .as_deref()
        .filter(|target| *target != row.source_profile)
        .map(str::to_owned);
    if let Some(target) = &target {
        if !state
            .canonical_metadata
            .read()
            .await
            .profiles
            .iter()
            .any(|profile| profile.name == *target)
        {
            return None;
        }
    }
    let (tool, command) = (body.tool.clone(), body.command_override.clone());
    let native_state = state.clone();
    let checked = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        use crate::session::SessionStore;
        let id = row.id.clone();
        let open = |profile: &str| {
            let store = crate::server::session_store::NativeSessionStore::open(
                native_state.clone(),
                profile,
                Some(id.clone()),
            )?;
            store.configuration(Some(profile))?;
            anyhow::Ok(store)
        };
        // The persisted row first, then the restart's settings over it: a reload after them
        // would undo them, and the hook and the gate decide on the launch the restart makes.
        let source = open(&row.source_profile)?;
        if let Err(error) = row.reconcile_from_store(&source) {
            return match error.downcast_ref::<crate::session::LifecycleReservationError>() {
                // The row is gone: the restart itself answers.
                Some(crate::session::LifecycleReservationError::Superseded) => Ok(()),
                _ => Err(error),
            };
        }
        apply_restart_settings(
            &mut row,
            target.as_deref(),
            tool.as_deref(),
            command.as_deref(),
        );
        let launch_store = match &target {
            Some(target) => open(target)?,
            None => source,
        };
        row.resolve_before_start_for_restart(&launch_store, false)?;
        row.ensure_sandbox_launchable_strict(&launch_store)
    })
    .await;
    match checked {
        Ok(Ok(())) => None,
        Ok(Err(error)) => lifecycle_rejection(state, &error),
        Err(_) => Some(StatusCode::INTERNAL_SERVER_ERROR.into_response()),
    }
}

fn reserve_structured_restart(
    state: Arc<AppState>,
    instance: &Instance,
    expected_generation: Option<u64>,
    body: &crate::daemon::RestartSessionBody,
    restart_origin: RestartOrigin,
) -> anyhow::Result<(
    crate::server::session_store::NativeSessionStore,
    Instance,
    crate::server::acp_events::BindingRetirement,
)> {
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
    // Decided on the durable row, before the reservation and before the worker stops.
    if let Some(message) = account_swap_refusal(&probe, body) {
        return Err(LifecycleTargetError::AccountSwapRefused(message).into());
    }
    if matches!(restart_origin, RestartOrigin::StartupRecovery) {
        // Preserve archived/trashed refusals.
        probe
            .ensure_startable()
            .map_err(LifecycleTargetError::Blocked)?;
        return Err(LifecycleTargetError::Busy.into());
    }
    let account_swap = keeps_account(
        &probe,
        body.profile
            .as_deref()
            .filter(|target| *target != instance.source_profile),
        body.tool.as_deref(),
    );
    admit_restart(&mut probe, body, account_swap)?;
    if probe.is_sandboxed() {
        // Strictly, under the locks, before the caller stops the worker; the target
        // profile's store when the restart moves the session.
        let target_native = body
            .profile
            .as_deref()
            .filter(|target| *target != instance.source_profile)
            .map(|target| {
                crate::server::session_store::NativeSessionStore::open(
                    state.clone(),
                    target,
                    Some(instance.id.clone()),
                )
            })
            .transpose()?;
        let mut gated = probe.clone();
        if let Some(target) = &target_native {
            gated.source_profile = target.storage().profile().to_owned();
        }
        gated.ensure_sandbox_launchable_strict(
            target_native
                .as_ref()
                .map_or(&native as &dyn SessionStore, |target| target),
        )?;
    }
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
        (&target_native as &dyn SessionStore).launch_configuration(instance.repo_config_path())?;
        let rows = target_native.load()?;
        if rows.iter().any(|row| row.id == instance.id)
            || is_duplicate_session(rows.iter(), &probe.title, &probe.project_path, None)
        {
            return Err(duplicate_session_error(&probe.title));
        }
    } else {
        (&native as &dyn SessionStore).launch_configuration(instance.repo_config_path())?;
    }
    let mut binding = None;
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
            || row.has_fresh_lifecycle_reservation(crate::time::Timestamp::now())
        {
            return Err(LifecycleTargetError::Busy.into());
        }
        let mut prepared =
            crate::server::acp_events::BindingRetirement::prepare(&state, row, native.storage())?;
        row.try_acquire_lifecycle_reservation(
            LifecycleOperation::Launch,
            Instance::LIFECYCLE_RESERVATION_TTL,
            crate::time::Timestamp::now(),
        )?;
        row.status = Status::Starting;
        row.rebase_acp_generation(native.storage())?;
        prepared.promote(row, native.storage())?;
        binding = Some(prepared);
        Ok(row.clone())
    });
    if reserved.is_ok()
        || reserved
            .as_ref()
            .is_err_and(|error| error.is::<crate::session::SessionCommitApplied>())
    {
        if let Some(binding) = &binding {
            binding.activate();
        }
    }
    let reserved = match reserved {
        Ok(reserved) => reserved,
        Err(error) => {
            if error.is::<crate::session::SessionCommitApplied>() {
                drop(_lifecycle);
                drop(_title);
                drop(_identity);
                if let Some(binding) = &binding {
                    if let Err(cleanup) = tokio::runtime::Handle::current().block_on(
                        state.acp_supervisor.shutdown_captured_and_wait(
                            &instance.id,
                            &binding.target,
                            std::time::Duration::from_secs(5),
                        ),
                    ) {
                        tracing::warn!(session = %instance.id, %cleanup, "applied restart retirement remains pending");
                    }
                }
            }
            return Err(error);
        }
    };
    let binding = binding.ok_or_else(|| anyhow::anyhow!("restart binding checkpoint missing"))?;
    Ok((native, reserved, binding))
}

struct PreparedQuotaRestart {
    park_seq: u64,
    resets_at: crate::time::Timestamp,
    continuation: Option<crate::server::acp_reconciler::PreparedContinuation>,
}

struct StructuredRestartLaunch {
    generation: u64,
    started: Instance,
    native: crate::server::session_store::NativeSessionStore,
    hooks: anyhow::Result<()>,
    /// The restart changed only the account, so the conversation continues where it stopped.
    account_swap: bool,
    carry: Option<crate::session::conversation_carry::ConversationCarry>,
    wake: Option<String>,
    reservation: Option<crate::acp::supervisor::ResumeReservation>,
    reason: &'static str,
    quota: Option<PreparedQuotaRestart>,
    /// A limit rejecting this launch is the target account's: it comes back on the error and
    /// parks nothing.
    defers_rate_limit: bool,
}

/// Without auto-resume a terminal launch starts a fresh conversation and still reports success,
/// so a required account swap must not reach it.
pub(super) fn terminal_resume_refusal(
    body: &crate::daemon::RestartSessionBody,
    structured: bool,
    auto_resume_on_restart: bool,
) -> Option<String> {
    (body.require_account_carry && !structured && !auto_resume_on_restart).then(|| {
        "A terminal session only keeps its conversation across an account swap while \
         auto_resume_on_restart is on for the target profile. Nothing changed."
            .to_owned()
    })
}

/// An explicit wake outranks a continuation an earlier failed resume left pending. Runs under the
/// submission authority, before the replacement worker exists to drain it.
pub(super) async fn discard_stale_continuation(
    state: &Arc<AppState>,
    id: &str,
    resumes_rate_limit: bool,
    explicit_wake: bool,
) {
    if resumes_rate_limit && explicit_wake {
        state.session_service.clear_pending_initial_turn(id).await;
    }
}

/// Select the next turn before publishing its durable quota release.
/// Deferred failover also requires the captured park and a current owner.
#[allow(clippy::too_many_arguments)]
pub(super) async fn continue_interrupted_prompt(
    state: &Arc<AppState>,
    id: &str,
    generation: u64,
    resets_at: crate::time::Timestamp,
    explicit_wake: bool,
    submission: &tokio::sync::OwnedMutexGuard<()>,
    prepared: Option<crate::server::acp_reconciler::PreparedContinuation>,
    expected_park_seq: Option<u64>,
) -> Result<bool, crate::acp::supervisor::SupervisorError> {
    let current = state.instances.read().await.iter().any(|row| {
        row.id == id
            && row.lifecycle_generation == generation
            && row.launch_is_finalized()
            && row.ensure_startable().is_ok()
            && (expected_park_seq.is_none()
                || (row.status != Status::Stopped && !row.is_idle_dormant()))
    });
    if !current {
        return if expected_park_seq.is_some() {
            Err(crate::acp::supervisor::SupervisorError::SpawnCancelled(
                id.to_owned(),
            ))
        } else {
            Ok(false)
        };
    }
    let selected = if explicit_wake {
        None
    } else {
        let prepared =
            prepared.ok_or(crate::acp::supervisor::SupervisorError::ControlStateUnavailable)?;
        let outcome = prepared.outcome(state, id, submission).await?;
        Some((prepared, outcome))
    };
    if let Some(expected_park_seq) = expected_park_seq {
        use crate::acp::supervisor::SupervisorError;
        let store = Arc::clone(&state.acp_event_store);
        let probe_id = id.to_owned();
        // Read the frontier first so a concurrent append invalidates this release.
        let (frontier, park) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let frontier = store.highest_seq(&probe_id)?;
            let park = store.rate_limit_park(&probe_id)?;
            Ok((frontier, park))
        })
        .await
        .map_err(|_| SupervisorError::NativeStoreUnavailable)?
        .map_err(|_| SupervisorError::NativeStoreUnavailable)?;
        match park {
            crate::acp::event_store::ControlKnowledge::Known(Some(park))
                if park.seq == expected_park_seq
                    && !park.cap_reached
                    && park.last_resume_attempt_ms.is_none() => {}
            crate::acp::event_store::ControlKnowledge::Indeterminate => {
                return Err(SupervisorError::ControlStateUnavailable);
            }
            _ => return Err(SupervisorError::SpawnCancelled(id.to_owned())),
        }
        let control = state
            .session_service
            .fold_control_state(id)
            .await
            .map_err(|_| SupervisorError::ControlStateUnavailable)?;
        if !control.connection_ready
            || control.connection_unavailable.is_some()
            || control.turn_active
        {
            return Err(SupervisorError::SpawnCancelled(id.to_owned()));
        }
        let current = state.instances.read().await.iter().any(|row| {
            row.id == id
                && row.lifecycle_generation == generation
                && row.launch_is_finalized()
                && row.ensure_startable().is_ok()
                && row.status != Status::Stopped
                && !row.is_idle_dormant()
        });
        if !current || crate::process::worker_registry::stop_requested(id) {
            return Err(SupervisorError::SpawnCancelled(id.to_owned()));
        }
        state
            .acp_supervisor
            .publish_rate_limit_auto_resumed_if_seq(id, resets_at, frontier)?;
    } else {
        state
            .acp_supervisor
            .publish_rate_limit_auto_resumed(id, resets_at, true)?;
    }
    if explicit_wake {
        // The caller clears stale pending only after this durable publication
        // succeeds, still under submission authority and before the spawn.
        return Ok(false);
    }
    // A continuation already pending is work too, even when the interrupted prompt is gone.
    let pending_before = state
        .instances
        .read()
        .await
        .iter()
        .any(|row| row.id == id && row.pending_initial_turn.is_some());
    use crate::server::acp_reconciler::ContinuationOutcome;
    let (prepared, outcome) =
        selected.ok_or(crate::acp::supervisor::SupervisorError::ControlStateUnavailable)?;
    let outcome = crate::server::acp_reconciler::install_prepared_continuation(
        state, id, submission, prepared, outcome,
    )
    .await?;
    Ok(pending_before
        || matches!(
            outcome,
            ContinuationOutcome::Installed | ContinuationOutcome::SupersededByQueue
        ))
}

async fn finish_deferred_quota_restart(
    state: &Arc<AppState>,
    id: &str,
    generation: u64,
    profile: &str,
    wake_requested: bool,
    prepared: PreparedQuotaRestart,
) -> anyhow::Result<bool> {
    state.acp_supervisor.wait_until_ready(id).await?;
    let namespace = state.profile_namespace.read().await;
    let submission = state.session_service.prompt_submission(id).await;
    let lock = state.instance_lock(id).await;
    let guard = lock.lock().await;
    let current = state.instances.read().await.iter().any(|row| {
        row.id == id
            && row.lifecycle_generation == generation
            && row.source_profile == profile
            && row.launch_is_finalized()
            && row.ensure_startable().is_ok()
            && row.status != Status::Stopped
            && !row.is_idle_dormant()
    });
    if !current || crate::process::worker_registry::stop_requested(id) {
        return Err(crate::acp::supervisor::SupervisorError::SpawnCancelled(id.to_owned()).into());
    }
    let park_seq = prepared.park_seq;
    let continued = continue_interrupted_prompt(
        state,
        id,
        generation,
        prepared.resets_at,
        wake_requested,
        &submission,
        prepared.continuation,
        Some(park_seq),
    )
    .await?;
    discard_stale_continuation(state, id, true, wake_requested).await;
    let pending = state
        .instances
        .read()
        .await
        .iter()
        .any(|row| row.id == id && row.pending_initial_turn.is_some());
    // Delivery retains submission, never the instance lock.
    drop(guard);
    drop(namespace);
    if continued {
        let delivery = crate::server::acp_reconciler::QuotaDelivery::new(park_seq);
        let handled = if pending {
            state
                .session_service
                .drain_pending_initial_turn_with_submission(id, &submission, Some(&delivery))
                .await
        } else {
            state
                .session_service
                .drain_queued_prompts_with_submission(id, &submission, Some(&delivery))
                .await
        };
        if !handled {
            return Err(
                crate::acp::supervisor::SupervisorError::SpawnCancelled(id.to_owned()).into(),
            );
        }
    }
    drop(submission);
    Ok(continued)
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
            account_swap,
            carry,
            wake,
            reservation,
            reason: _,
            quota,
            defers_rate_limit,
        } = launch;
        let quota = if account_swap { quota } else { None };
        let wake_requested = wake.is_some();
        let mut reservation = match reservation {
            Some(reservation) => reservation,
            None => supervisor.reserve_spawn(id, crate::acp::supervisor::ResumeKind::Spawn,
                crate::process::worker_registry::ResumeAuthority::Explicit).await?,
        };
        reservation.expected_lifecycle_generation = Some(generation);
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
            supervisor.validate_reserved_resume(&reservation)?;
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
        // Automatic failover keeps the park until the replacement really starts.
        let (deferred_quota, continued) = if defers_rate_limit {
            (quota, false)
        } else {
            let resumes_rate_limit = quota.is_some();
            let continued = match quota {
                Some(prepared) => continue_interrupted_prompt(
                    state, id, generation, prepared.resets_at, wake_requested,
                    &submission, prepared.continuation, None,
                ).await?,
                None => false,
            };
            discard_stale_continuation(state, id, resumes_rate_limit, wake_requested).await;
            (None, continued)
        };
        let agent = state
            .acp_supervisor
            .pick_agent_for_tool(
                &started.tool,
                started.agent_name.as_deref(),
                &started.source_profile,
                started.repo_config_path(),
            )
            .await;
        let mut request = crate::server::api::acp::spawn_request_for(
            &started,
            agent,
            started.sandbox_info.clone(),
        );
        request.launch_admission = Some(crate::acp::supervisor::LaunchAdmission {
            establishment: started
                .acp_establishment
                .as_ref()
                .and_then(crate::acp::establishment::AcpEstablishment::attempt)
                .cloned()
                .map(Arc::new),
            store: native.clone(),
            generation,
            namespace: Some(state.profile_namespace.clone()),
            native_scope: None,
        });
        if defers_rate_limit {
            reservation.lease().defer_rate_limit_rejection();
        }
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
        let continued = match deferred_quota {
            Some(prepared) => Box::pin(finish_deferred_quota_restart(
                state, id, generation, &profile, wake_requested, prepared,
            )).await?,
            None => continued,
        };
        let wake = if continued { String::new() } else { wake };
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
                        && matches!(dispatch,
                            Ok(crate::acp::dispatch::PromptDispatch::Sent
                                | crate::acp::dispatch::PromptDispatch::Steered))
                    {
                        let result = state
                            .session_service
                            .send_turn(
                                &crate::server::session_service::SessionCaller::User,
                                &id,
                                crate::server::session_service::SendTurnRequest { resume_reservation: None, resume_authority:
                                    crate::process::worker_registry::ResumeAuthority::Automatic,
                                text: &wake,
                                attachments: &[],
                                woke_idle_dormant: false,
                                prompt_ids: Vec::new(),
                                synthesized: true,
                                    origin: crate::daemon::PromptOrigin::Aoe,
                                no_revive: true, },
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
            if let Some(response) = sandbox_migration_response(state, id, &error).await {
                return response;
            }
            if let Some(crate::acp::supervisor::SupervisorError::Acp(failure)) =
                error.downcast_ref::<crate::acp::supervisor::SupervisorError>()
            {
                if failure.is_authentication_failure() {
                    tracing::warn!(target: "http.api.sessions", session = %id, %error, "structured restart failed: the agent is not logged in");
                    return api_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        AGENT_AUTH_FAILED,
                        "The agent could not authenticate: Authentication required",
                    );
                }
                if let crate::acp::acp_client::AcpError::RateLimited(info) = failure {
                    tracing::warn!(target: "http.api.sessions", session = %id, %error, "structured restart failed: the agent is rate limited");
                    return (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({
                            "error": AGENT_RATE_LIMITED,
                            "message": "The agent is rate limited",
                            "status": info.status,
                            "resets_at": info.resets_at,
                            "kind": info.kind,
                        })),
                    )
                        .into_response();
                }
            }
            if let Some(cause) = error.downcast_ref::<crate::acp::supervisor::SupervisorError>() {
                return crate::server::api::acp::supervisor_error_response(
                    "structured restart refused",
                    cause,
                );
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
    /// An automatic account failover decided on this event-log head and lifecycle generation.
    Failover {
        park_seq: u64,
        generation: u64,
    },
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

struct AgentPreparationInput {
    body: Result<
        Option<Json<crate::daemon::StartSessionBody>>,
        axum::extract::rejection::JsonRejection,
    >,
    captured: Option<Instance>,
    physical: Option<Arc<crate::session::LegacyToolObservation>>,
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
        AgentPreparationInput {
            body,
            captured: None,
            physical: None,
        },
        preparation,
        restart,
        None,
        RestartOrigin::Explicit,
        None,
    )
    .await
}

pub(super) async fn prepare_captured_terminal_session(
    state: Arc<AppState>,
    id: String,
    instance: Instance,
    expected: crate::daemon::NativePreparationIdentity,
    physical: Option<Arc<crate::session::LegacyToolObservation>>,
) -> axum::response::Response {
    prepare_agent_session_inner(
        state,
        id,
        AgentPreparationInput {
            body: Ok(Some(Json(crate::daemon::StartSessionBody {
                size: None,
                expected: Some(expected),
            }))),
            captured: Some(instance),
            physical,
        },
        AgentPreparation::Ensure,
        None,
        None,
        RestartOrigin::Explicit,
        None,
    )
    .await
}

/// Error code of a restart whose agent refused to start for want of a login.
const AGENT_AUTH_FAILED: &str = "agent_auth_failed";

/// Error code of a restart whose agent refused to start on a rate limit.
const AGENT_RATE_LIMITED: &str = "agent_rate_limited";

fn rate_limit_of(body: &serde_json::Value) -> Option<crate::acp::state::RateLimitInfo> {
    Some(crate::acp::state::RateLimitInfo {
        status: body["status"].as_str()?.to_owned(),
        resets_at: body["resets_at"]
            .as_str()
            .and_then(|at| crate::time::Timestamp::parse_rfc3339(at).ok()),
        kind: body["kind"].as_str()?.to_owned(),
    })
}

/// Why a failover swap did not happen.
#[derive(Debug, Clone)]
pub(crate) struct FailoverError {
    /// A retry cannot help for this park: the swap is refused by design.
    /// Anything else (a worker that was still stopping, a failed spawn, a log head that moved
    /// without ending the park) may clear by itself.
    pub permanent: bool,
    /// The target account's agent refused to start for want of a login.
    pub target_auth_failed: bool,
    /// The target account's agent refused to start on a rate limit of its own.
    pub target_rate_limit: Option<crate::acp::state::RateLimitInfo>,
    pub message: String,
}

impl FailoverError {
    fn from_response(status: StatusCode, body: Option<&serde_json::Value>) -> Self {
        let code = body
            .and_then(|body| body["error"].as_str())
            .unwrap_or_default();
        let message = body
            .and_then(|body| body["message"].as_str())
            .unwrap_or("no detail");
        Self {
            permanent: code == "account_swap_unavailable",
            target_auth_failed: code == AGENT_AUTH_FAILED,
            target_rate_limit: (code == AGENT_RATE_LIMITED)
                .then(|| body.and_then(rate_limit_of))
                .flatten(),
            message: format!("{status}: {message}"),
        }
    }
}

/// Switch a rate-limit-parked structured session to another account of the same agent,
/// continuing the interrupted prompt. `expected_seq` is the event-log head the failover
/// decision read: under the submission authority and the instance lock, the swap proceeds only
/// if the session is still parked on that head with no worker, so a prompt, a manual resume or
/// the timed auto-resume that landed meanwhile wins and nothing is sent twice. `generation` is
/// the row's lifecycle generation at the decision: a stop that landed since is the owner's and
/// is never consumed.
pub(crate) async fn failover_account_swap(
    state: &Arc<AppState>,
    id: &str,
    tool: &str,
    expected_seq: u64,
    generation: u64,
) -> Result<(), FailoverError> {
    failover_account_swap_with(state, id, tool, expected_seq, generation, async {}).await
}

/// `after_restart` runs once the restart returned, before the failure is handled: the point
/// where another lifecycle operation can complete.
pub(super) async fn failover_account_swap_with(
    state: &Arc<AppState>,
    id: &str,
    tool: &str,
    expected_seq: u64,
    generation: u64,
    after_restart: impl std::future::Future<Output = ()>,
) -> Result<(), FailoverError> {
    let claim = Arc::new(FailoverClaim::default());
    let response = Box::pin(prepare_agent_session_inner(
        state.clone(),
        id.to_owned(),
        AgentPreparationInput {
            body: Ok(None),
            captured: None,
            physical: None,
        },
        AgentPreparation::Restart,
        Some(Arc::new(crate::daemon::RestartSessionBody {
            tool: Some(tool.to_owned()),
            require_account_carry: true,
            ..Default::default()
        })),
        None,
        RestartOrigin::Failover {
            park_seq: expected_seq,
            generation,
        },
        Some(claim.clone()),
    ))
    .await;
    after_restart.await;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let body = axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    let found = claim.found.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(before) = found {
        let owned = claim
            .owned_generation
            .load(std::sync::atomic::Ordering::Acquire);
        restore_account(state, id, tool, before, owned).await;
    }
    Err(FailoverError::from_response(status, body.as_ref()))
}

/// What a failover's restart records under its own locks, for the rollback to read: the row's
/// account as the restart found it, and the newest generation the restart reserved (0 when none).
#[derive(Default)]
struct FailoverClaim {
    found: std::sync::Mutex<Option<AccountIdentity>>,
    owned_generation: std::sync::atomic::AtomicU64,
}

/// The account, conversation bindings and selection authority found before a failover swap.
struct AccountIdentity {
    tool: String,
    detect_as: String,
    agent_session_binding: Option<crate::session::ConversationBinding>,
    resume_binding: Option<crate::session::ConversationBinding>,
    acp_establishment: Option<crate::acp::establishment::AcpEstablishment>,
}

impl AccountIdentity {
    fn of(row: &Instance) -> Self {
        Self {
            tool: row.tool.clone(),
            detect_as: row.detect_as.clone(),
            agent_session_binding: row.agent_session_binding.clone(),
            resume_binding: row.resume_binding.clone(),
            acp_establishment: row.acp_establishment.clone(),
        }
    }
}

/// Restore only the failed swap's account, bindings and dormancy marker.
/// Its captured runner and native checkpoint retire before rollback; a live
/// worker or a newer lifecycle generation wins.
async fn restore_account(
    state: &Arc<AppState>,
    id: &str,
    target: &str,
    before: AccountIdentity,
    owned_generation: u64,
) {
    if owned_generation == 0 {
        return;
    }
    let lock = state.instance_lock(id).await;
    let _guard = lock.lock().await;
    let profile = {
        let rows = state.instances.read().await;
        let Some(row) = rows.iter().find(|row| row.id == id) else {
            return;
        };
        row.source_profile.clone()
    };
    let mut owned_generation = owned_generation;
    if state.acp_supervisor.is_running(id).await {
        return;
    }
    let Ok(captured) = state.acp_supervisor.capture_retirement(id) else {
        return;
    };
    if captured
        .lease
        .as_ref()
        .is_some_and(|lease| lease.defers_rate_limit_rejection())
    {
        if !state.acp_supervisor.binding_producer_is_current(&captured) {
            return;
        }
        let worker_state = Arc::clone(state);
        let session = id.to_owned();
        let profile = profile.clone();
        let expected_tool = target.to_owned();
        let checkpoint = tokio::task::spawn_blocking(move || -> anyhow::Result<u64> {
            use crate::session::SessionStore;
            let native = crate::server::session_store::NativeSessionStore::open(
                Arc::clone(&worker_state),
                &profile,
                Some(session.clone()),
            )?;
            let _lifecycle = native.storage().acquire_instance_lifecycle_lock(&session)?;
            let row = native
                .load()?
                .into_iter()
                .find(|row| row.id == session)
                .ok_or(crate::session::SessionGone)?;
            anyhow::ensure!(
                row.lifecycle_generation == owned_generation && row.tool == expected_tool,
                crate::session::LifecycleReservationError::Superseded
            );
            let retirement = crate::server::acp_events::BindingRetirement::prepare_captured(
                &worker_state,
                captured,
                &row,
                native.storage(),
            )?;
            retirement.activate();
            retirement.commit_under_native_lock(&native)?;
            worker_state
                .acp_supervisor
                .acknowledge_retirement(&retirement.target)?;
            let row = native
                .load()?
                .into_iter()
                .find(|row| row.id == session)
                .ok_or(crate::session::SessionGone)?;
            Ok(row.lifecycle_generation)
        })
        .await
        .map_err(anyhow::Error::from)
        .and_then(|result| result);
        match checkpoint {
            Ok(generation) => owned_generation = generation,
            Err(error) => {
                tracing::warn!(target: "http.api.sessions", session = %id, %error,
                    "failed failover retains its original retirement checkpoint");
                return;
            }
        }
        if state.acp_supervisor.is_running(id).await {
            return;
        }
    }
    let worker_state = state.clone();
    let session = id.to_owned();
    let target = target.to_owned();
    let restored = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        use crate::session::SessionStore;
        let native = crate::server::session_store::NativeSessionStore::open(
            worker_state,
            &profile,
            Some(session.clone()),
        )?;
        (&native as &dyn SessionStore).update(|rows, _| {
            let Some(row) = rows.iter_mut().find(|row| row.id == session) else {
                return Ok(());
            };
            if row.lifecycle_generation != owned_generation {
                return Ok(());
            }
            row.idle_dormant_since = None;
            if row.tool == target && before.tool != target {
                row.tool = before.tool;
                row.detect_as = before.detect_as;
                row.agent_session_binding = before.agent_session_binding;
                row.resume_binding = before.resume_binding;
                row.acp_establishment = before.acp_establishment;
            }
            Ok(())
        })
    })
    .await;
    if let Err(error) = restored
        .map_err(anyhow::Error::from)
        .and_then(|result| result)
    {
        tracing::warn!(target: "http.api.sessions", session = %id, %error, "could not restore the account after a failed failover swap");
    }
}

pub(crate) async fn recovery_restart(
    state: Arc<AppState>,
    id: String,
    expected_generation: u64,
) -> axum::response::Response {
    prepare_agent_session_inner(
        state,
        id,
        AgentPreparationInput {
            body: Ok(None),
            captured: None,
            physical: None,
        },
        AgentPreparation::Restart,
        Some(Arc::new(crate::daemon::RestartSessionBody {
            bound_hooks: true,
            ..Default::default()
        })),
        Some(expected_generation),
        RestartOrigin::StartupRecovery,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn prepare_agent_session_inner(
    state: Arc<AppState>,
    id: String,
    input: AgentPreparationInput,
    preparation: AgentPreparation,
    restart: Option<Arc<crate::daemon::RestartSessionBody>>,
    expected_generation: Option<u64>,
    restart_origin: RestartOrigin,
    failover_claim: Option<Arc<FailoverClaim>>,
) -> axum::response::Response {
    let AgentPreparationInput {
        body,
        captured,
        physical: mut captured_physical,
    } = input;
    if preparation == AgentPreparation::Ensure && state.cityhall_mode {
        return crate::server::api::cityhall_response();
    }
    if state.read_only && preparation == AgentPreparation::Ensure {
        return super::ensure::agent_target_response(&state, &id, None).await;
    }
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    if let Some(body) = restart
        .as_deref()
        .filter(|_| matches!(restart_origin, RestartOrigin::Explicit))
    {
        if let Some(response) = admit_restart_sandbox_login(&state, &id, body).await {
            return response;
        }
    }
    #[cfg(test)]
    launch_hook_gate::pause(&id, launch_hook_gate::Stage::BeforeLocks).await;
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
    let mut instance = match captured {
        Some(instance) => instance,
        None => {
            let instances = state.instances.read().await;
            let Some(instance) = instances.iter().find(|row| row.id == id) else {
                return if state.cityhall_mode {
                    crate::server::api::cityhall_response()
                } else {
                    crate::server::api::session_not_found()
                };
            };
            instance.clone()
        }
    };
    if preparation == AgentPreparation::Ensure && instance.is_structured() {
        return StatusCode::BAD_REQUEST.into_response();
    }
    if let RestartOrigin::Failover {
        park_seq: expected_seq,
        generation,
    } = restart_origin
    {
        // Idle reaping and status polling stop or dormant a row without bumping its generation,
        // so the status checks are not covered by the comparison.
        if instance.lifecycle_generation != generation
            || instance.status == Status::Stopped
            || instance.is_idle_dormant()
        {
            return api_error(
                StatusCode::CONFLICT,
                "park_changed",
                "The session was stopped or restarted since the failover was decided".to_string(),
            );
        }
        let store = Arc::clone(&state.acp_event_store);
        let probe_id = id.clone();
        let still_parked = tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
            use crate::acp::event_store::ControlKnowledge::Known;
            Ok(matches!(store.rate_limit_park(&probe_id)?, Known(Some(_)))
                && store.highest_seq(&probe_id)? == expected_seq)
        })
        .await;
        let still_parked = matches!(still_parked, Ok(Ok(true)));
        if !still_parked || state.acp_supervisor.is_running(&id).await {
            return api_error(
                StatusCode::CONFLICT,
                "park_changed",
                "The session moved on since the failover was decided".to_string(),
            );
        }
        // The account the rollback returns to is the one found under these locks: a restart
        // that changed it since the decision was made is not this swap's to undo.
        if let Some(claim) = &failover_claim {
            *claim.found.lock().unwrap_or_else(|e| e.into_inner()) =
                Some(AccountIdentity::of(&instance));
        }
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
    // A refused swap must not first consume a stop's resume authority below; the durable row is
    // checked again under the locks before the worker stops.
    if let Some(message) = restart
        .as_ref()
        .and_then(|body| account_swap_refusal(&instance, body))
    {
        return crate::server::api::api_error_with_code_header(
            crate::daemon::ApiErrorCode::AccountSwapUnavailable,
            message,
        );
    }
    match instance.require_resolved_acp_selection() {
        Err(error) if instance.is_structured() && restart.is_some() => {
            return structured_lifecycle_error(&state, error.into());
        }
        _ => {}
    }
    let quota = if let Some(body) = restart.as_ref().filter(|body| {
        instance.is_structured()
            && body.tool.as_deref().is_some_and(|tool| {
                tool != instance.tool
                    && matches!(
                        crate::session::conversation_carry::classify(
                            &instance,
                            body.profile.as_deref().unwrap_or(&instance.source_profile),
                            tool,
                        ),
                        crate::session::conversation_carry::ToolSwap::KeepConversation(_)
                    )
            })
    }) {
        let marker = match crate::server::api::acp::rate_limit_resume_probe(&state, &id).await {
            Ok(crate::acp::event_store::ControlKnowledge::Known(marker)) => marker,
            Ok(crate::acp::event_store::ControlKnowledge::Indeterminate) => {
                return structured_lifecycle_error(
                    &state,
                    crate::acp::supervisor::SupervisorError::ControlStateUnavailable.into(),
                )
            }
            Err(error) => return structured_lifecycle_error(&state, error.into()),
        };
        match marker {
            Some((park_seq, resets_at)) => {
                let continuation = if body.wake_message.is_some() {
                    None
                } else {
                    match crate::server::acp_reconciler::resolve_continuation(&state, &id).await {
                        Ok(prepared) => Some(prepared),
                        Err(error) => return structured_lifecycle_error(&state, error.into()),
                    }
                };
                Some(PreparedQuotaRestart {
                    park_seq,
                    resets_at,
                    continuation,
                })
            }
            None => None,
        }
    } else {
        None
    };
    let mut resume_reservation = None;
    // Automatic failover must not consume an owner's stop.
    if instance.is_structured()
        && matches!(restart_origin, RestartOrigin::Explicit)
        && (instance.status == Status::Stopped || instance.is_idle_dormant())
    {
        let resumed = if restart.is_some() {
            resume_structured_stop_deferred(&state, instance, None).await
        } else {
            resume_structured_stop(&state, instance, None).await
        };
        let (resumed, reservation) = match resumed {
            Ok(prepared) => prepared,
            Err(response) => return response,
        };
        instance = resumed;
        resume_reservation = reservation;
    }
    let structured = instance.is_structured();
    if structured && restart.is_some() && resume_reservation.is_none() {
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
        let (native, reserved, binding) = match reserved {
            Ok(Ok(reserved)) => reserved,
            Ok(Err(error)) => {
                return lifecycle_rejection(&state, &error)
                    .unwrap_or_else(|| StatusCode::INTERNAL_SERVER_ERROR.into_response());
            }
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        };
        let binding_target = binding.target.clone();
        let needs_retirement = resume_reservation.is_none();
        if let Some(claim) = &failover_claim {
            claim.owned_generation.store(
                reserved.lifecycle_generation,
                std::sync::atomic::Ordering::Release,
            );
        }
        let stopped = if resume_reservation.is_some() {
            Ok(())
        } else {
            state
                .acp_supervisor
                .shutdown_captured_and_wait(&id, &binding.target, std::time::Duration::from_secs(5))
                .await
        };
        if let Err(error) = stopped {
            tracing::warn!(target: "http.api.sessions", session = %id, %error, "structured restart did not prove teardown");
            return StatusCode::CONFLICT.into_response();
        }
        let released = tokio::task::spawn_blocking(move || {
            use crate::session::SessionStore as _;
            let _lifecycle = native
                .storage()
                .acquire_instance_lifecycle_lock(&reserved.id)?;
            binding.commit_under_native_lock(&native)?;
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
                row.idle_dormant_since = Some(crate::time::Timestamp::now());
                Ok(row.clone())
            })
        })
        .await;
        instance = match released {
            Ok(Ok(instance)) => instance,
            _ => return StatusCode::CONFLICT.into_response(),
        };
        if needs_retirement {
            if let Err(error) = state.acp_supervisor.acknowledge_retirement(&binding_target) {
                return structured_lifecycle_error(&state, error.into());
            }
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
    // This is an owned reference, not a lock; keep its directory pinned through hooks/waits.
    let (_preparation_source, preparation_identity) = match (|| -> anyhow::Result<_> {
        if preparation == AgentPreparation::Ensure && body.expected.is_none() {
            let source = crate::session::Storage::open_reference(&instance.source_profile)?;
            let expected = crate::daemon::NativePreparationIdentity::from_instance(
                &instance,
                source.bound_profile_identity()?,
            );
            Ok((Some(source), Some(expected)))
        } else {
            Ok((None, body.expected))
        }
    })() {
        Ok(captured) => captured,
        Err(error) => {
            tracing::warn!(target: "http.api.sessions", session = %id, %error,
                "native preparation profile scope unavailable");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    let worker_state = state.clone();
    let worker_claim = failover_claim.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        use crate::session::SessionStore;
        let profile = instance.source_profile.clone();
        let mut native = crate::server::session_store::NativeSessionStore::open(
            worker_state.clone(),
            &profile,
            (preparation != AgentPreparation::Ensure).then(|| instance.id.clone()),
        )?;
        native.configuration(Some(&profile))?;
        anyhow::ensure!(
            preparation_identity
                .as_ref()
                .is_none_or(|expected| expected.matches_source(native.storage())),
            crate::session::LifecycleReservationError::Superseded
        );
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
        let mut revival = if worker_restart.is_some() && !outgoing.is_structured() {
            outgoing.capture_revival_target()?
        } else {
            None
        };
        if let Some(body) = &worker_restart {
            account_swap_precondition(&outgoing, body)
                .map_err(LifecycleTargetError::AccountSwapRefused)?;
            // The launch runs under the target profile.
            let auto_resume = target_native
                .as_ref()
                .unwrap_or(&native)
                .configuration(Some(target.unwrap_or(&profile)))?
                .session
                .auto_resume_on_restart;
            if let Some(message) =
                terminal_resume_refusal(body, outgoing.is_structured(), auto_resume)
            {
                return Err(LifecycleTargetError::AccountSwapRefused(message).into());
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
        // One classification decides both the refusal and the carry.
        if let Some(body) = worker_restart
            .as_ref()
            .filter(|body| body.require_account_carry)
        {
            if !account_swap {
                let tool = body.tool.as_deref().unwrap_or_default();
                return Err(LifecycleTargetError::AccountSwapRefused(park_refusal(tool)).into());
            }
        }
        if let Some(body) = &worker_restart {
            let mut probe = outgoing.clone();
            if let Some(target) = target {
                probe.source_profile = target.into();
            }
            admit_restart(&mut probe, body, account_swap)?;
            if probe.is_sandboxed() && !instance.is_structured() {
                // Strictly, on what the session holds under these locks (the hook was resolved
                // before them), and before anything below stops the agent or discards its
                // container. A structured session was checked before its worker was stopped.
                probe.ensure_sandbox_launchable_strict(
                    target_native
                        .as_ref()
                        .map_or(&native as &dyn crate::session::SessionStore, |target| {
                            target
                        }),
                )?;
            }
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
        let mut start_admission = None;
        let mut ready_target = None;
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
                anyhow::ensure!(
                    preparation_identity
                        .as_ref()
                        .is_none_or(|expected| expected.matches(row)),
                    crate::session::LifecycleReservationError::Superseded
                );
                if worker_state.cityhall_mode && !row.is_structured() {
                    return Err(LifecycleTargetError::CityHall.into());
                }
                if let Some(body) = &worker_restart {
                    row.source_profile.clone_from(&profile);
                    admit_restart(row, body, account_swap)?;
                    row.rebase_acp_generation(native.storage())?;
                    return Ok(Some(row.clone()));
                }
                let now = crate::time::Timestamp::now();
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
                if row.is_structured() {
                    anyhow::ensure!(
                        preparation != AgentPreparation::Ensure,
                        "Structured sessions have no agent pane"
                    );
                    if row.status != Status::Stopped {
                        return Ok(None);
                    }
                } else {
                    let physical_target = captured_physical
                        .as_ref()
                        .map(|expected| -> anyhow::Result<_> {
                            let observed = super::ensure::physical_agent_target(row)?
                                .ok_or(crate::session::LifecycleReservationError::Superseded)?;
                            anyhow::ensure!(
                                observed.1.as_ref() == expected.as_ref(),
                                crate::session::LifecycleReservationError::Superseded
                            );
                            Ok(observed)
                        })
                        .transpose()?;
                    let admission = row.admit_native_start()?;
                    anyhow::ensure!(
                        captured_physical.is_none() || admission.is_none(),
                        crate::session::LifecycleReservationError::Superseded
                    );
                    let Some(admission) = admission else {
                        if preparation == AgentPreparation::Ensure {
                            let (name, observed, ready) = match physical_target {
                                Some(target) => target,
                                None => super::ensure::physical_agent_target(row)?
                                    .ok_or(crate::session::NativeInputError::Ownership)?,
                            };
                            anyhow::ensure!(
                                ready || captured_physical.is_some(),
                                crate::session::NativeInputError::Ownership
                            );
                            let physical = captured_physical.take().unwrap_or(observed);
                            ready_target = Some((
                                row.created_at,
                                row.title.clone(),
                                crate::daemon::TerminalTarget {
                                    tmux_session: name,
                                    physical: Some(physical),
                                    lifecycle_generation: row.lifecycle_generation,
                                    profile: row.source_profile.clone(),
                                    status: crate::daemon::TerminalTargetStatus::Alive,
                                },
                            ));
                        }
                        return Ok(None);
                    };
                    anyhow::ensure!(
                        preparation_identity.as_ref().is_none_or(|expected| {
                            expected.selection.as_ref() == Some(admission.selection())
                        }),
                        crate::session::LifecycleReservationError::Superseded
                    );
                    revival = admission.revival_target();
                    start_admission = Some(admission);
                }
                let generation = row.try_acquire_lifecycle_reservation(
                    LifecycleOperation::Launch,
                    Instance::LIFECYCLE_RESERVATION_TTL,
                    now,
                )?;
                row.idle_dormant_since = None;
                row.idle_entered_at = None;
                if row.is_structured() {
                    row.last_error.clear();
                    row.last_error_evidence = None;
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
            let prepared = if let Some((created_at, title, target)) = ready_target {
                // A fresh live observation is not yet in the runtime-only projection.
                // Adopt that exact observation before publishing its receipt.
                let mut owner = instance;
                owner.title = title;
                native.adopt_agent_observation(
                    &owner,
                    crate::session::PaneObservation {
                        state: crate::session::PanePresence::Alive,
                        tmux_session: Some(target.tmux_session.clone()),
                        physical: target.physical.clone(),
                        legacy_tool: None,
                    },
                )?;
                Some(super::ensure::PreparedAgentTarget {
                    created_at,
                    target,
                    native,
                })
            } else {
                None
            };
            return Ok((None, prepared));
        };
        if let Some(claim) = &worker_claim {
            claim.owned_generation.store(
                started.lifecycle_generation,
                std::sync::atomic::Ordering::Release,
            );
        }
        // The final capture can move an unbound id, so the copy follows the admitted row's ids.
        if let Some(carry) = conversation_carry.as_mut() {
            carry.retarget(crate::session::conversation_carry::conversation_ids(
                &started,
            ));
        }
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
        let reservation = started.prepare_reserved_launch_hooks(
            store,
            preparation == AgentPreparation::Restart && worker_restart.is_none(),
            crate::session::LaunchReservation {
                generation: started.lifecycle_generation,
                revival,
                start_admission,
                title_lock,
                lifecycle_lock,
            },
        )?;
        drop(identity);
        Ok((
            Some((
                reservation,
                started,
                native,
                (account_swap, conversation_carry),
            )),
            None,
        ))
    })
    .await;
    let mut prepared_target = None;
    let result = result.map(|result| {
        result.map(|(started, target)| {
            prepared_target = target;
            started
        })
    });
    drop(guard);
    drop(submission);
    drop(namespace);
    #[cfg(test)]
    launch_hook_gate::pause(&id, launch_hook_gate::Stage::BeforeHooks).await;
    let reason = restart_reason(restart_origin, restart.as_deref());
    let requested_wake_message = restart.as_ref().and_then(|body| body.wake_message.clone());
    let hooked = match result {
        Ok(Ok(Some((reservation, mut started, native, conversation_carry)))) => {
            tokio::task::spawn_blocking(move || {
                let generation = reservation.generation;
                let _timeout = restart.as_ref().filter(|body| body.bound_hooks).map(|_| {
                    crate::session::recovery::HookTimeoutScope::new(
                        crate::session::recovery::recovery_hook_timeout(),
                    )
                });
                let hooks = started.run_agent_launch_hooks(
                    restart.as_ref().is_some_and(|body| body.skip_on_launch),
                    &native,
                    None,
                    Some(generation),
                );
                Ok(Some((
                    reservation,
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
        Ok(Ok(Some((reservation, started, native, hooks, carry)))) if started.is_structured() => {
            return finish_structured_restart(
                &state,
                &id,
                StructuredRestartLaunch {
                    generation: reservation.generation,
                    started,
                    native,
                    hooks,
                    account_swap: carry.0,
                    carry: carry.1,
                    wake: requested_wake_message,
                    reservation: resume_reservation,
                    reason,
                    quota,
                    defers_rate_limit: matches!(restart_origin, RestartOrigin::Failover { .. }),
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
    let adoption_state = state.clone();
    let result = match hooked {
        Ok(Ok(Some((reservation, mut started, native, hooks, conversation_carry)))) => {
            tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                use crate::session::SessionStore;
                let generation = reservation.generation;
                let outcome = started.finish_reserved_launch(
                    &native,
                    size,
                    crate::session::ResumeLaunchOptions {
                        resume_policy: if preparation == AgentPreparation::Ensure {
                            crate::session::ResumeAttemptPolicy::Allow
                        } else {
                            crate::session::ResumeAttemptPolicy::HonorAutoResumeSetting
                        },
                        restart: preparation == AgentPreparation::Restart,
                        conversation_carry: conversation_carry.1,
                    },
                    reservation,
                    hooks,
                );
                let born_at = started.created_at;
                let born_pane = std::mem::take(&mut started.agent_pane);
                #[cfg(test)]
                tokio::runtime::Handle::current().block_on(
                    super::tests::terminal_custody::pause(
                        &started.id, super::tests::terminal_custody::Boundary::LaunchFinished,
                    ),
                );
                #[cfg(all(test, unix))]
                tokio::runtime::Handle::current().block_on(
                    durable_stop_tests::pause_before_adoption(&started.id),
                );
                let pending_release = outcome
                    .as_ref()
                    .err()
                    .is_some_and(|error| error.is::<crate::session::LaunchFinalizationPending>())
                    .then(|| started.id.clone());
                let adoption = (|| -> anyhow::Result<_> {
                let _title = crate::session::acquire_session_title_lock(&started.id)?;
                let _lifecycle = native.storage().acquire_instance_lifecycle_lock(&started.id)?;
                // The launch released its exclusions on return. A CLI peer may
                // have replaced it without the daemon cache observing that write.
                let durable = native.load()?.into_iter()
                    .find(|row| row.id == started.id)
                    .ok_or(crate::session::LifecycleReservationError::Superseded)?;
                anyhow::ensure!(
                    durable.created_at == born_at
                        && durable.lifecycle_generation == generation
                        && durable.source_profile == started.source_profile,
                    crate::session::LifecycleReservationError::Superseded
                );
                let panes = match crate::tmux::batch_pane_metadata() {
                    Ok(panes) => Some(panes),
                    Err(error) => {
                        tracing::warn!(target: "http.api.sessions", session = %started.id, "post-launch pane observation failed: {error}");
                        None
                    }
                };
                native.check_available()?;
                let _publication = adoption_state.publication.blocking_write();
                let metadata = adoption_state.canonical_metadata.blocking_read();
                let mut instances = adoption_state.instances.blocking_write();
                let legacy_scope = crate::server::pane::legacy_tool_scope_for_panes(
                    &instances, &metadata.raw_tool_names, panes.as_ref(),
                );
                let row = instances.iter_mut().find(|row| {
                    row.id == started.id
                        && row.lifecycle_generation == generation
                        && started.lifecycle_generation == generation
                        && row.created_at == started.created_at
                        && row.source_profile == started.source_profile
                        && row.title == started.title
                }).ok_or(crate::session::LifecycleReservationError::Superseded)?;
                row.inherit_process_runtime(started);
                let profile = row.source_profile.clone();
                let tools = metadata.auxiliary_tools.get(&profile)
                    .map(Vec::as_slice).unwrap_or_default();
                crate::server::pane::sample_panes(
                    row, tools, panes.as_ref(), legacy_scope.as_ref(), None,
                );
                adoption_state.mutation_epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                adoption_state.runtime.request_publish();
                    Ok(profile)
                })();
                if let Some(id) = pending_release {
                    native.defer_lifecycle_release(
                        id,
                        crate::session::LifecycleRelease::IndeterminateLaunch,
                        generation,
                        None,
                    );
                    return Err(outcome.expect_err("pending release retains the launch error"));
                }
                match adoption {
                    Ok(profile) => {
                        // Only this operation's captured birth can authorize its target.
                        let prepared = super::ensure::PreparedAgentTarget::captured(
                            born_at, generation, profile.clone(), born_pane,
                            if preparation == AgentPreparation::Restart {
                                crate::daemon::TerminalTargetStatus::Restarted
                            } else {
                                crate::daemon::TerminalTargetStatus::Created
                            },
                            native,
                        );
                        Ok(Some((generation, profile, outcome, prepared)))
                    }
                    Err(adoption_error) => match outcome {
                        // Adoption cannot replace migration failure or retained finalization debt.
                        Err(error)
                            if error.is::<crate::migrations::SandboxMigrationError>()
                                || error.is::<crate::session::LaunchFinalizationPending>() =>
                        {
                            Err(error)
                        }
                        _ => Err(adoption_error),
                    },
                }
            })
            .await
        }
        Ok(Ok(None)) => Ok(Ok(None)),
        Ok(Err(error)) => Ok(Err(error)),
        Err(error) => Err(error),
    };
    let outcome = match result {
        Ok(Ok(Some((generation, profile, outcome, prepared)))) => {
            restart_identity = Some((generation, profile));
            prepared_target = prepared;
            outcome.map(Some)
        }
        Ok(Ok(None)) => Ok(None),
        Ok(Err(error)) => Err(error),
        Err(error) => Err(error.into()),
    };
    let migration_failure = outcome
        .as_ref()
        .err()
        .is_some_and(|error| error.is::<crate::migrations::SandboxMigrationError>());
    let outcome = match outcome {
        Err(error) => {
            let response = if let Some(&cause) =
                error.downcast_ref::<crate::session::LaunchIndeterminateCause>()
            {
                super::ensure::indeterminate_launch_response(cause)
            } else if let Some(response) = lifecycle_rejection(&state, &error) {
                response
            } else if error.is::<crate::session::NativeStoreUnavailable>()
                || error.is::<crate::session::LaunchFinalizationPending>()
            {
                StatusCode::SERVICE_UNAVAILABLE.into_response()
            } else if error.is::<crate::session::HostHookApprovalRequired>() {
                crate::server::api::api_error_with_code_header(
                    crate::daemon::ApiErrorCode::AgentHooksNotApproved,
                    "Agent hook paths have not been approved; run `aoe hooks approve` on the Daemon host",
                )
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
            } else if let Some(failure) =
                error.downcast_ref::<crate::migrations::SandboxMigrationError>()
            {
                let code = failure.api_code();
                crate::server::api::api_error_with_code_header(
                    code,
                    code.sandbox_migration_message().unwrap(),
                )
            } else {
                tracing::warn!(target: "http.api.sessions", session = %id, %error, "session start failed");
                api_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "start_failed",
                    "Session start failed",
                )
            };
            if restart_identity.is_some() || migration_failure {
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
    let resume_triggered = if let Some(reservation) = resume_reservation.take() {
        match crate::server::acp_reconciler::trigger_resume_background(
            &state.session_service,
            &id,
            crate::process::worker_registry::ResumeAuthority::Explicit,
            crate::server::sandbox_login::LaunchOrigin::Interactive,
            Some(reservation),
        )
        .await
        {
            Ok(crate::server::acp_reconciler::ResumeTrigger::Started) => true,
            Ok(_) => return StatusCode::CONFLICT.into_response(),
            Err(error) => {
                return crate::server::api::acp::supervisor_error_response("start failed", &error);
            }
        }
    } else {
        false
    };
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
        let mut wake_source = None;
        let target = if row.is_structured() {
            None
        } else {
            let Some(prepared) = prepared_target.take().filter(|prepared| {
                prepared.created_at == row.created_at
                    && prepared.target.lifecycle_generation == generation
                    && prepared.target.profile == profile
                    && prepared.target.physical == row.agent_pane.physical
                    && row.agent_pane.tmux_session.as_deref()
                        == Some(prepared.target.tmux_session.as_str())
            }) else {
                return StatusCode::CONFLICT.into_response();
            };
            wake_source = Some(prepared.native);
            Some(prepared.target)
        };
        let mut wake = None;
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
                let profile_identity = wake_source
                    .as_ref()
                    .and_then(|native| native.storage().bound_profile_identity().ok());
                wake = Some((wake_message, row.clone(), profile_identity));
            }
        }
        drop(rows);
        if let Some((wake_message, wake_row, profile_identity)) = wake {
            let delay = wake_row.send_keys_enter_delay();
            crate::session::restart::spawn_wake_worker(
                &wake_row,
                wake_message,
                profile_identity,
                delay,
            );
        }
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
        #[cfg(test)]
        super::tests::terminal_custody::pause(
            &id,
            super::tests::terminal_custody::Boundary::EnsureReceipt,
        )
        .await;
        let response =
            super::ensure::prepared_agent_target_response(&state, &id, outcome, prepared_target)
                .await;
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
        if structured && !resume_triggered {
            state.acp_supervisor.request_respawn(&id);
        }
        if outcome.is_some() || structured {
            tracing::info!(target: "session.start", session = %id, ?outcome, "session started");
        }
        crate::server::runtime::session_mutation_response(&state, &id, None::<()>).await
    }
}

pub(super) async fn sandbox_migration_response(
    state: &Arc<AppState>,
    id: &str,
    error: &anyhow::Error,
) -> Option<axum::response::Response> {
    let failure = error.downcast_ref::<crate::migrations::SandboxMigrationError>()?;
    tracing::warn!(target: "http.api.sessions", session = %id, error = %format_args!("{error:#}"), "sandbox migration did not complete");
    let code = failure.api_code();
    let response = crate::server::api::api_error_with_code_header(
        code,
        code.sandbox_migration_message().unwrap(),
    );
    Some(match state.runtime.publish(state).await {
        Ok(snapshot) => crate::server::runtime::mutation_response(&snapshot.value.cursor, response),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    })
}

/// Holds one session's restart at a named point, so a test can run another operation there.
#[cfg(test)]
pub(super) mod launch_hook_gate {
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(in crate::server::api::sessions) enum Stage {
        /// Before the restart takes its submission and instance locks.
        BeforeLocks,
        /// After the locks are released, before the pre-launch hooks run.
        BeforeHooks,
    }

    type Gate = (
        String,
        Stage,
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    );
    static GATE: std::sync::Mutex<Option<Gate>> = std::sync::Mutex::new(None);

    /// Arms the gate for `id` at `stage`: the receiver fires once a restart waits there, and
    /// sending on the returned sender lets it go on.
    pub(in crate::server::api::sessions) fn arm(
        id: &str,
        stage: Stage,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        *GATE.lock().unwrap() = Some((id.to_owned(), stage, entered_tx, release_rx));
        (entered_rx, release_tx)
    }

    pub(super) async fn pause(id: &str, stage: Stage) {
        let gate = {
            let mut gate = GATE.lock().unwrap();
            if gate
                .as_ref()
                .is_some_and(|(session, at, _, _)| session == id && *at == stage)
            {
                gate.take()
            } else {
                None
            }
        };
        if let Some((_, _, entered, release)) = gate {
            let _ = entered.send(());
            let _ = release.await;
        }
    }
}

#[cfg(all(test, unix))]
mod durable_stop_tests {
    use super::*;
    use crate::process::worker_registry::{self, WorkerRecord};
    type AdoptionGate = (
        String,
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    );
    static ADOPTION_GATE: std::sync::Mutex<Option<AdoptionGate>> = std::sync::Mutex::new(None);
    struct AdoptionGateGuard {
        id: String,
        release: Option<tokio::sync::oneshot::Sender<()>>,
    }
    impl Drop for AdoptionGateGuard {
        fn drop(&mut self) {
            let mut gate = ADOPTION_GATE.lock().unwrap();
            if gate.as_ref().is_some_and(|(id, _, _)| id == &self.id) {
                gate.take();
            }
        }
    }
    struct MigrationFixture {
        row: Instance,
        private: std::path::PathBuf,
        cause: Option<String>,
        _path: crate::session::test_support::EnvGuard,
        _env: crate::session::test_support::EnvGuard,
        _home: crate::session::test_support::AppDirGuard,
        root: tempfile::TempDir,
    }
    impl MigrationFixture {
        fn new(case: &str) -> anyhow::Result<Self> {
            use std::os::unix::fs::PermissionsExt;

            let root = tempfile::tempdir_in("/tmp")?;
            let home = crate::session::test_support::isolate_app_dir_at(root.path());
            crate::migrations::run_migrations()?;
            crate::session::config::update_app_state(|state| {
                state.has_acknowledged_agent_hooks = true;
            })?;
            crate::session::config::update_config(|config| {
                config.systemd.enabled = false;
                config.session.smart_rename = false;
                config.sandbox.container_runtime = crate::session::ContainerRuntimeName::Docker;
            })?;
            let env = crate::session::test_support::EnvGuard::unset(&["DOCKER_CONTEXT"])
                .and_set("DOCKER_HOST", "unix:///var/run/docker.sock")
                .and_set(
                    "AOE_DEFER_SANDBOX_MIGRATION",
                    if case == "deferred" { "1" } else { "" },
                )
                .and_set("AOE_TMUX_SOCKET", root.path().join("tmux.sock"));
            let bin = root.path().join("bin");
            std::fs::create_dir(&bin)?;
            let message = if case == "other" {
                "permission denied"
            } else {
                "Error: No such container (container not found)"
            };
            for runtime in ["docker", "podman", "container"] {
                let path = bin.join(runtime);
                std::fs::write(&path, format!(
                    "#!/bin/sh\ncase \"$*\" in *inspect*) echo '{message}' >&2; exit 1;; esac\nexit 0\n"
                ))?;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
            }
            let path = crate::session::test_support::path_prepended(&bin);
            let project = root.path().join("project");
            std::fs::create_dir(&project)?;
            let mut raw =
                serde_json::to_value(Instance::new("original", project.to_str().unwrap()))?;
            raw["tool"] = "codex".into();
            raw["command"] = "codex".into();
            raw["status"] = "stopped".into();
            raw["sandbox_store_generation"] = 0.into();
            raw["sandbox_info"] = serde_json::json!({
                "enabled": true, "image": "test-image", "container_name": "migration",
            });
            let mut row: Instance = serde_json::from_value(raw)?;
            row.source_profile = "default".into();
            let source = root.path().join(".codex/sandbox").join(&row.id);
            std::fs::create_dir_all(&source)?;
            let private = source.join("config.toml");
            std::fs::write(&private, b"private-content")?;
            let cause = if case == "failed" {
                let blocker = root.path().join(".codex/sandbox-v2");
                std::fs::write(&blocker, b"not-a-directory")?;
                let error = std::fs::create_dir(blocker.join("child")).unwrap_err();
                assert_eq!(error.kind(), std::io::ErrorKind::NotADirectory);
                Some(nix::errno::Errno::from_raw(error.raw_os_error().unwrap()).to_string())
            } else {
                None
            };
            Ok(Self {
                row,
                private,
                cause,
                _path: path,
                _env: env,
                _home: home,
                root,
            })
        }
    }
    pub(super) async fn pause_before_adoption(id: &str) {
        let gate = {
            let mut gate = ADOPTION_GATE.lock().unwrap();
            if gate.as_ref().is_some_and(|(session, _, _)| session == id) {
                gate.take()
            } else {
                None
            }
        };
        if let Some((_, entered, release)) = gate {
            let _ = entered.send(());
            let _ = release.await;
        }
    }
    #[tokio::test]
    #[serial_test::serial]
    async fn superseded_sandbox_launch_preserves_migration_failure() -> anyhow::Result<()> {
        for case in ["deferred", "failed", "other"] {
            let fixture = MigrationFixture::new(case)?;
            let row = fixture.row.clone();
            let root = &fixture.root;
            let private = &fixture.private;
            crate::server::test_support::seed_instances_on_disk_for_test(
                "default",
                vec![row.clone()],
            );
            let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
            crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
            let before = state.runtime.publish(&state).await?;
            let request_state = state.clone();
            let id = row.id.clone();
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            let mut gate = AdoptionGateGuard {
                id: id.clone(),
                release: Some(release_tx),
            };
            *ADOPTION_GATE.lock().unwrap() = Some((id.clone(), entered_tx, release_rx));
            let request = start_session(State(request_state), Path(id), Ok(None));
            tokio::pin!(request);
            tokio::time::timeout(std::time::Duration::from_secs(20), async {
                tokio::select! {
                    entered = entered_rx => entered?,
                    _ = &mut request => anyhow::bail!("request completed before adoption gate"),
                }
                Ok::<(), anyhow::Error>(())
            })
            .await??;
            let newer = {
                let mut instances = state.instances.write().await;
                let current = instances
                    .iter_mut()
                    .find(|current| current.id == row.id)
                    .unwrap();
                current.title = "newer title".into();
                current.clone()
            };
            gate.release.take().unwrap().send(()).unwrap();
            let response = tokio::time::timeout(std::time::Duration::from_secs(30), &mut request)
                .await?
                .into_response();
            let expected = match case {
                "deferred" => crate::daemon::ApiErrorCode::SandboxMigrationDeferred,
                "failed" => crate::daemon::ApiErrorCode::SandboxMigrationFailed,
                _ => crate::daemon::ApiErrorCode::LifecycleSuperseded,
            };
            assert_eq!(response.status(), expected.status(), "{case}");
            assert_eq!(
                response.headers()[crate::daemon::ERROR_CODE_HEADER],
                expected.as_str(),
                "{case}"
            );
            if case != "other" {
                assert_eq!(
                    response.headers()[crate::daemon::RUNTIME_EPOCH_HEADER],
                    before.value.cursor.epoch
                );
                let revision: u64 = response.headers()[crate::daemon::RUNTIME_REVISION_HEADER]
                    .to_str()?
                    .parse()?;
                assert!(revision > before.value.cursor.revision);
            } else {
                assert!(!response
                    .headers()
                    .contains_key(crate::daemon::RUNTIME_EPOCH_HEADER));
            }
            let body = axum::body::to_bytes(response.into_body(), 8192).await?;
            let public = String::from_utf8_lossy(&body);
            if let Some(message) = expected.sandbox_migration_message() {
                assert!(public.contains(message), "{case}: {public}");
            }
            assert!(!public.contains(root.path().to_str().unwrap()));
            assert!(!public.contains("private-content"));
            assert_eq!(std::fs::read(private)?, b"private-content");
            let instances = state.instances.read().await;
            let current = instances
                .iter()
                .find(|current| current.id == row.id)
                .unwrap();
            assert_eq!(current.title, "newer title");
            assert_eq!(current.status, newer.status);
            assert_eq!(current.lifecycle_generation, newer.lifecycle_generation);
            assert_eq!(current.last_error, newer.last_error);
        }
        Ok(())
    }
    #[tokio::test]
    #[serial_test::serial]
    async fn sandbox_migration_refusals_publish_known_sanitized_failures() -> anyhow::Result<()> {
        for (deferred, target) in [
            (true, "start"),
            (false, "start"),
            (true, "restart"),
            (false, "restart"),
            (true, "container-terminal"),
            (false, "container-terminal"),
        ] {
            let fixture = MigrationFixture::new(if deferred { "deferred" } else { "failed" })?;
            let root = &fixture.root;
            let private = &fixture.private;
            let source = private.parent().unwrap();
            let cause = &fixture.cause;
            let mut row = fixture.row.clone();
            if target == "restart" {
                row.view = crate::session::View::Structured;
                row.status = Status::Running;
                crate::server::test_support::select_retained_for_test(&mut row);
            }
            crate::server::test_support::seed_instances_on_disk_for_test(
                "default",
                vec![row.clone()],
            );
            let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
            crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
            let logs = crate::session::test_support::LogCapture::start();
            let before = state.runtime.publish(&state).await?;
            let response = match target {
                "restart" => {
                    restart_session(State(state.clone()), Path(row.id.clone()), Ok(None)).await
                }
                "container-terminal" => super::super::ensure::ensure_container_terminal(
                    State(state.clone()),
                    Path(row.id.clone()),
                    axum::extract::Query(crate::server::live_ws::TerminalIndexQuery::default()),
                    Ok(None),
                )
                .await
                .into_response(),
                _ => start_session(State(state.clone()), Path(row.id.clone()), Ok(None))
                    .await
                    .into_response(),
            };
            let code = if deferred {
                crate::daemon::ApiErrorCode::SandboxMigrationDeferred
            } else {
                crate::daemon::ApiErrorCode::SandboxMigrationFailed
            };
            assert_eq!(
                response.status(),
                code.status(),
                "target={target} deferred={deferred} control_path_and_bytes={:?} logs={}",
                crate::process::worker_registry::socket_path_for(&row.id).map(|socket| {
                    let control = crate::process::worker::control_socket_sibling(&socket);
                    let bytes = control.as_os_str().as_encoded_bytes().len();
                    (control, bytes)
                }),
                logs.contents()
            );
            assert_eq!(
                response
                    .headers()
                    .get(crate::daemon::ERROR_CODE_HEADER)
                    .and_then(|value| value.to_str().ok()),
                Some(code.as_str()),
                "{}",
                logs.contents()
            );
            assert_eq!(
                response.headers()[crate::daemon::RUNTIME_EPOCH_HEADER],
                before.value.cursor.epoch
            );
            let revision: u64 = response.headers()[crate::daemon::RUNTIME_REVISION_HEADER]
                .to_str()?
                .parse()?;
            assert!(revision > before.value.cursor.revision);
            let bytes = axum::body::to_bytes(response.into_body(), 8192).await?;
            let public = String::from_utf8_lossy(&bytes);
            assert!(!public.contains(root.path().to_str().unwrap()));
            assert!(!public.contains("private-content"));
            if let Some(cause) = cause.as_ref() {
                assert!(!public.contains(cause));
            }
            if target == "container-terminal" {
                if let Some(cause) = cause.as_ref() {
                    let captured = logs.contents();
                    assert!(
                        captured.lines().any(|line| {
                            line.contains(&row.id)
                                && line.contains("sandbox-v2")
                                && line.contains(cause)
                        }),
                        "missing correlated migration cause: {captured}"
                    );
                }
            }
            let rows = state.instances.read().await;
            let current = rows.iter().find(|current| current.id == row.id).unwrap();
            assert_eq!(
                current.status,
                if target == "container-terminal" {
                    Status::Stopped
                } else {
                    Status::Error
                }
            );
            assert!(current.last_error.as_deref().is_none_or(|message| {
                !message.contains(source.to_str().unwrap())
                    && !message.contains("private-content")
                    && cause.as_ref().is_none_or(|cause| !message.contains(cause))
            }));
            assert!(current.lifecycle_reservation.is_none());
            assert_eq!(current.sandbox_store_generation, 0);
            assert!(private.exists());
        }
        Ok(())
    }
    #[tokio::test]
    #[serial_test::serial]
    async fn structured_creation_and_stopped_start_preserve_original_admission_at_capacity() {
        use crate::acp::establishment::AcpEstablishment;
        use crate::acp::runner_lifecycle::{ProcessControl as _, SystemProcessControl};
        use crate::acp::supervisor::test_support::managed::with_managed_capacity;

        with_managed_capacity(1000, 1, |scope| {
            Box::pin(async move {
                let project = scope.tempdir();
                let sid = "capacity-stopped-original";
                let (config, original_native) = scope.native_peer(
                    project.path().into(),
                    "codex",
                    "codex",
                    move |request| {
                        let id = request.get("id")?;
                        Some(match request["method"].as_str().unwrap() {
                            "initialize" => serde_json::json!({"jsonrpc":"2.0","id":id,
                                "result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}}),
                            "session/new" => serde_json::json!({"jsonrpc":"2.0","id":id,
                                "result":{"sessionId":sid}}),
                            method => panic!("unexpected original Native method: {method}"),
                        })
                    },
                );
                let (gate, release, entered) = scope.gate();
                scope.configure_sdk(crate::acp::acp_client::SdkTestOptions {
                    initialize_response_application: Some(gate),
                    ..Default::default()
                });
                let (id, events) = scope
                    .create_observed(config, "capacity-stopped-original")
                    .await;
                tokio::time::timeout(std::time::Duration::from_secs(10), entered)
                    .await
                    .expect("structured creation reaches actual Native initialize")
                    .unwrap();
                let created = Storage::open_unwatched("test")
                    .unwrap()
                    .load_instances_complete()
                    .unwrap()
                    .into_iter()
                    .find(|row| row.id == id)
                    .unwrap();
                let attempt = created
                    .acp_establishment
                    .as_ref()
                    .and_then(AcpEstablishment::attempt)
                    .unwrap()
                    .clone();
                scope.state.runtime.publish(&scope.state).await.unwrap();
                let competing = crate::server::api::acp::spawn_acp(
                    State(scope.state.clone()),
                    Path(id.clone()),
                    Ok(Json(serde_json::from_value(serde_json::json!({})).unwrap())),
                )
                .await
                .into_response();
                assert_eq!(competing.status(), StatusCode::CONFLICT);
                assert!(matches!(
                    crate::server::acp_reconciler::trigger_resume_background(
                        &scope.state.session_service,
                        &id,
                        worker_registry::ResumeAuthority::Automatic,
                        crate::server::sandbox_login::LaunchOrigin::Unattended,
                        None,
                    )
                    .await
                    .unwrap(),
                    crate::server::acp_reconciler::ResumeTrigger::AlreadyResuming
                ));
                release.release();
                let original = scope.observe_external_resume(id, events).await;
                original.ready().await;
                let id = original.id.clone();
                let owner = original.lease.spawned_runner().expect("actual original runner");
                assert_ne!(owner.pid, std::process::id());
                let state = scope.state.clone();
                let storage = Storage::open_unwatched("test").unwrap();
                let load = || {
                    storage
                        .load_instances_complete()
                        .unwrap()
                        .into_iter()
                        .find(|row| row.id == id)
                        .unwrap()
                };
                assert!(matches!(
                    load().acp_establishment,
                    Some(AcpEstablishment::Applied { .. })
                ));
                assert_eq!(
                    load().acp_establishment,
                    Some(AcpEstablishment::Applied { attempt_id: attempt.id })
                );
                original.stop_session().await;
                assert!(!SystemProcessControl::default().is_alive(owner));
                let stopped = load();
                assert_eq!(stopped.status, Status::Stopped);
                assert_eq!(stopped.acp_session_id.as_deref(), Some(sid));
                let choice = crate::acp::state::NativeBindingState::capture(&stopped);
                let authority = stopped.acp_establishment.clone();
                let generation = stopped.lifecycle_generation;
                let lifecycle_reservation = stopped.lifecycle_reservation.clone();
                let stop = worker_registry::stop_intent(&id).unwrap();

                let filler_project = scope.tempdir();
                let (config, filler_native) = scope.native_peer(
                    filler_project.path().into(),
                    "codex",
                    "codex",
                    |request| {
                        let id = request.get("id")?;
                        Some(match request["method"].as_str().unwrap() {
                            "initialize" => serde_json::json!({"jsonrpc":"2.0","id":id,
                                "result":{"protocolVersion":1,"agentCapabilities":{}}}),
                            "session/new" => serde_json::json!({"jsonrpc":"2.0","id":id,
                                "result":{"sessionId":"capacity-actual-filler"}}),
                            method => panic!("unexpected filler Native method: {method}"),
                        })
                    },
                );
                let filler = scope.spawn(config, "capacity-actual-filler").await.unwrap();
                filler.ready().await;
                let filler_owner = filler.lease.spawned_runner().expect("actual capacity occupant");
                assert!(SystemProcessControl::default().is_alive(filler_owner));
                let original_methods = original_native.requests();
                let filler_methods = filler_native.requests();
                let frontier = state.acp_event_store.highest_seq(&id).unwrap();
                let (load_config, load_native) = scope.native_peer(
                    project.path().into(),
                    "codex",
                    "codex",
                    move |request| {
                        let id = request.get("id")?;
                        Some(match request["method"].as_str().unwrap() {
                            "initialize" => serde_json::json!({"jsonrpc":"2.0","id":id,
                                "result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}}),
                            "session/load" => {
                                assert_eq!(request["params"]["sessionId"], sid);
                                serde_json::json!({"jsonrpc":"2.0","id":id,"result":{}})
                            }
                            method => panic!("ordinary resume cannot issue New/Fork/prompt: {method}"),
                        })
                    },
                );
                let namespace = state.profile_namespace.read().await;
                let submission = state.session_service.prompt_submission(&id).await;
                let lock = state.instance_lock(&id).await;
                let guard = lock.lock().await;
                let refused = resume_structured_stop(&state, stopped, None)
                    .await
                    .expect_err("a real other worker fills capacity one");
                assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
                let body = axum::body::to_bytes(refused.into_body(), 4096).await.unwrap();
                assert!(
                    String::from_utf8_lossy(&body).contains("worker capacity full (1/1)"),
                    "refusal must come from actual capacity admission"
                );
                let after = load();
                assert!(choice.matches(&after));
                assert_eq!(after.acp_establishment, authority);
                assert!(matches!(after.acp_establishment, Some(AcpEstablishment::Applied { .. })));
                assert_eq!(after.lifecycle_generation, generation);
                assert_eq!(after.lifecycle_reservation, lifecycle_reservation);
                assert_eq!(after.status, Status::Stopped);
                assert_eq!(worker_registry::stop_intent(&id).unwrap(), stop);
                assert_eq!(state.acp_event_store.highest_seq(&id).unwrap(), frontier);
                assert!(state.acp_supervisor.current_lease(&id).is_none());
                assert!(load_native.requests().is_empty());
                assert_eq!(original_native.requests(), original_methods);
                assert_eq!(filler_native.requests(), filler_methods);
                drop(guard);
                drop(submission);
                drop(namespace);

                filler.stop_session().await;
                assert!(!SystemProcessControl::default().is_alive(filler_owner));
                let events = scope.bind_external_launch(&id, load_config);
                let response = start_session(
                    State(state.clone()),
                    Path(id.clone()),
                    Ok(None),
                )
                .await.into_response();
                assert_eq!(response.status(), StatusCode::OK);
                let resumed = scope.observe_external_resume(id.clone(), events).await;
                resumed.ready().await;
                let current = load();
                assert_eq!(current.acp_session_id.as_deref(), Some(sid));
                assert!(current.lifecycle_generation > generation);
                assert!(matches!(
                    current.acp_establishment,
                    Some(AcpEstablishment::Applied { .. })
                ));
                assert!(worker_registry::stop_intent(&id).unwrap().is_none());
                assert_eq!(original_native.requests(), ["initialize", "session/new"]);
                assert_eq!(load_native.requests(), ["initialize", "session/load"]);
                assert_ne!(resumed.lease.spawned_runner(), Some(owner));
                let record = {
                    let registry = worker_registry::registry_lock_until(
                        &id, tokio::time::Instant::now() + std::time::Duration::from_secs(10),
                    ).await.expect("the actual runner releases registry publication");
                    registry.record().unwrap().expect("the actual resumed runner remains registered")
                };
                assert_eq!(
                    record.stored_acp_session_id
                        .as_deref(),
                    Some(sid)
                );
            })
        })
        .await;
    }

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
            "PI_CODING_AGENT_DIR",
            "PI_CODING_AGENT_SESSION_DIR",
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
        let _pi = crate::session::test_support::install_login_shell_path_command(
            root.path(),
            "pi",
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
        for (tool, transcript_bound, expected_cause) in [
            (
                "pi",
                false,
                crate::session::ManagedResumeCause::PiTranscriptBindingRequired,
            ),
            (
                "omp",
                false,
                crate::session::ManagedResumeCause::OmpTranscriptBindingRequired,
            ),
            (
                "omp",
                true,
                crate::session::ManagedResumeCause::ExecutionContextUnattested,
            ),
            (
                "prime-agent",
                true,
                crate::session::ManagedResumeCause::ExecutionContextUnattested,
            ),
        ] {
            let mut row = Instance::new("managed refusal", project.to_str().unwrap());
            row.tool = tool.into();
            row.command = tool.into();
            row.source_profile = "default".into();
            row.status = Status::Stopped;
            row.agent_session_id = Some(sid.into());
            let mut binding = crate::session::ConversationBinding::unknown(sid);
            binding.transcript_path = transcript_bound.then(|| transcript.clone());
            row.agent_session_binding = Some(binding);
            row.omp_capture_generation = Some("original-generation".into());
            if tool == "prime-agent" {
                let agent = crate::agents::get_agent("prime-agent").unwrap();
                assert!(row.launch_invokes_resolved_agent_directly(agent));
                assert_eq!(row.command, "prime-agent");
                assert_eq!(row.agent_session_id.as_deref(), Some(sid));
                assert_eq!(
                    std::fs::read_to_string(project.join(".prime/agent/settings.json")).unwrap(),
                    r#"{"sessionDir":7}"#
                );
            } else {
                assert!(row.supports_native_resume());
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
            assert_eq!(
                headers[crate::daemon::MANAGED_RESUME_CAUSE_HEADER],
                expected_cause.as_str(),
            );
            let code = crate::daemon::ApiErrorCode::from_headers(status, &headers, false).unwrap();
            assert_eq!(
                code,
                crate::daemon::ApiErrorCode::ManagedResumeRefused(expected_cause)
            );
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
        row.acp_session_id = Some("retained-generic-restart".into());
        row.resume_intent = crate::session::ResumeIntent::Use("retained-generic-restart".into());
        row.mark_idle_dormant();
        let observed = row
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Stop,
                Instance::LIFECYCLE_RESERVATION_TTL,
                crate::time::Timestamp::now(),
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
            row.acp_session_id = Some(format!("retained-internal-{reason}"));
            row.resume_intent =
                crate::session::ResumeIntent::Use(format!("retained-internal-{reason}"));
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
                "idle" => {
                    let (lease, activity_seq) =
                        state.acp_supervisor.idle_observation(&row.id).unwrap();
                    state.acp_supervisor.test_bind_native_scope(&lease, &row);
                    let receipt = state
                        .session_service
                        .mark_idle_dormant(&row.id, lease.native_scope().unwrap().unwrap())
                        .await
                        .unwrap()
                        .unwrap();
                    let retirement = state
                        .acp_supervisor
                        .shutdown_idle(
                            &row.id,
                            (&lease, lease.admissions()),
                            activity_seq,
                            receipt.stamp,
                        )
                        .await
                        .unwrap();
                    drop(receipt);
                    state
                        .acp_supervisor
                        .settle_idle_retirement(retirement)
                        .await;
                }
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
            assert_eq!(stored.is_idle_dormant(), reason == "idle", "{reason}");
            assert!(
                worker_registry::stop_intent(&row.id).unwrap().is_none(),
                "{reason}"
            );
            if !stored.is_idle_dormant() {
                let admission = crate::acp::supervisor::LaunchAdmission {
                    establishment: None,
                    store: Arc::new(Storage::open_unwatched("default").unwrap()),
                    generation: stored.lifecycle_generation,
                    namespace: None,
                    native_scope: None,
                };
                let mut child = crate::acp::supervisor::with_durable_launch(
                    Some(admission),
                    row.id.clone(),
                    || {
                        std::process::Command::new("true").spawn().map_err(|error| {
                            crate::acp::acp_client::AcpError::Spawn(error.to_string())
                        })
                    },
                )
                .await
                .unwrap();
                assert!(child.wait().unwrap().success());
            }
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
        if let Err(error) = async {
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
            tracing::warn!(target: "acp.supervisor", session = %id, %error,
                "snooze committed; native retirement or binding remains unconfirmed");
            return api_error(StatusCode::SERVICE_UNAVAILABLE, "outcome_unconfirmed",
                "Snooze committed, but native retirement is unconfirmed. Check canonical state before retrying.");
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
    use super::*;

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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial]
    async fn explicit_restart_keeps_existing_pending_when_quota_publication_fails() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| {
            Box::pin(async move {
                let mut live = crate::server::test_support::managed_control_session(
                    scope,
                    "restart-quota-publication-io",
                    "claude",
                    None,
                )
                .await;
                live.park("original interrupted work").await;
                let id = live.id().to_owned();
                let state = live.state.clone();
                state
                    .session_service
                    .set_pending_initial_turn(
                        &id,
                        "existing pending work".into(),
                        Vec::new(),
                        crate::daemon::PromptOrigin::Unknown,
                    )
                    .await;
                let (park_seq, resets_at) =
                    match crate::server::api::acp::rate_limit_resume_probe(&state, &id)
                        .await
                        .unwrap()
                    {
                        crate::acp::event_store::ControlKnowledge::Known(Some(resets_at)) => {
                            resets_at
                        }
                        marker => panic!("expected the original quota marker, got {marker:?}"),
                    };
                // A real restart retires the original before reserving a replacement; detaching
                // alone correctly refuses Spawn while that retained physical owner stays alive.
                live.client.shutdown().await.unwrap();
                let requests = live.native.requests();
                let frontier = state.acp_event_store.highest_seq(&id).unwrap();
                let budget = state
                    .acp_event_store
                    .rate_limit_redelivery_streak(&id)
                    .unwrap();
                assert!(!state.acp_supervisor.is_running(&id).await);
                let preparation_state = state.clone();
                let target = id.clone();
                let (native, started, generation) = tokio::task::spawn_blocking(move || {
                    let runtime = tokio::runtime::Handle::current();
                    let mut started = runtime
                        .block_on(preparation_state.instances.read())
                        .iter()
                        .find(|row| row.id == target)
                        .unwrap()
                        .clone();
                    let native = crate::server::session_store::NativeSessionStore::open(
                        preparation_state,
                        &started.source_profile,
                        Some(target),
                    )
                    .unwrap();
                    let generation = started
                        .acquire_lifecycle_reservation(
                            &native,
                            LifecycleOperation::Launch,
                            Some(Status::Starting),
                        )
                        .unwrap();
                    native.adopt_runtime_fields(&started).unwrap();
                    (native, started, generation)
                })
                .await
                .unwrap();
                state.acp_event_store.test_conn().execute_batch(
                    "CREATE TEMP TRIGGER fail_explicit_restart_quota BEFORE INSERT ON acp_events
                     WHEN NEW.discriminant = 'RateLimitAutoResumed'
                     BEGIN SELECT RAISE(ABORT, 'genuine explicit restart quota IO failure'); END;"
                ).unwrap();
                let response = finish_structured_restart(
                    &state,
                    &id,
                    StructuredRestartLaunch {
                        generation,
                        started,
                        native,
                        hooks: Ok(()),
                        account_swap: true,
                        carry: None,
                        wake: Some("explicit replacement wake".into()),
                        reservation: None,
                        reason: "user",
                        defers_rate_limit: false,
                        quota: Some(PreparedQuotaRestart {
                            park_seq,
                            resets_at,
                            continuation: None,
                        }),
                    },
                )
                .await;
                assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
                let current = state
                    .instances
                    .read()
                    .await
                    .iter()
                    .find(|row| row.id == id)
                    .unwrap()
                    .clone();
                assert!(
                    current.launch_is_finalized(),
                    "post-hook generation must finalize before the refused breadcrumb"
                );
                assert_eq!(
                    current
                        .pending_initial_turn
                        .as_ref()
                        .map(|turn| turn.text.as_str()),
                    Some("existing pending work")
                );
                let durable = crate::session::Storage::open_unwatched(&current.source_profile)
                    .unwrap()
                    .load()
                    .unwrap()
                    .into_iter()
                    .find(|row| row.id == id)
                    .unwrap();
                assert_eq!(
                    serde_json::to_value(durable.pending_initial_turn).unwrap(),
                    serde_json::to_value(current.pending_initial_turn).unwrap()
                );
                assert!(!state.acp_supervisor.is_running(&id).await);
                assert_eq!(state.acp_event_store.highest_seq(&id).unwrap(), frontier);
                assert_eq!(
                    state
                        .acp_event_store
                        .rate_limit_redelivery_streak(&id)
                        .unwrap(),
                    budget
                );
                assert_eq!(live.native.requests(), requests);
                state
                    .acp_event_store
                    .test_conn()
                    .execute_batch("DROP TRIGGER fail_explicit_restart_quota")
                    .unwrap();
            })
        })
        .await;
    }
}
