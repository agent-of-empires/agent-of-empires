//! Rename, worktree-edit, and attach-project endpoints.

use super::*;

// --- Rename session ---

#[derive(Deserialize)]
pub struct RenameSessionBody {
    pub title: String,
    /// When the session is tied (`session.tie_workdir_to_name`) and an
    /// aoe-managed worktree, also rename the underlying git branch to match
    /// the new title. Off by default; ignored for untied / non-worktree
    /// sessions. See #1927.
    #[serde(default)]
    pub rename_branch: bool,
}

pub(super) fn apply_session_title_rename(inst: &mut Instance, title: String) {
    inst.title = title;
}

/// Publish only fields owned by the rename transaction onto the current cache
/// row. Watchers and user actions may have advanced every other field while the
/// blocking git and storage work ran. Identity fields that the rename did not
/// change are reconciled from the authoritative disk snapshot only while the
/// cache still matches the live baseline captured at the start of the request.
pub(super) struct SessionRenameCachePatch<'a> {
    pub(super) title: &'a str,
    pub(super) initial_path: &'a str,
    pub(super) initial_branch: Option<&'a str>,
    pub(super) authoritative_path: &'a str,
    pub(super) authoritative_branch: Option<&'a str>,
    pub(super) renamed_path: Option<&'a str>,
    pub(super) renamed_branch: Option<&'a str>,
}

/// Reconcile one identity field the rename transaction does not own, returning
/// the value to write or `None` to keep the current cached value.
///
/// `renamed` is `Some` when the rename explicitly changed the field and always
/// wins. Otherwise the field is adopted from the `authoritative` disk snapshot
/// only while the live `cached` value still equals the `baseline` captured at
/// the start of the request; if a watcher or user action advanced it since,
/// `None` is returned so the newer cached value survives. `path` and `branch`
/// share this exact rule, so both route through here.
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
    inst.title = patch.title.to_string();
    if let Some(path) = reconcile_unowned_identity(
        Some(inst.project_path.as_str()),
        Some(patch.initial_path),
        Some(patch.authoritative_path),
        patch.renamed_path,
    ) {
        inst.project_path = path.to_string();
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

/// Quiesce a structured-view worker before its worktree directory is moved.
/// A live ACP worker is pinned to the current cwd; `git worktree move` pulls
/// that directory out, the worker crashes, and the supervisor respawns it at
/// the stale baked-in cwd, crash-looping until the reconciler parks the
/// session with a misleading install-the-adapter banner (#2260). The
/// blocks_worktree_edit gate does not catch this because a structured session
/// the user "stopped" sits at Idle yet still owns a live worker.
///
/// `shutdown` is the reversible teardown: it keeps the agent transcript and the
/// instance's acp_session_id, so once the move lands the reconciler fresh-spawns
/// at the new path and resumes context via session/load. Callers hold the
/// session's instance_lock across shutdown plus move plus persist, and the
/// reconciler re-reads project_path under that same lock, so the post-move
/// respawn never targets the old path. No-op for a session with no live worker;
/// refuses the move (409) if a live worker cannot be stopped, so the directory
/// is never moved out from under one.
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
            Err((
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "worker_shutdown_failed",
                    "message": "Could not stop the structured view worker before renaming; retry in a moment"
                })),
            )
                .into_response())
        }
    }
}

/// Release a sandboxed session's hold on its worktree mount ahead of a
/// `git worktree move`, on the blocking pool, and report whether the
/// worktree is *still* held.
///
/// NOT a read-only probe: for a container that is merely stopped this
/// removes it, because a surviving container keeps pinning the bind mount
/// and the rename would fail. Only call it on a path that is about to
/// perform the move. See `ensure_sandbox_container_released` for the
/// running-vs-stopped split.
///
/// Fails closed at the async boundary: a `spawn_blocking` panic or
/// cancellation reports the worktree as held (with a `warn!` log), so
/// the caller rejects the mutating request with `409 CONFLICT` rather
/// than risk renaming against a possibly-live container mount. Sharing
/// this helper between `rename_session` and `set_worktree_name` keeps
/// the fail-closed policy synchronized across the two endpoints (#2596).
async fn ensure_sandbox_container_released_blocking(id: &str, is_sandboxed: bool) -> bool {
    let probe_id = id.to_string();
    let log_id = id.to_string();
    tokio::task::spawn_blocking(move || {
        crate::session::worktree_edit::ensure_sandbox_container_released(&probe_id, is_sandboxed)
    })
    .await
    .unwrap_or_else(|e| {
        tracing::warn!(
            target: "server.api.sessions",
            session = %log_id,
            error = %e,
            "sandbox container release task failed at the async boundary; failing closed and reporting the worktree as held rather than renaming against a possibly-live container mount"
        );
        true
    })
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum RenamePersistOutcome {
    Updated { old_title: String },
    Missing,
}

pub(super) fn persist_rename_metadata(
    storage: &Storage,
    id: &str,
    title: &str,
    new_path: Option<&str>,
    new_branch: Option<&str>,
) -> anyhow::Result<RenamePersistOutcome> {
    storage.update(|instances, _groups| {
        let Some(inst) = instances.iter_mut().find(|instance| instance.id == id) else {
            return Ok(RenamePersistOutcome::Missing);
        };
        let old_title = inst.title.clone();
        if let Some(path) = new_path {
            apply_worktree_name_edit(inst, path, new_branch);
        }
        apply_session_title_rename(inst, title.to_string());
        Ok(RenamePersistOutcome::Updated { old_title })
    })
}

/// Rename a session's title (and, when tied, its worktree directory).
///
/// The sandbox container probe runs on the blocking pool via
/// `ensure_sandbox_container_released_blocking`, which fails closed on a
/// `spawn_blocking` panic or cancellation so the rename is rejected
/// with `409 CONFLICT` rather than proceeding against a possibly-live
/// container mount.
pub async fn rename_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<RenameSessionBody>, axum::extract::rejection::JsonRejection>,
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
    let title = body.title.trim().to_string();
    if title.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "message": "Title cannot be empty" })),
        )
            .into_response();
    }
    if let Err(msg) = validate_display_label(&title, "title") {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "message": msg })),
        )
            .into_response();
    }

    // Serialize against other mutations on this session (start, delete,
    // worktree edit) so the tied git move and the metadata write don't race.
    // Prompt submission has its own authority and never takes `instance_lock`
    // (#3621), so hold that one too or a queue drain lands a follow-up on the
    // worker this quiesces for the move. Submission first, as it documents,
    // and via the admission form so an unknown id allocates neither lock.
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return crate::server::api::session_not_found();
    };
    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    let live = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return crate::server::api::session_not_found();
        };
        inst.clone()
    };
    let profile = live.source_profile.clone();
    // App-wide and per-session flocks may wait on another process, so never
    // acquire them on a Tokio worker. Identity nests outside session title,
    // source lifecycle, and profile Storage.
    let _identity_lock = match tokio::task::spawn_blocking(
        crate::session::acquire_session_identity_lock,
    )
    .await
    {
        Ok(Ok(lock)) => lock,
        Ok(Err(error)) => {
            tracing::error!(target: "http.api.sessions", session = %id, %error, "Failed to acquire session identity lock");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        Err(error) => {
            tracing::error!(target: "http.api.sessions", session = %id, %error, "Session identity lock task failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let lock_id = id.clone();
    let lock_profile = profile.clone();
    let lock_file_watch = state.file_watch.clone();
    let (_session_title_lock, _lifecycle_lock, storage, disk_instances) =
        match tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let session_title_lock = crate::session::acquire_session_title_lock(&lock_id)?;
            let storage = Storage::new(&lock_profile, lock_file_watch)?;
            let lifecycle_lock = storage.acquire_instance_lifecycle_lock(&lock_id)?;
            let instances = storage.load()?;
            Ok((session_title_lock, lifecycle_lock, storage, instances))
        })
        .await
        {
            Ok(Ok(locks)) => locks,
            Ok(Err(error)) => {
                tracing::error!(target: "http.api.sessions", session = %id, "failed to acquire rename locks or load authoritative state: {error}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": "title_lock_failed",
                        "message": "Could not serialize the session rename"
                    })),
                )
                    .into_response();
            }
            Err(error) => {
                tracing::error!(target: "http.api.sessions", session = %id, "rename lock task failed: {error}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": "title_lock_failed",
                        "message": "Could not serialize the session rename"
                    })),
                )
                    .into_response();
            }
        };
    let Some(mut fresh) = disk_instances
        .iter()
        .find(|instance| instance.id == id)
        .cloned()
    else {
        return crate::server::api::session_not_found();
    };
    fresh.source_profile.clone_from(&profile);
    fresh.merge_runtime_from_reload(&live);
    let current_title = fresh.title.clone();
    let worktree_info = fresh.worktree_info.clone();
    let current_path = fresh.project_path.clone();
    let current_branch = worktree_info
        .as_ref()
        .map(|worktree| worktree.branch.clone());
    let status = fresh.status;
    let is_sandboxed = fresh.is_sandboxed();
    let is_structured = fresh.is_structured();

    // Tied mode (#1927): renaming an aoe-managed worktree session also moves
    // its directory leaf to match the title, so title and dir cannot drift.
    let tied = fresh.tie_workdir_applies(
        crate::session::config::profile_config::resolve_config_or_warn(&profile)
            .session
            .tie_workdir_to_name,
    );
    let duplicate_path = if tied {
        crate::session::worktree_edit::derived_worktree_path(
            std::path::Path::new(&current_path),
            &title,
        )
    } else {
        current_path.clone()
    };
    let pair_changed = title != current_title
        || duplicate_path.trim_end_matches('/') != current_path.trim_end_matches('/');
    if pair_changed
        && is_duplicate_session(disk_instances.iter(), &title, &duplicate_path, Some(&id))
    {
        let message = duplicate_session_error(&title).to_string();
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "duplicate_session",
                "message": message,
            })),
        )
            .into_response();
    }

    // What to write to disk + memory once any git side effect has landed.
    let mut new_path: Option<String> = None;
    let mut new_branch: Option<String> = None;

    if tied {
        // A directory move or branch rename is gated on a quiescent worktree,
        // exactly like the standalone worktree-name edit. A running session
        // must be stopped first; the setting is the escape hatch for
        // free-form relabeling.
        //
        // A sandbox session's container keeps the worktree dir mounted even
        // while the agent is Idle, so a directory move would fail. The helper
        // drops a merely-stopped container to free the mount and only reports
        // held for a live one, which the user has to stop.
        //
        // Short-circuited twice, because the helper removes a stopped
        // container: once on the status check, so a request about to be
        // rejected never discards, and once on whether the directory is
        // actually going to move, so a no-op or branch-only rename does not
        // either.
        let leaf = crate::session::worktree_edit::worktree_leaf_from_title(&title);
        let moves_worktree = crate::session::worktree_edit::worktree_move_required(
            std::path::Path::new(&current_path),
            &leaf,
        );
        let renames_branch = worktree_info.as_ref().is_some_and(|wt| {
            crate::session::worktree_edit::worktree_branch_rename_required(
                wt,
                &leaf,
                body.rename_branch,
            )
        });
        let container_holds = !status.blocks_worktree_edit()
            && moves_worktree
            && ensure_sandbox_container_released_blocking(&id, is_sandboxed).await;
        if (moves_worktree || renames_branch) && (status.blocks_worktree_edit() || container_holds)
        {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "session_running",
                    "message": "Stop the session before renaming its worktree directory or branch. Disable \"Tie Worktree Directory to Session Name\" to relabel a running session."
                })),
            )
                .into_response();
        }

        // Stop a live structured-view worker only when its cwd will move. A
        // title-only or branch-only edit leaves the cwd valid and must not
        // interrupt the worker.
        if moves_worktree {
            if let Err(response) =
                quiesce_structured_worker_for_worktree_move(&state, &id, is_structured).await
            {
                return response;
            }
        }

        let wt = worktree_info.expect("tied implies worktree_info is Some");
        let cur = current_path.clone();
        let rename_branch = body.rename_branch;
        let edit = tokio::task::spawn_blocking(move || {
            crate::session::worktree_edit::edit_worktree_workdir(
                crate::session::worktree_edit::WorktreeEditRequest {
                    worktree_info: &wt,
                    current_path: std::path::Path::new(&cur),
                    new_name: &leaf,
                    rename_branch,
                },
            )
            .map(|o| (o.new_path.to_string_lossy().to_string(), o.new_branch))
        })
        .await;

        match edit {
            Ok(Ok((path, branch))) => {
                // The dir moved (path changed): a sandbox container created
                // against the old path is now stale, so drop it to force a
                // fresh create on next start. A branch-only edit leaves the
                // path (and the mount) unchanged, so skip it then. Awaited so
                // the response only lands once the stale container is gone; an
                // immediate restart must not race the removal and revive it.
                if path != current_path {
                    let id = id.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        crate::session::worktree_edit::discard_sandbox_container_after_move(
                            &id,
                            is_sandboxed,
                        )
                    })
                    .await;
                }
                new_path = Some(path);
                new_branch = branch;
            }
            // The title slug maps to the current leaf and no branch rename was
            // requested: nothing to move, fall through to a plain title rename.
            Ok(Err(crate::session::worktree_edit::WorktreeEditError::Unchanged)) => {}
            Ok(Err(e)) => {
                tracing::warn!(target: "http.api.sessions", session = %id, "tied rename worktree edit failed: {e}");
                let (code, msg) = worktree_edit_error_response(&e);
                return (code, Json(serde_json::json!({ "message": msg }))).into_response();
            }
            Err(e) => {
                tracing::error!(target: "http.api.sessions", "tied rename worktree edit join failed: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "message": "Worktree edit task failed" })),
                )
                    .into_response();
            }
        }
    }

    // Persist BEFORE mutating in-memory state: when a git move has landed, a
    // silent persist failure would otherwise leave metadata pointing at the
    // old path after a daemon restart, so it returns 500 rather than a
    // misleading 200.
    let title_clone = title.clone();
    let id_clone = id.clone();
    let new_path_clone = new_path.clone();
    let new_branch_clone = new_branch.clone();
    let persisted = tokio::task::spawn_blocking(move || {
        persist_rename_metadata(
            &storage,
            &id_clone,
            &title_clone,
            new_path_clone.as_deref(),
            new_branch_clone.as_deref(),
        )
    })
    .await
    .map_err(|error| error.to_string())
    .and_then(|result| result.map_err(|error| error.to_string()));
    let persisted_old_title = match persisted {
        Ok(RenamePersistOutcome::Updated { old_title }) => old_title,
        Ok(RenamePersistOutcome::Missing) => {
            // AppState can lag an external delete. A missing authoritative row
            // is not a successful rename and must not trigger tmux/cache work.
            if let Some(path) = new_path.as_deref() {
                tracing::warn!(
                    target: "http.api.sessions",
                    session = %id,
                    new_path = %path,
                    "authoritative row vanished after the worktree move; the moved directory is unreferenced"
                );
            }
            return crate::server::api::session_not_found();
        }
        Err(error) => {
            tracing::error!(target: "http.api.sessions", session = %id, "Failed to save after rename: {error}");
            // Persist-first: never fall through to mutate in-memory state on a
            // failed write, or the rename silently reverts on restart. When a
            // dir move already landed, say so; otherwise it is a plain title
            // persist.
            let message = if new_path.is_some() {
                "Worktree was moved on disk, but persisting the new session metadata failed"
            } else {
                "Persisting the renamed session failed"
            };
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": "persist_failed", "message": message })),
            )
                .into_response();
        }
    };

    let published_path = new_path.as_deref().unwrap_or(&current_path);
    let renamed_path = new_path
        .as_deref()
        .filter(|path| *path != current_path.as_str());
    let published_branch = new_branch.as_deref().or(current_branch.as_deref());
    let renamed_branch = new_branch
        .as_deref()
        .filter(|branch| current_branch.as_deref() != Some(*branch));
    let initial_branch = live
        .worktree_info
        .as_ref()
        .map(|worktree| worktree.branch.as_str());
    let mut response = {
        let mut instances = state.instances.write().await;
        let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
            return crate::server::api::session_not_found();
        };
        apply_session_rename_cache_patch(
            inst,
            SessionRenameCachePatch {
                title: &title,
                initial_path: &live.project_path,
                initial_branch,
                authoritative_path: published_path,
                authoritative_branch: published_branch,
                renamed_path,
                renamed_branch,
            },
        );
        SessionResponse::from_instance(&*inst, crate::claude_settings::read_tui_fullscreen())
    };
    // Single-session responses are not run through list_sessions' overlay, so
    // carry the resolved tie value here too (#1927); otherwise a client that
    // trusts the mutation response would see a managed worktree claim it is
    // untied until the next list refresh.
    response.tie_workdir_to_name = tied;
    drop(_identity_lock);

    let tmux_warning = if persisted_old_title != title && !is_structured {
        let rekey_id = id.clone();
        let rekey_old_title = persisted_old_title.clone();
        let rekey_new_title = title.clone();
        match tokio::task::spawn_blocking(move || {
            crate::tmux::rekey_session(&rekey_id, &rekey_old_title, &rekey_new_title)
        })
        .await
        {
            Ok(Ok(_)) => None,
            Ok(Err(error)) => {
                tracing::warn!(target: "http.api.sessions", session = %id, "tmux rename failed after persistence: {error}");
                Some(format!(
                    "Session metadata was renamed, but its live tmux session could not be rekeyed: {error}"
                ))
            }
            Err(error) => {
                tracing::warn!(target: "http.api.sessions", session = %id, "tmux rename task failed after persistence: {error}");
                Some(format!(
                    "Session metadata was renamed, but its live tmux session could not be rekeyed: {error}"
                ))
            }
        }
    } else {
        None
    };
    if let Some(warning) = tmux_warning {
        response.warnings.push(warning);
    }

    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

// --- Edit worktree workdir name ---

#[derive(Deserialize)]
pub struct SetWorktreeNameBody {
    pub name: String,
    /// Also rename the underlying git branch to match. Off by default: the
    /// session may have done meaningful work on its branch already.
    #[serde(default)]
    pub rename_branch: bool,
}

/// Map a worktree-edit failure to an HTTP status + client-safe message.
/// Validation failures are 400/409; git/IO failures stay generic (raw git
/// stderr and IO paths must not reach the wire).
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

/// Edit a managed worktree session's workdir directory name (and optionally
/// its git branch).
///
/// The sandbox container gate runs on the blocking pool via
/// `ensure_sandbox_container_released_blocking`, which fails closed on a
/// `spawn_blocking` panic or cancellation so the edit is rejected with
/// `409 CONFLICT` rather than proceeding against a possibly-live container
/// mount.
pub async fn set_worktree_name(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<SetWorktreeNameBody>, axum::extract::rejection::JsonRejection>,
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
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "message": "Workdir name cannot be empty" })),
        )
            .into_response();
    }
    // #2624: no shell-injection check here. `name` becomes a git branch and
    // filesystem leaf via `edit_worktree_workdir`, which already runs it
    // through `git_sanitize_branch_name` + `sanitize_branch_name` before
    // either ever sees a raw byte (src/session/worktree_edit.rs).

    // Serialize against other mutations on this session (start, delete,
    // another rename) so the git ops and the metadata write don't race.
    // Prompt submission has its own authority and never takes `instance_lock`
    // (#3621), so hold that one too or a queue drain lands a follow-up on the
    // worker this quiesces for the move. Submission first, as it documents,
    // and via the admission form so an unknown id allocates neither lock.
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return crate::server::api::session_not_found();
    };
    let lock = state.instance_lock(&id).await;
    let _guard = lock.lock().await;

    let live = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return crate::server::api::session_not_found();
        };
        inst.clone()
    };
    let profile = live.source_profile.clone();
    let _identity_lock = match tokio::task::spawn_blocking(
        crate::session::acquire_session_identity_lock,
    )
    .await
    {
        Ok(Ok(lock)) => lock,
        Ok(Err(error)) => {
            tracing::error!(target: "http.api.sessions", session = %id, %error, "Failed to acquire worktree identity lock");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        Err(error) => {
            tracing::error!(target: "http.api.sessions", session = %id, %error, "Worktree identity lock task failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let lock_id = id.clone();
    let lock_profile = profile.clone();
    let lock_file_watch = state.file_watch.clone();
    let (_lifecycle_lock, storage, authoritative_instances) = match tokio::task::spawn_blocking(
        move || -> anyhow::Result<_> {
            let storage = Storage::new(&lock_profile, lock_file_watch)?;
            let lifecycle = storage.acquire_instance_lifecycle_lock(&lock_id)?;
            let instances = storage.load()?;
            Ok((lifecycle, storage, instances))
        },
    )
    .await
    {
        Ok(Ok(locked)) => locked,
        Ok(Err(error)) => {
            tracing::error!(target: "http.api.sessions", session = %id, %error, "Failed to lock or load worktree rename");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
        Err(error) => {
            tracing::error!(target: "http.api.sessions", session = %id, %error, "Worktree rename lock task failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let Some(mut fresh) = authoritative_instances
        .iter()
        .find(|instance| instance.id == id)
        .cloned()
    else {
        return crate::server::api::session_not_found();
    };
    fresh.source_profile.clone_from(&profile);
    fresh.merge_runtime_from_reload(&live);
    let worktree_info = fresh.worktree_info.clone();
    let current_path = fresh.project_path.clone();
    let status = fresh.status;
    let is_sandboxed = fresh.is_sandboxed();
    let is_structured = fresh.is_structured();

    let Some(worktree_info) = worktree_info else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "message": "Session does not use a worktree" })),
        )
            .into_response();
    };
    // When tied (#1927), the directory is not edited independently: it follows
    // the title. Reject the standalone edit so no client can drift the two
    // apart, pointing callers at the unified rename.
    if worktree_info.managed_by_aoe
        && crate::session::config::profile_config::resolve_config_or_warn(&profile)
            .session
            .tie_workdir_to_name
    {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "tied",
                "message": "Renaming is unified while \"Tie Worktree Directory to Session Name\" is on; rename the session instead, and its directory follows."
            })),
        )
            .into_response();
    }
    let duplicate_path = crate::session::worktree_edit::target_worktree_path(
        std::path::Path::new(&current_path),
        &name,
    )
    .unwrap_or_else(|| std::path::PathBuf::from(&current_path))
    .to_string_lossy()
    .into_owned();
    if duplicate_path.trim_end_matches('/') != current_path.trim_end_matches('/')
        && is_duplicate_session(
            authoritative_instances.iter(),
            &fresh.title,
            &duplicate_path,
            Some(&id),
        )
    {
        let message = duplicate_session_error(&fresh.title).to_string();
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "duplicate_session",
                "message": message,
            })),
        )
            .into_response();
    }
    // A sandbox container keeps the worktree dir mounted even while the agent
    // is Idle, so the move would fail. The helper drops a merely-stopped
    // container to free the mount and only reports held for a live one, which
    // the user has to stop, same as the active-status case.
    // Short-circuited twice, because the helper removes a stopped container:
    // once on the status check, so a request about to be rejected never
    // discards, and once on whether the directory is actually going to move, so
    // a no-op or branch-only edit does not either.
    let moves_worktree = crate::session::worktree_edit::worktree_move_required(
        std::path::Path::new(&current_path),
        &name,
    );
    let container_holds = !status.blocks_worktree_edit()
        && moves_worktree
        && ensure_sandbox_container_released_blocking(&id, is_sandboxed).await;
    if status.blocks_worktree_edit() || container_holds {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "message": "Cannot edit the workdir name while the session is active; stop it first"
            })),
        )
            .into_response();
    }

    // Stop any live structured-view worker before the move so it can't crash on
    // the pulled-out cwd and respawn-loop at the stale path (#2260). Held under
    // the instance_lock acquired at the top of this function. Gated on
    // `moves_worktree` for the same reason as the tied `rename_session` path: a
    // branch-only edit (name unchanged, `rename_branch` set) leaves the cwd
    // valid, so interrupting the worker would be a needless respawn. When the
    // name is unchanged and no branch rename is requested, `edit_worktree_workdir`
    // rejects with `Unchanged` below and nothing is touched either way.
    if moves_worktree {
        if let Err(resp) =
            quiesce_structured_worker_for_worktree_move(&state, &id, is_structured).await
        {
            return resp;
        }
    }

    let wt = worktree_info.clone();
    let cur = current_path.clone();
    let new_name = name.clone();
    let rename_branch = body.rename_branch;
    let edit = tokio::task::spawn_blocking(move || {
        crate::session::worktree_edit::edit_worktree_workdir(
            crate::session::worktree_edit::WorktreeEditRequest {
                worktree_info: &wt,
                current_path: std::path::Path::new(&cur),
                new_name: &new_name,
                rename_branch,
            },
        )
        .map(|o| (o.new_path.to_string_lossy().to_string(), o.new_branch))
    })
    .await;

    let (new_path, new_branch) = match edit {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => {
            tracing::warn!(target: "http.api.sessions", session = %id, "worktree edit failed: {e}");
            let (code, msg) = worktree_edit_error_response(&e);
            return (code, Json(serde_json::json!({ "message": msg }))).into_response();
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "worktree edit join failed: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "message": "Worktree edit task failed" })),
            )
                .into_response();
        }
    };

    // The dir moved (path changed): a sandbox container created against the old
    // path is now stale, so drop it to force a fresh create on next start. A
    // branch-only edit leaves the path (and the mount) unchanged. Awaited so
    // the response only lands once the stale container is gone; an immediate
    // restart must not race the removal and revive it.
    if new_path != current_path {
        let id_for_discard = id.clone();
        let _ = tokio::task::spawn_blocking(move || {
            crate::session::worktree_edit::discard_sandbox_container_after_move(
                &id_for_discard,
                is_sandboxed,
            )
        })
        .await;
    }

    // The git move has already landed, so persist to disk BEFORE mutating
    // in-memory state. A silent persist failure here would leave stale
    // metadata that points at the old (now-moved) path after a daemon
    // restart, so any failure returns 500 instead of a misleading 200.
    let persist_failed = || {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": "persist_failed",
                "message": "Worktree was moved on disk, but persisting the new session metadata failed"
            })),
        )
            .into_response()
    };

    let id_clone = id.clone();
    let new_path_clone = new_path.clone();
    let new_branch_clone = new_branch.clone();
    match tokio::task::spawn_blocking(move || {
        storage.update(|instances, _groups| {
            let Some(inst) = instances.iter_mut().find(|i| i.id == id_clone) else {
                return Ok(false);
            };
            apply_worktree_name_edit(inst, &new_path_clone, new_branch_clone.as_deref());
            Ok(true)
        })
    })
    .await
    {
        Ok(Ok(true)) => {}
        Ok(Ok(false)) => {
            tracing::warn!(
                target: "http.api.sessions",
                session = %id,
                new_path = %new_path,
                "authoritative row vanished after the worktree move; the moved directory is unreferenced"
            );
            return crate::server::api::session_not_found();
        }
        Ok(Err(e)) => {
            tracing::error!(target: "http.api.sessions", "Failed to save after worktree edit: {e}");
            return persist_failed();
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions", "Worktree edit persist join failed: {e}");
            return persist_failed();
        }
    }

    let response = {
        let mut instances = state.instances.write().await;
        let Some(inst) = instances.iter_mut().find(|i| i.id == id) else {
            return crate::server::api::session_not_found();
        };
        apply_worktree_name_edit(inst, &new_path, new_branch.as_deref());
        SessionResponse::from_instance(&*inst, crate::claude_settings::read_tui_fullscreen())
    };
    drop(_identity_lock);

    (StatusCode::OK, Json(serde_json::json!(response))).into_response()
}

// --- Attach a project to an existing session (#3103) ---

#[derive(Deserialize)]
pub struct AttachProjectBody {
    /// Absolute host path of the repo to attach, or the name of a registered
    /// project. A name is resolved against the project registry.
    pub project: String,
    /// Check out a branch that already exists in the added repo instead of
    /// refusing. Off by default: a same-named branch in another repo can hold
    /// unrelated commits, and checking it out would feed the agent the wrong
    /// tree. Setting this records the branch as not aoe-created, so deleting the
    /// session leaves it alone.
    #[serde(default)]
    pub attach_existing_branch: bool,
}

/// `POST /api/sessions/:id/projects`. Attaches a repo to a session that already
/// exists, converting it into a multi-repo workspace and restarting it so the
/// agent comes up there with its transcript intact.
///
/// Modelled on the workdir endpoint, which refuses while the session is active
/// because it moves the directory out from under a live worker (#2260).
/// Attaching moves it too, so rather than refuse (which would gut the feature)
/// this stops the session for the move and starts it again, which is what #2346
/// asks for. Mid-turn is still refused, with 409.
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
    // `git/clone` and `POST /api/projects` rather than with the session lifecycle
    // routes CityHall mode allows.
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
            None => return crate::server::api::session_not_found(),
        }
    };

    // A bare name is a registry lookup; anything path-shaped is used as-is. The
    // registry is what the picker offers, so this keeps the API usable by hand
    // without making the caller resolve names itself.
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
            // durable. The client renders the worker status so the user can see
            // the agent needs a restart rather than being told the whole
            // operation failed and left nothing behind.
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

/// Resolve the request's `project` field to a host path.
///
/// An absolute path is taken as-is. Anything else is looked up in the project
/// registry, so the web picker can send the name it already displays.
async fn resolve_project_input(profile: &str, raw: &str) -> Result<std::path::PathBuf, String> {
    // `Path` in this module is axum's extractor, so the std types are qualified.
    if std::path::Path::new(raw).is_absolute() {
        return Ok(std::path::PathBuf::from(raw));
    }
    // Path-shaped but not absolute. Without this the input falls through to the
    // registry lookup and comes back as "not in the registry", sending the user
    // after a registry problem they do not have.
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
