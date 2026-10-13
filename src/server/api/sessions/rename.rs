//! Rename, worktree-edit, and attach-project endpoints.

use super::*;
use crate::daemon::{
    AttachProjectBody, AttachProjectOutcome, AttachedProject, AttachedWorkerOutcome, RenameOutcome,
    RenameSessionBody, SetWorktreeNameBody, WorktreeEditOutcome,
};
use crate::session::SessionStore;

pub async fn rename_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<RenameSessionBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    if let Some(response) = cityhall_block_non_structured(&state, &id).await {
        return response;
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    match edit_session(&state, &id, SessionEdit::Rename(body)).await {
        Ok(warnings) => {
            crate::server::runtime::session_mutation_response(
                &state,
                &id,
                Some(RenameOutcome { warnings }),
            )
            .await
        }
        Err(response) => response,
    }
}

pub async fn set_worktree_name(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<SetWorktreeNameBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    if let Some(response) = cityhall_block_non_structured(&state, &id).await {
        return response;
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    match edit_session(&state, &id, SessionEdit::Worktree(body)).await {
        Ok(warnings) => {
            crate::server::runtime::session_mutation_response(
                &state,
                &id,
                Some(WorktreeEditOutcome { warnings }),
            )
            .await
        }
        Err(response) => response,
    }
}

enum SessionEdit {
    Rename(RenameSessionBody),
    Worktree(SetWorktreeNameBody),
}

fn edit_identity_matches(row: &Instance, expected: &Instance) -> bool {
    row.id == expected.id
        && row.title == expected.title
        && row.project_path == expected.project_path
        && row.group_path == expected.group_path
        && row.lifecycle_generation == expected.lifecycle_generation
        && row.lifecycle_reservation.is_none()
        && row.worktree_info == expected.worktree_info
}

#[derive(Debug, thiserror::Error)]
#[error("session is active or its sandbox still holds the worktree")]
struct WorktreeHeld;

async fn edit_session(
    state: &Arc<AppState>,
    id: &str,
    edit: SessionEdit,
) -> Result<Vec<String>, axum::response::Response> {
    if state.read_only {
        return Err(crate::server::api::read_only_response());
    }
    if let Some(response) = cityhall_block_non_structured(state, id).await {
        return Err(response);
    }
    match &edit {
        SessionEdit::Rename(body) => {
            if let Some(title) = &body.title {
                if title.trim().is_empty() {
                    return Err(api_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_title",
                        "Title cannot be empty",
                    ));
                }
                if let Err(message) = validate_display_label(title.trim(), "title") {
                    return Err(api_error(StatusCode::BAD_REQUEST, "invalid_title", message));
                }
            }
            if let Some(group) = &body.group {
                if !group.is_empty() {
                    if let Err(message) = validate_display_label(group, "group") {
                        return Err(api_error(StatusCode::BAD_REQUEST, "invalid_group", message));
                    }
                }
            }
            if body
                .profile
                .as_ref()
                .is_some_and(|profile| profile.is_empty())
            {
                return Err(api_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_profile",
                    "Profile cannot be empty",
                ));
            }
        }
        SessionEdit::Worktree(body) if body.name.trim().is_empty() => {
            return Err(api_error(
                StatusCode::BAD_REQUEST,
                "invalid_worktree_name",
                "Workdir name cannot be empty",
            ));
        }
        SessionEdit::Worktree(_) => {}
    }
    // Namespace -> submission -> instance -> identity -> title -> lifecycle -> stores.
    // Retain the namespace across the composed title/group/profile transaction.
    let namespace = state.profile_namespace.clone().write_owned().await;
    let Some(submission) = state
        .session_service
        .prompt_submission_for_session(id)
        .await
    else {
        return Err(session_not_found());
    };
    let lock = state.instance_lock(id).await;
    let guard = lock.lock().await;
    let live = state
        .instances
        .read()
        .await
        .iter()
        .find(|row| row.id == id)
        .cloned()
        .ok_or_else(session_not_found)?;
    let requested_profile = match &edit {
        SessionEdit::Rename(body) => body.profile.as_deref().unwrap_or(&live.source_profile),
        SessionEdit::Worktree(_) => &live.source_profile,
    };
    if !state
        .canonical_metadata
        .read()
        .await
        .profiles
        .iter()
        .any(|profile| profile.name == requested_profile)
    {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "profile_not_found",
            "Target profile does not exist",
        ));
    }
    let target_profile = requested_profile.to_owned();
    let prepare_state = state.clone();
    let prepared = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let identity = crate::session::acquire_session_identity_lock()?;
        let title_lock = crate::session::acquire_session_title_lock(&live.id)?;
        let native = crate::server::session_store::NativeSessionStore::open(
            prepare_state.clone(),
            &live.source_profile,
            None,
        )?;
        let lifecycle = native.storage().acquire_instance_lifecycle_lock(&live.id)?;
        let mut before = native
            .load()?
            .into_iter()
            .find(|row| row.id == live.id)
            .ok_or(crate::session::SessionGone)?;
        before.source_profile.clone_from(&live.source_profile);
        before.merge_runtime_from_reload(&live);
        anyhow::ensure!(
            before.lifecycle_reservation.is_none(),
            crate::session::LifecycleReservationError::Superseded
        );
        let mut after = before.clone();
        let tied = before.tie_workdir_applies(
            native
                .configuration(Some(&live.source_profile))?
                .session
                .tie_workdir_to_name,
        );
        let (name, rename_branch, allow_unchanged) = match edit {
            SessionEdit::Rename(body) => {
                let title_requested = body.title.is_some();
                if let Some(title) = body.title {
                    after.title = title.trim().to_owned();
                }
                if let Some(group) = body.group {
                    after.group_path = group;
                }
                let name = (tied && (title_requested || body.rename_branch))
                    .then(|| crate::session::worktree_edit::worktree_leaf_from_title(&after.title));
                (name, body.rename_branch, true)
            }
            SessionEdit::Worktree(body) => {
                anyhow::ensure!(
                    before.worktree_info.is_some(),
                    crate::session::worktree_edit::WorktreeEditError::NotManaged
                );
                anyhow::ensure!(!tied, TiedWorktree);
                (Some(body.name.trim().to_owned()), body.rename_branch, false)
            }
        };
        after.source_profile.clone_from(&target_profile);
        if let Some(name) = &name {
            let worktree = before
                .worktree_info
                .as_ref()
                .ok_or(crate::session::worktree_edit::WorktreeEditError::NotManaged)?;
            after.project_path = crate::session::worktree_edit::target_worktree_path(
                std::path::Path::new(&before.project_path),
                name,
            )
            .ok_or_else(|| {
                crate::session::worktree_edit::WorktreeEditError::NoParent(
                    before.project_path.clone().into(),
                )
            })?
            .to_string_lossy()
            .into_owned();
            if rename_branch {
                after.worktree_info.as_mut().expect("checked above").branch =
                    crate::session::builder::git_sanitize_branch_name(name);
            }
            let renames = crate::session::worktree_edit::worktree_branch_rename_required(
                worktree,
                name,
                rename_branch,
            );
            // Standalone workdir editing has always refused active sessions, even a no-op.
            anyhow::ensure!(
                !(before.status.blocks_worktree_edit()
                    && (!allow_unchanged || after.project_path != before.project_path || renames)),
                WorktreeHeld
            );
        }
        let target = if target_profile != before.source_profile {
            Some(crate::server::session_store::NativeSessionStore::open(
                prepare_state,
                &target_profile,
                None,
            )?)
        } else {
            None
        };
        // Advisory preflight before ACP quiescence. The locked commit repeats this
        // before git/container effects, including for cross-profile duplicates.
        let rows = match &target {
            Some(target) => target.load()?,
            None => native.load()?,
        };
        validate_edit_duplicate(&rows, &before, &after)?;
        let moves = after.project_path != before.project_path;
        Ok((
            native,
            target,
            before,
            after,
            name,
            rename_branch,
            allow_unchanged,
            moves,
            identity,
            title_lock,
            lifecycle,
        ))
    })
    .await;
    let (
        native,
        target,
        before,
        after,
        name,
        rename_branch,
        allow_unchanged,
        moves,
        identity,
        title_lock,
        lifecycle,
    ) = match prepared {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(error)) => return Err(session_edit_error(&error, false)),
        Err(error) => {
            tracing::error!(%error, "session edit preparation task failed");
            return Err(api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "title_lock_failed",
                "Could not prepare session edit",
            ));
        }
    };
    // Never call the supervisor while a storage/publication lock is held.
    if moves {
        quiesce_structured_worker_for_worktree_move(state, id, before.is_structured()).await?;
    }
    let effect_applied = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let effect_flag = effect_applied.clone();
    let old_title = before.title.clone();
    let new_title = after.title.clone();
    let structured = before.is_structured();
    let profiles = vec![before.source_profile.clone(), after.source_profile.clone()];
    let committed = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let changes = [(before, after)];
        let (before, after) = (&changes[0].0, &changes[0].1);
        let effect = |candidate: &Instance| -> anyhow::Result<()> {
            let Some(name) = name.as_deref() else {
                return Ok(());
            };
            if moves
                && crate::session::worktree_edit::ensure_sandbox_container_released(
                    &before.id,
                    before.is_sandboxed(),
                )
            {
                return Err(WorktreeHeld.into());
            }
            let result = crate::session::worktree_edit::edit_worktree_workdir(
                crate::session::worktree_edit::WorktreeEditRequest {
                    worktree_info: before
                        .worktree_info
                        .as_ref()
                        .expect("validated managed worktree"),
                    current_path: std::path::Path::new(&before.project_path),
                    new_name: name,
                    rename_branch,
                },
            );
            match result {
                Ok(result) => {
                    effect_flag.store(
                        result.new_path != std::path::Path::new(&before.project_path)
                            || result.new_branch.as_deref().is_some_and(|branch| {
                                before
                                    .worktree_info
                                    .as_ref()
                                    .is_some_and(|worktree| worktree.branch != branch)
                            }),
                        std::sync::atomic::Ordering::SeqCst,
                    );
                    anyhow::ensure!(
                        result.new_path == std::path::Path::new(&candidate.project_path)
                            && result.new_branch.as_deref().is_none_or(|branch| candidate
                                .worktree_info
                                .as_ref()
                                .is_some_and(|worktree| worktree.branch == branch)),
                        "git edit landed at a different path or branch than the metadata candidate"
                    );
                    if moves {
                        crate::session::worktree_edit::discard_sandbox_container_after_move(
                            &before.id,
                            before.is_sandboxed(),
                        );
                    }
                    Ok(())
                }
                Err(crate::session::worktree_edit::WorktreeEditError::Unchanged)
                    if allow_unchanged =>
                {
                    Ok(())
                }
                Err(error) => {
                    if matches!(
                        error,
                        crate::session::worktree_edit::WorktreeEditError::RollbackFailed { .. }
                    ) {
                        effect_flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    Err(error.into())
                }
            }
        };
        if let Some(target) = target {
            let group_move =
                crate::session::GroupMovePlan::single(&before.group_path, &after.group_path);
            native.move_instances_to_with_effect(
                &target,
                &changes,
                crate::session::ProfileMovePlan {
                    group_move: &group_move,
                    merge_complete_post: true,
                    account_swap: false,
                },
                |rows, candidates| {
                    let candidate = &candidates[0];
                    anyhow::ensure!(
                        edit_identity_matches(candidate, after),
                        crate::session::LifecycleReservationError::Superseded
                    );
                    validate_edit_duplicate(rows, before, candidate)
                },
                |candidates| effect(&candidates[0]),
            )?;
        } else {
            (&native as &dyn SessionStore).update(|rows, groups| {
                let row = rows
                    .iter()
                    .find(|row| row.id == before.id)
                    .ok_or(crate::session::SessionGone)?;
                anyhow::ensure!(
                    edit_identity_matches(row, before),
                    crate::session::LifecycleReservationError::Superseded
                );
                validate_edit_duplicate(rows, before, after)?;
                effect(after)?;
                let row = rows
                    .iter_mut()
                    .find(|row| row.id == before.id)
                    .expect("row validated above");
                row.title.clone_from(&after.title);
                row.group_path.clone_from(&after.group_path);
                row.project_path.clone_from(&after.project_path);
                if rename_branch && name.is_some() {
                    row.worktree_info.clone_from(&after.worktree_info);
                }
                if !row.group_path.is_empty() {
                    let mut tree = crate::session::GroupTree::new_with_groups(&[], groups);
                    tree.create_group(&row.group_path);
                    *groups = tree.get_all_groups();
                }
                Ok(())
            })?;
        }
        Ok((identity, title_lock, lifecycle))
    })
    .await;
    let locks = match committed {
        Ok(Ok(locks)) => locks,
        Ok(Err(error)) => {
            let partial = effect_applied.load(std::sync::atomic::Ordering::SeqCst);
            if partial {
                state
                    .mark_reload_failure(crate::daemon::RuntimeHealth::Degraded {
                        code: crate::daemon::ReloadFailureCode::ProfileData,
                        profiles,
                    })
                    .await;
            }
            return Err(session_edit_error(&error, partial));
        }
        Err(error) => {
            tracing::error!(%error, "session edit task failed after admission");
            state
                .mark_reload_failure(crate::daemon::RuntimeHealth::Degraded {
                    code: crate::daemon::ReloadFailureCode::ProfileData,
                    profiles,
                })
                .await;
            return Err(api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "persist_failed",
                "Session edit task failed; git effects may have occurred and were not rolled back",
            ));
        }
    };
    let mut warnings = Vec::new();
    if old_title != new_title && !structured {
        let rekey_id = id.to_owned();
        match tokio::task::spawn_blocking(move || crate::tmux::rekey_session(&rekey_id, &old_title, &new_title)).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => warnings.push(format!("Session metadata was renamed, but its live tmux session could not be rekeyed: {error}")),
            Err(error) => warnings.push(format!("Session metadata was renamed, but its live tmux session could not be rekeyed: {error}")),
        }
    }
    drop(locks);
    drop(guard);
    drop(submission);
    drop(namespace);
    Ok(warnings)
}

fn validate_edit_duplicate(
    rows: &[Instance],
    before: &Instance,
    after: &Instance,
) -> anyhow::Result<()> {
    let pair_changed = before.title != after.title
        || before.source_profile != after.source_profile
        || before.project_path.trim_end_matches('/') != after.project_path.trim_end_matches('/');
    if pair_changed
        && is_duplicate_session(
            rows.iter(),
            &after.title,
            &after.project_path,
            (before.source_profile == after.source_profile).then_some(before.id.as_str()),
        )
    {
        return Err(EditDuplicate(after.title.clone()).into());
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("standalone workdir editing is disabled while the directory follows the session title")]
struct TiedWorktree;

fn session_edit_error(error: &anyhow::Error, partial: bool) -> axum::response::Response {
    if partial {
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, "persist_failed",
            "Git effects occurred, but the session edit could not be committed and published; the filesystem/branch effects were not rolled back");
    }
    if error.is::<crate::session::SessionGone>() {
        return session_not_found();
    }
    if error.is::<crate::session::NativeStoreUnavailable>()
        || error.is::<crate::session::SessionCommitApplied>()
    {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    if error.is::<crate::session::LifecycleReservationError>()
        || error.is::<crate::session::ProfileMoveRejected>()
    {
        return api_error(
            StatusCode::CONFLICT,
            "superseded",
            "Session changed while preparing its edit",
        );
    }
    if error.is::<WorktreeHeld>() {
        return api_error(
            StatusCode::CONFLICT,
            "session_running",
            "Stop the session before editing its worktree directory or branch",
        );
    }
    if error.is::<TiedWorktree>() {
        return api_error(StatusCode::CONFLICT, "tied", "Renaming is unified while Tie Worktree Directory to Session Name is on; rename the session instead");
    }
    if let Some(error) = error.downcast_ref::<crate::session::worktree_edit::WorktreeEditError>() {
        let (status, message) = worktree_edit_error_response(error);
        return api_error(status, "worktree_edit_failed", message);
    }
    // Preserve duplicate-session classification without parsing an error string.
    if error.is::<EditDuplicate>() {
        return api_error(
            StatusCode::CONFLICT,
            "duplicate_session",
            duplicate_session_error(
                &error
                    .downcast_ref::<EditDuplicate>()
                    .expect("checked above")
                    .0,
            )
            .to_string(),
        );
    }
    tracing::error!(%error, "session edit failed");
    api_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "persist_failed",
        "Could not persist the session edit",
    )
}

#[derive(Debug, thiserror::Error)]
#[error("session title/path already exists: {0}")]
struct EditDuplicate(String);

/// Quiesce a structured-view worker before its worktree directory moves. A live
/// ACP worker is pinned to the current cwd, so `git worktree move` crash-loops
/// it at the stale baked-in cwd until the reconciler parks the session with a
/// misleading banner (#2260). `blocks_worktree_edit` misses this because a
/// "stopped" structured session sits at Idle yet still owns a live worker.
///
/// `shutdown` is reversible: it keeps the transcript and `acp_session_id`, so
/// after the move the reconciler fresh-spawns at the new path and resumes via
/// session/load. Callers hold `instance_lock` across shutdown, move and persist,
/// and the reconciler re-reads `project_path` under it, so the respawn never
/// targets the old path. Refuses the move (409) if a live worker cannot be
/// stopped.
async fn quiesce_structured_worker_for_worktree_move(
    state: &Arc<AppState>,
    id: &str,
    is_structured: bool,
) -> Result<(), axum::response::Response> {
    if !is_structured {
        return Ok(());
    }
    match state.acp_supervisor.shutdown(id).await {
        Ok(()) | Err(crate::acp::supervisor::SupervisorError::UnknownSession(_)) => Ok(()),
        Err(e) => {
            tracing::warn!(
                target: "http.api.sessions",
                session = %id,
                "could not stop structured-view worker before worktree move: {e}"
            );
            Err(api_error(
                StatusCode::CONFLICT,
                "worker_shutdown_failed",
                "Could not stop the structured view worker before renaming; retry in a moment",
            ))
        }
    }
}

/// Map a worktree-edit failure to an HTTP status and client-safe message.
/// Validation failures are 400/409; git/IO failures stay generic, since raw git
/// stderr and IO paths must not reach the wire.
fn worktree_edit_error_response(
    e: &crate::session::worktree_edit::WorktreeEditError,
) -> (StatusCode, String) {
    use crate::session::worktree_edit::WorktreeEditError as E;
    match e {
        E::NotManaged => (
            StatusCode::BAD_REQUEST,
            "This worktree is not managed by aoe; its workdir name cannot be edited".to_string(),
        ),
        E::EmptyName => (
            StatusCode::BAD_REQUEST,
            "Workdir name cannot be empty".to_string(),
        ),
        E::Unchanged => (
            StatusCode::BAD_REQUEST,
            "The workdir name is unchanged".to_string(),
        ),
        E::NoParent(_) => (
            StatusCode::BAD_REQUEST,
            "Cannot determine the worktree's parent directory".to_string(),
        ),
        E::SourceMissing(_) => (
            StatusCode::CONFLICT,
            "The worktree directory no longer exists on disk".to_string(),
        ),
        E::TargetExists(_) => (
            StatusCode::CONFLICT,
            "A directory with that name already exists".to_string(),
        ),
        E::BranchExists(name) => (
            StatusCode::CONFLICT,
            format!("Branch '{name}' already exists"),
        ),
        E::RollbackFailed { .. } => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to move the worktree, and rolling back the branch rename also failed; the repository may be left on the new branch".to_string(),
        ),
        E::Git(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to move the worktree".to_string(),
        ),
    }
}

pub async fn attach_session_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<AttachProjectBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    if let Some(response) = crate::server::api::cityhall_block(&state) {
        return response;
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    let raw = body.project.trim();
    if raw.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_project",
            "Project path or name is required",
        );
    }
    let namespace = state.profile_namespace.clone().read_owned().await;
    let profile = match state.instances.read().await.iter().find(|row| row.id == id) {
        Some(row) => row.source_profile.clone(),
        None => return session_not_found(),
    };
    let repo = match resolve_project_input(&profile, raw).await {
        Ok(repo) => repo,
        Err(message) => return api_error(StatusCode::BAD_REQUEST, "invalid_project", message),
    };
    let on_existing = if body.attach_existing_branch {
        crate::session::attach_project::ExistingBranch::Attach
    } else {
        crate::session::attach_project::ExistingBranch::Refuse
    };
    let result =
        crate::server::attach_project::attach_project(&state, &id, &repo, on_existing).await;
    drop(namespace);
    match result {
        Ok((outcome, worker)) => {
            use crate::server::attach_project::WorkerOutcome;
            let worker = match worker {
                WorkerOutcome::Restarted => AttachedWorkerOutcome::Restarted,
                WorkerOutcome::NotRunning => AttachedWorkerOutcome::NotRunning,
                WorkerOutcome::RestartFailed(message) => {
                    AttachedWorkerOutcome::RestartFailed { message }
                }
            };
            crate::server::runtime::session_mutation_response(
                &state,
                &id,
                Some(AttachProjectOutcome {
                    attached: AttachedProject {
                        name: outcome.repo.name,
                        worktree_path: outcome.repo.worktree_path,
                        branch: outcome.repo.branch,
                        branch_created: !outcome.repo.branch_preexisting,
                        moved_to: outcome.moved_to,
                    },
                    warnings: outcome.warnings,
                    worker,
                }),
            )
            .await
        }
        Err(error) => {
            use crate::server::attach_project::AttachError;
            match &error {
                AttachError::NotFound => session_not_found(),
                AttachError::TurnInFlight => {
                    api_error(StatusCode::CONFLICT, "turn_in_flight", error.to_string())
                }
                AttachError::Rejected(cause)
                    if cause.is::<crate::session::NativeStoreUnavailable>()
                        || cause.is::<crate::session::SessionCommitApplied>()
                        || cause
                            .is::<crate::session::attach_project::AttachRollbackIncomplete>() =>
                {
                    StatusCode::SERVICE_UNAVAILABLE.into_response()
                }
                AttachError::Rejected(cause)
                    if cause.is::<crate::session::attach_project::AttachPersistFailed>() =>
                {
                    api_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "persist_failed",
                        error.to_string(),
                    )
                }
                AttachError::Rejected(_) => {
                    api_error(StatusCode::BAD_REQUEST, "attach_failed", error.to_string())
                }
            }
        }
    }
}

/// Resolve the request's `project` field to a host path: an absolute path is
/// taken as-is, anything else is looked up in the project registry so the web
/// picker can send the name it displays.
async fn resolve_project_input(profile: &str, raw: &str) -> Result<std::path::PathBuf, String> {
    // `Path` here is axum's extractor, so std types are qualified.
    if std::path::Path::new(raw).is_absolute() {
        return Ok(std::path::PathBuf::from(raw));
    }
    // Path-shaped but not absolute. Without this the input falls through to the
    // registry lookup and reports a registry problem the user does not have.
    if raw.starts_with('~') || raw.contains('/') || raw.contains(std::path::MAIN_SEPARATOR) {
        return Err(format!(
            "'{raw}' looks like a path but is not absolute. Pass an absolute path, or the name of \
             a registered project."
        ));
    }
    let profile = profile.to_string();
    let name = raw.to_string();
    tokio::task::spawn_blocking(move || {
        crate::session::projects::resolve_names(&profile, &[name])
            .map_err(|e| format!("{e:#}"))
            .and_then(|projects| {
                projects
                    .into_iter()
                    .next()
                    .map(|p| std::path::PathBuf::from(p.path))
                    .ok_or_else(|| "Project not found in the registry".to_string())
            })
    })
    .await
    .map_err(|e| format!("project lookup panicked: {e}"))?
}
