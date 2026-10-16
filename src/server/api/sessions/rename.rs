//! Rename, worktree-edit, and attach-project endpoints.

use super::*;

// --- Rename session ---

#[derive(Deserialize)]
pub struct RenameSessionBody {
    pub title: String,
    /// When the session is tied (`session.tie_workdir_to_name`) and an
    /// aoe-managed worktree, also rename the git branch to match the new title.
    /// Ignored for untied / non-worktree sessions (#1927).
    #[serde(default)]
    pub rename_branch: bool,
}

pub(super) fn apply_session_title_rename(inst: &mut Instance, title: String) {
    inst.title = title;
}

/// Publish only the fields the rename transaction owns onto the cache row.
/// Identity fields it did not change are reconciled from the disk snapshot only
/// while the cache still matches the baseline captured at request start.
pub(super) struct SessionRenameCachePatch<'a> {
    pub(super) title: &'a str,
    pub(super) initial_path: &'a str,
    pub(super) initial_branch: Option<&'a str>,
    pub(super) authoritative_path: &'a str,
    pub(super) authoritative_branch: Option<&'a str>,
    pub(super) renamed_path: Option<&'a str>,
    pub(super) renamed_branch: Option<&'a str>,
}

/// Reconcile one identity field the rename does not own, returning the value to
/// write or `None` to keep the cached one.
///
/// `renamed` always wins. Otherwise the `authoritative` disk value is adopted
/// only while `cached` still equals the request-start `baseline`; if a watcher
/// or user action advanced it, the newer cached value survives. `path` and
/// `branch` share this rule.
fn reconcile_unowned_identity<'a>(
    cached: Option<&str>,
    baseline: Option<&str>,
    authoritative: Option<&'a str>,
    renamed: Option<&'a str>,
) -> Option<&'a str> {
    match renamed {
        Some(_) => renamed,
        None if cached == baseline => authoritative,
        None => None,
    }
}

pub(super) fn apply_session_rename_cache_patch(
    inst: &mut Instance,
    patch: SessionRenameCachePatch<'_>,
) {
    apply_session_title_rename(inst, patch.title.to_string());
    if let Some(path) = reconcile_unowned_identity(
        Some(inst.project_path.as_str()),
        Some(patch.initial_path),
        Some(patch.authoritative_path),
        patch.renamed_path,
    ) {
        apply_worktree_name_edit(inst, path, None);
    }
    let cached_branch = inst
        .worktree_info
        .as_ref()
        .map(|worktree| worktree.branch.as_str());
    let branch = reconcile_unowned_identity(
        cached_branch,
        patch.initial_branch,
        patch.authoritative_branch,
        patch.renamed_branch,
    );
    if let (Some(worktree), Some(branch)) = (inst.worktree_info.as_mut(), branch) {
        worktree.branch = branch.to_string();
    }
}

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

/// Rename a session's title (and, when tied, its worktree directory). The
/// sandbox container probe fails closed, so the rename is rejected with
/// `409 CONFLICT` rather than proceeding against a possibly-live mount.
pub async fn rename_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<RenameSessionBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(response) = crate::server::api::read_only_block(&state) {
        return response;
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    let title = body.title.trim().to_string();
    if title.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_title",
            "Title cannot be empty",
        );
    }
    if let Err(message) = validate_display_label(&title, "title") {
        return api_error(StatusCode::BAD_REQUEST, "invalid_title", message);
    }
    run_rename(
        state,
        id,
        crate::session::rename::Edit::Session {
            title: Some(title),
            group: None,
            profile: None,
            rename_branch: body.rename_branch,
            branch: None,
        },
    )
    .await
}

async fn run_rename(
    state: Arc<AppState>,
    id: String,
    edit: crate::session::rename::Edit,
) -> axum::response::Response {
    if let Some(response) = cityhall_block_non_structured(&state, &id).await {
        return response;
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
    let live = {
        let instances = state.instances.read().await;
        let Some(instance) = instances.iter().find(|instance| instance.id == id) else {
            return session_not_found();
        };
        instance.clone()
    };
    let request = crate::session::rename::Request {
        baseline: live,
        edit,
        file_watch: state.file_watch.clone(),
    };
    let worker_state = Arc::clone(&state);
    let runtime = tokio::runtime::Handle::current();
    let committed = match tokio::task::spawn_blocking(move || {
        crate::session::rename::perform(request, |original, moves| {
            if moves {
                // A structured worker retains its old cwd even while Idle (#2260).
                runtime
                    .block_on(quiesce_structured_worker_for_worktree_move(
                        &worker_state,
                        &original.id,
                        original.is_structured(),
                    ))
                    .map_err(|_| crate::session::rename::RenameRejected::WorkerShutdown)?;
            }
            Ok(())
        })
    })
    .await
    {
        Ok(Ok(committed)) => committed,
        Ok(Err(error)) => {
            tracing::warn!(target: "http.api.sessions", session = %id, "rename refused: {error:#}");
            if let Some(error) =
                error.downcast_ref::<crate::session::worktree_edit::WorktreeEditError>()
            {
                let (status, message) = worktree_edit_error_response(error);
                return api_error(status, "worktree_edit_failed", message);
            }
            if let Some(busy) = error.downcast_ref::<crate::session::LifecycleReservationError>() {
                return api_error(StatusCode::CONFLICT, "lifecycle_busy", busy.to_string());
            }
            if let Some(rejected) = error.downcast_ref::<crate::session::rename::RenameRejected>() {
                use crate::session::rename::RenameRejected;
                let (status, code) = match rejected {
                    RenameRejected::Missing => return session_not_found(),
                    RenameRejected::Active => (StatusCode::CONFLICT, "session_running"),
                    RenameRejected::NotWorktree => (StatusCode::BAD_REQUEST, "not_worktree"),
                    RenameRejected::Tied => (StatusCode::CONFLICT, "tied"),
                    RenameRejected::WorkerShutdown => {
                        (StatusCode::CONFLICT, "worker_shutdown_failed")
                    }
                    RenameRejected::Duplicate(_) => (StatusCode::CONFLICT, "duplicate_session"),
                };
                return api_error(status, code, rejected.to_string());
            }
            return api_error(
                StatusCode::CONFLICT,
                "session_changed",
                "Could not complete the rename. Completed Git effects may still roll forward during recovery; inspect and reload the session before retrying",
            );
        }
        Err(error) => {
            tracing::error!(target: "http.api.sessions", session = %id, %error, "rename worker failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let tied = committed.instance.tie_workdir_applies(
        crate::session::config::profile_config::resolve_config_or_warn(
            &committed.instance.source_profile,
        )
        .session
        .tie_workdir_to_name,
    );
    let mut instances = state.instances.write().await;
    let Some(instance) = instances
        .iter_mut()
        .find(|instance| instance.id == id && instance.created_at == committed.instance.created_at)
    else {
        return session_not_found();
    };
    if instance.lifecycle_generation <= committed.instance.lifecycle_generation
        && instance.source_profile == committed.original.source_profile
    {
        apply_session_rename_cache_patch(
            instance,
            SessionRenameCachePatch {
                title: &committed.instance.title,
                initial_path: &committed.baseline.project_path,
                initial_branch: committed
                    .baseline
                    .worktree_info
                    .as_ref()
                    .map(|info| info.branch.as_str()),
                authoritative_path: &committed.instance.project_path,
                authoritative_branch: committed
                    .instance
                    .worktree_info
                    .as_ref()
                    .map(|info| info.branch.as_str()),
                renamed_path: (committed.original.project_path != committed.instance.project_path)
                    .then_some(committed.instance.project_path.as_str()),
                renamed_branch: committed
                    .instance
                    .worktree_info
                    .as_ref()
                    .filter(|info| {
                        committed
                            .original
                            .worktree_info
                            .as_ref()
                            .is_some_and(|old| old.branch != info.branch)
                    })
                    .map(|info| info.branch.as_str()),
            },
        );
        instance.lifecycle_generation = committed.instance.lifecycle_generation;
        instance.lifecycle_reservation = committed.instance.lifecycle_reservation.clone();
    }
    let mut response =
        SessionResponse::from_instance(&*instance, crate::claude_settings::read_tui_fullscreen());
    response.tie_workdir_to_name = tied;
    if let Some(warning) = committed.warning {
        response.warnings.push(warning);
    }
    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

// --- Edit worktree workdir name ---

#[derive(Deserialize)]
pub struct SetWorktreeNameBody {
    pub name: String,
    /// Also rename the git branch to match. Off by default: the session may
    /// have done meaningful work on its branch already.
    #[serde(default)]
    pub rename_branch: bool,
}

/// Map a worktree-edit failure to an HTTP status and client-safe message.
/// Validation failures are 400/409; git/IO failures stay generic, since raw git
/// stderr and IO paths must not reach the wire.
fn worktree_edit_error_response(
    e: &crate::session::worktree_edit::WorktreeEditError,
) -> (StatusCode, String) {
    use crate::session::worktree_edit::WorktreeEditError as E;
    match e {
        E::Ownership(_) => (
            StatusCode::CONFLICT,
            "Another session uses this checkout or branch, or its owners could not be checked"
                .to_string(),
        ),
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
    }
}

/// Edit a managed worktree session's workdir directory name, and optionally its
/// git branch. The sandbox container gate fails closed, so the edit is rejected
/// with `409 CONFLICT` rather than proceeding against a possibly-live mount.
pub async fn set_worktree_name(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<SetWorktreeNameBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(response) = crate::server::api::read_only_block(&state) {
        return response;
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_name",
            "Workdir name cannot be empty",
        );
    }
    run_rename(
        state,
        id,
        crate::session::rename::Edit::Workdir {
            name,
            rename_branch: body.rename_branch,
        },
    )
    .await
}

// --- Attach a project to an existing session (#3103) ---

#[derive(Deserialize)]
pub struct AttachProjectBody {
    /// Absolute host path of the repo to attach, or the name of a registered
    /// project, resolved against the project registry.
    pub project: String,
    /// Check out a branch that already exists in the added repo instead of
    /// refusing. Off by default: a same-named branch elsewhere can hold
    /// unrelated commits. Setting it records the branch as not aoe-created, so
    /// deleting the session leaves it alone.
    #[serde(default)]
    pub attach_existing_branch: bool,
}

/// `POST /api/sessions/:id/projects`. Attaches a repo to an existing session,
/// converting it into a multi-repo workspace and restarting it so the agent
/// comes up there with its transcript intact.
///
/// Attaching moves the directory, which the workdir endpoint refuses while
/// active (#2260). Rather than gut the feature, this stops the session for the
/// move and starts it again (#2346). Mid-turn is still refused with 409.
pub async fn attach_session_project(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<AttachProjectBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    // Defense in depth behind `cityhall_gate`, which already denies this route:
    // attaching takes an arbitrary host path, so it is classified with
    // `git/clone` rather than with the session lifecycle routes CityHall allows.
    if let Some(resp) = crate::server::api::cityhall_block(&state) {
        return resp;
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    let raw = body.project.trim().to_string();
    if raw.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "message": "Project path or name is required" })),
        )
            .into_response();
    }

    let profile = {
        let instances = state.instances.read().await;
        match instances.iter().find(|i| i.id == id) {
            Some(inst) => inst.source_profile.clone(),
            None => return session_not_found(),
        }
    };

    // A bare name is a registry lookup; anything path-shaped is used as-is, so
    // the API stays usable by hand.
    let repo_path = match resolve_project_input(&profile, &raw).await {
        Ok(p) => p,
        Err(message) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "message": message })),
            )
                .into_response();
        }
    };

    let on_existing = if body.attach_existing_branch {
        crate::session::attach_project::ExistingBranch::Attach
    } else {
        crate::session::attach_project::ExistingBranch::Refuse
    };

    match crate::server::attach_project::attach_project(&state, &id, &repo_path, on_existing).await
    {
        Ok((outcome, worker)) => {
            use crate::server::attach_project::WorkerOutcome;
            let (worker_status, worker_message) = match &worker {
                WorkerOutcome::Restarted => ("restarted", None),
                WorkerOutcome::NotRunning => ("not_running", None),
                WorkerOutcome::RestartFailed(m) => ("restart_failed", Some(m.clone())),
            };
            let response = {
                let instances = state.instances.read().await;
                instances.iter().find(|i| i.id == id).map(|inst| {
                    SessionResponse::from_instance(
                        inst,
                        crate::claude_settings::read_tui_fullscreen(),
                    )
                })
            };
            // 200 even on RestartFailed: the attachment itself succeeded and is
            // durable, and the client renders the worker status, so reporting a
            // total failure would be wrong.
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "session": response,
                    "attached": {
                        "name": outcome.repo.name,
                        "worktree_path": outcome.repo.worktree_path,
                        "branch": outcome.repo.branch,
                        "branch_created": !outcome.repo.branch_preexisting,
                        "moved_to": outcome.moved_to,
                    },
                    "warnings": outcome.warnings,
                    "worker": worker_status,
                    "worker_message": worker_message,
                })),
            )
                .into_response()
        }
        Err(e) => {
            use crate::server::attach_project::AttachError;
            let status = match &e {
                AttachError::NotFound => StatusCode::NOT_FOUND,
                AttachError::TurnInFlight => StatusCode::CONFLICT,
                AttachError::Rejected(_) => StatusCode::BAD_REQUEST,
            };
            (
                status,
                Json(serde_json::json!({ "message": e.to_string() })),
            )
                .into_response()
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

pub(super) fn apply_worktree_name_edit(
    inst: &mut Instance,
    new_path: &str,
    new_branch: Option<&str>,
) {
    inst.project_path = new_path.to_string();
    if let Some(branch) = new_branch {
        if let Some(wt) = inst.worktree_info.as_mut() {
            wt.branch = branch.to_string();
        }
    }
}
