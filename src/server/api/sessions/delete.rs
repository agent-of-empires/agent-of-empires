//! Session and workspace deletion, plus worktree/trash reconciliation.

use super::*;

// --- Delete session ---

#[derive(Default, Deserialize, Clone)]
pub struct DeleteSessionBody {
    #[serde(default)]
    pub delete_worktree: bool,
    #[serde(default)]
    pub delete_branch: bool,
    #[serde(default)]
    pub delete_sandbox: bool,
    #[serde(default)]
    pub force_delete: bool,
    /// For scratch sessions, keep the scratch directory on disk. The session
    /// record is still deleted. No effect on non-scratch sessions.
    #[serde(default)]
    pub keep_scratch: bool,
}

/// Flip a session out of `Status::Deleting` into `Status::Error` so a
/// bookkeeping failure after teardown does not strand it greyed-out and
/// unclickable, the state this detached-task delete exists to prevent.
async fn mark_delete_error(state: &AppState, id: &str, message: String) {
    let mut instances = state.instances.write().await;
    if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
        inst.status = Status::Error;
        inst.last_error = Some(message);
    }
}

/// Permanently purge a session: irreversible ACP teardown, optional sidecar
/// cleanup per `body`, and removal from both `sessions.json` and the in-memory
/// list. Shared by `DELETE /api/sessions/{id}` and the retention auto-purge
/// worker so the permanent-delete path cannot diverge. Blocking reservation,
/// hook and completion phases are dispatched internally, so no caller-held
/// lifecycle guard crosses an await.
///
/// The success `bool` is `true` when the row was actually removed and `false`
/// when a concurrent restore won the race and the row was kept. Callers must
/// not report a kept row as deleted.
async fn purge_session_artifacts(
    state: &Arc<AppState>,
    id: &str,
    instance: Instance,
    body: &DeleteSessionBody,
    recent_entry: Option<crate::session::RecentProjectEntry>,
) -> Result<(bool, Vec<String>), String> {
    let profile = instance.source_profile.clone();
    if profile.is_empty() {
        return Err(
            "Session has no source profile; refusing to acquire a default-profile purge lock"
                .to_string(),
        );
    }
    let delete_request = crate::session::deletion::DeletionRequest {
        session_id: id.to_string(),
        instance: instance.clone(),
        delete_worktree: body.delete_worktree,
        delete_branch: body.delete_branch,
        delete_sandbox: body.delete_sandbox,
        force_delete: body.force_delete,
        detach_hooks: true,
        keep_scratch: body.keep_scratch,
    };
    let file_watch = state.file_watch.clone();
    let reserve_profile = profile.clone();
    let reservation = tokio::task::spawn_blocking(move || {
        let storage = Storage::new(&reserve_profile, file_watch)
            .map_err(|e| format!("Storage init failed before session teardown: {e}"))?;
        crate::session::deletion::PurgeTransaction::reserve(storage, delete_request)
            .map_err(|e| format!("Failed to reserve session purge: {e}"))
    })
    .await
    .map_err(|e| format!("Deletion reservation task failed: {e}"))??;
    let transaction = match reservation {
        crate::session::deletion::PurgeReservation::Reserved(transaction) => transaction,
        crate::session::deletion::PurgeReservation::Rejected(result) => {
            return match result.disposition {
                crate::session::deletion::DeletionDisposition::AlreadyGone => {
                    remove_instance(
                        &mut *state.instances.write().await,
                        id,
                        &state.mutation_epoch,
                    );
                    state.instance_locks.write().await.remove(id);
                    state.session_service.forget_prompt_lock(id).await;
                    Ok((true, result.messages))
                }
                crate::session::deletion::DeletionDisposition::KeptRestored => {
                    Err("Session is being restored, so it was not purged".to_string())
                }
                crate::session::deletion::DeletionDisposition::Busy => {
                    Err(result.errors.first().cloned().unwrap_or_else(|| {
                        "Session is busy with another lifecycle operation, so it was not purged"
                            .to_string()
                    }))
                }
                crate::session::deletion::DeletionDisposition::Failed
                | crate::session::deletion::DeletionDisposition::Removed => {
                    Err(result.errors.join("; "))
                }
            };
        }
    };
    let transaction = tokio::task::spawn_blocking(move || transaction.run_hooks())
        .await
        .map_err(|e| format!("Deletion hook task failed: {e}"))?;

    let transcript_purged = instance.is_structured();

    let deletion_result = if transcript_purged {
        // Commit the row removal before deleting the ACP transcript, so a lost
        // restore/generation race leaves both intact and a successful commit
        // makes later cleanup failures non-restorable by construction.
        let committed = tokio::task::spawn_blocking(move || transaction.begin_irreversible())
            .await
            .map_err(|e| format!("Irreversible deletion commit task failed: {e}"))?;
        match committed {
            Err(result) => *result,
            Ok(committed) => {
                // Remove the local mirror before awaiting ACP so the reconciler
                // cannot surface a durable row that no longer exists. The epoch
                // bump is under the same lock: ACP teardown is slow, and a
                // reload landing inside it would otherwise restore the row.
                remove_instance(
                    &mut *state.instances.write().await,
                    id,
                    &state.mutation_epoch,
                );

                // The worker may still use the worktree, so ACP teardown stays
                // ahead of sidecar cleanup.
                match state.acp_supervisor.shutdown_and_delete(id).await {
                    Ok(()) | Err(crate::acp::supervisor::SupervisorError::UnknownSession(_)) => {}
                    Err(e) => {
                        tracing::warn!(
                            target: "acp.supervisor",
                            session = %id,
                            "shutdown during purge failed: {e}"
                        );
                    }
                }
                state.acp_supervisor.forget_session(id);
                state.acp_event_store.delete_session(id);

                tokio::task::spawn_blocking(move || committed.finish())
                    .await
                    .map_err(|e| format!("Deletion cleanup task failed: {e}"))?
            }
        }
    } else {
        tokio::task::spawn_blocking(move || transaction.complete())
            .await
            .map_err(|e| format!("Deletion task failed: {e}"))?
    };

    let mut messages = deletion_result.messages.clone();
    match deletion_result.disposition {
        crate::session::deletion::DeletionDisposition::KeptRestored
        | crate::session::deletion::DeletionDisposition::Busy => {
            tracing::warn!(
                target: "http.api.sessions",
                session = %id,
                "session changed or was restored before purge completion; kept the durable row"
            );
            return Ok((false, messages));
        }
        crate::session::deletion::DeletionDisposition::Failed => {
            let errs = if deletion_result.errors.is_empty() {
                "Unknown error".to_string()
            } else {
                deletion_result.errors.join("; ")
            };
            return Err(errs);
        }
        crate::session::deletion::DeletionDisposition::Removed
        | crate::session::deletion::DeletionDisposition::AlreadyGone => {}
    }
    if !deletion_result.success {
        let errs = if deletion_result.errors.is_empty() {
            "Unknown error".to_string()
        } else {
            deletion_result.errors.join("; ")
        };
        if !transcript_purged {
            return Err(errs);
        }
        tracing::warn!(
            target: "http.api.sessions",
            session = %id,
            "purge sidecar cleanup failed after durable removal; session stays removed: {errs}"
        );
        messages.push(format!(
            "Cleanup incomplete (session removed anyway): {errs}"
        ));
    }

    {
        // The row is gone from disk and memory, so a reloader carrying an older
        // `sessions.json` snapshot must drop it rather than fold the row back
        // in. `remove_instance` bumps while holding the `instances` write lock
        // and the reloader checks under that same lock, so no reload can slip
        // between removal and bump. See invariant 8 on
        // `reload_state_instances_from_disk`.
        let mut instances = state.instances.write().await;
        remove_instance(&mut instances, id, &state.mutation_epoch);
    }
    state.instance_locks.write().await.remove(id);
    state.session_service.forget_prompt_lock(id).await;
    if let Some(entry) = recent_entry {
        if let Err(e) = crate::session::record_recent_project(entry) {
            tracing::warn!(target: "http.api.sessions",
                "recording recent project after delete failed: {e}");
        }
    }
    Ok((true, messages))
}

/// Heal managed worktree sessions whose recorded `project_path` no longer
/// exists because the directory moved outside aoe, rewriting it from git's own
/// worktree listing. Runs once on daemon startup so every later path-derived
/// decision acts on the live location (#2002).
///
/// A healthy session costs one `stat` and never shells out to git, because the
/// recorded path existing short-circuits the pass inside
/// [`crate::session::worktree_reconcile::reconcile_and_persist`].
pub(crate) async fn reconcile_worktree_paths(state: &Arc<AppState>) {
    let candidates: Vec<String> = {
        let instances = state.instances.read().await;
        instances
            .iter()
            .filter(|i| i.worktree_info.as_ref().is_some_and(|wt| wt.managed_by_aoe))
            .map(|i| i.id.clone())
            .collect()
    };
    for id in candidates {
        let lock = state.instance_lock(&id).await;
        let _guard = lock.lock().await;

        let snapshot = {
            let instances = state.instances.read().await;
            match instances.iter().find(|instance| instance.id == id) {
                Some(instance) => instance.clone(),
                None => continue,
            }
        };
        // `exists()` and the git listing are blocking, so the whole reconcile
        // runs off the runtime and only the resulting path is reapplied under
        // the write lock.
        let reconciled = match tokio::task::spawn_blocking(move || {
            let mut instance = snapshot;
            // An empty profile resolves to the default profile rather than
            // failing, which would aim the persist at another profile's
            // sessions.json. Refuse outright rather than lean on the
            // compare-and-set inside the reconcile.
            anyhow::ensure!(
                !instance.source_profile.is_empty(),
                "session has no source profile; refusing worktree path reconciliation"
            );
            let storage = crate::session::Storage::open_unwatched(&instance.source_profile)?;
            let resolution = crate::session::worktree_reconcile::reconcile_and_persist(
                &storage,
                &mut instance,
                &mut Default::default(),
            )?;
            anyhow::Ok((resolution, instance))
        })
        .await
        {
            Ok(Ok(pair)) => pair,
            Ok(Err(error)) => {
                tracing::warn!(target: "http.api.sessions", session = %id, "worktree path reconcile skipped: {error}");
                continue;
            }
            Err(error) => {
                tracing::warn!(target: "http.api.sessions", session = %id, "worktree path reconcile join failed: {error}");
                continue;
            }
        };
        let crate::session::worktree_reconcile::WorktreePathResolution::Moved(_) = reconciled.0
        else {
            continue;
        };
        let mut instances = state.instances.write().await;
        if let Some(instance) = instances.iter_mut().find(|instance| instance.id == id) {
            instance.project_path = reconciled.1.project_path;
        }
    }
}

/// Relocate any trashed managed worktree still in the active dir into the
/// holding area, and heal a pointer left stale by a crash between the move and
/// its persist. Backfills rows trashed before relocation existed. Runs once on
/// daemon startup, best-effort and per-session locked. The git move is blocking,
/// so it runs off the async runtime.
pub(crate) async fn reconcile_trashed_worktrees(state: &Arc<AppState>) {
    let candidates: Vec<(String, String)> = {
        let instances = state.instances.read().await;
        instances
            .iter()
            .filter(|i| i.is_trashed())
            .map(|i| (i.id.clone(), i.source_profile.clone()))
            .collect()
    };
    for (id, _profile) in candidates {
        let lock = state.instance_lock(&id).await;
        let _guard = lock.lock().await;

        let snapshot = {
            let instances = state.instances.read().await;
            match instances.iter().find(|instance| instance.id == id) {
                Some(instance) if instance.is_trashed() => instance.clone(),
                _ => continue,
            }
        };
        let reconciled = match tokio::task::spawn_blocking(move || {
            let mut instance = snapshot;
            let changed = crate::session::trash::reconcile_trashed_transition(&mut instance)?;
            anyhow::Ok((changed, instance))
        })
        .await
        {
            Ok(Ok(pair)) => pair,
            Ok(Err(error)) => {
                tracing::warn!(target: "http.api.sessions", session = %id, "trash reconcile skipped: {error}");
                continue;
            }
            Err(error) => {
                tracing::warn!(target: "http.api.sessions", session = %id, "trash reconcile join failed: {error}");
                continue;
            }
        };
        if !reconciled.0 {
            continue;
        }
        let moved = reconciled.1;
        let mut instances = state.instances.write().await;
        if let Some(instance) = instances.iter_mut().find(|instance| instance.id == id) {
            instance.project_path = moved.project_path;
            instance.pre_trash_project_path = moved.pre_trash_project_path;
            instance.lifecycle_generation = moved.lifecycle_generation;
            instance.lifecycle_reservation = moved.lifecycle_reservation;
        }
    }
}

/// Auto-purge trashed sessions past their retention window
/// (`trashed_at + session.trash_retention_days`), on daemon startup and hourly.
/// Routed through [`purge_session_artifacts`] so it matches `DELETE` exactly.
/// Each candidate is per-instance locked and re-validated under the lock, so a
/// concurrent restore wins the race and is never purged (#2489).
pub(crate) async fn purge_expired_trash(state: &Arc<AppState>) {
    use std::collections::HashMap;

    let now = chrono::Utc::now();
    let candidates: Vec<(String, String)> = {
        let instances = state.instances.read().await;
        instances
            .iter()
            .filter(|i| i.is_trashed())
            .map(|i| (i.id.clone(), i.source_profile.clone()))
            .collect()
    };
    if candidates.is_empty() {
        return;
    }

    let mut retention_by_profile: HashMap<String, u32> = HashMap::new();
    for (id, profile) in candidates {
        let retention = *retention_by_profile
            .entry(profile.clone())
            .or_insert_with(|| {
                crate::session::config::profile_config::resolve_config_or_warn(&profile)
                    .session
                    .trash_retention_days
            });
        if retention == 0 {
            continue;
        }

        // Submission authority before `instance_lock`, as the permanent DELETE
        // path takes them: teardown must not start under an in-flight queue
        // drain (#3650). A row that vanished since the snapshot is skipped here.
        let Some(_submission) = state
            .session_service
            .prompt_submission_for_session(&id)
            .await
        else {
            continue;
        };
        let lock = state.instance_lock(&id).await;
        let _guard = lock.lock().await;

        // Re-validate under the lock: a restore or earlier purge may have
        // landed since the snapshot.
        let (instance, recent_entry) = {
            let instances = state.instances.read().await;
            match instances.iter().find(|i| i.id == id) {
                Some(inst) if crate::session::trash::is_expired(inst, retention, now) => {
                    (inst.clone(), crate::session::recent_project_entry_for(inst))
                }
                _ => continue,
            }
        };

        // Forces sidecar removal so a dirty worktree cannot keep an expired
        // session pinned in the trash forever.
        let cfg = crate::session::config::profile_config::resolve_config_or_warn(
            &instance.source_profile,
        );
        let body = DeleteSessionBody {
            delete_worktree: cfg.worktree.auto_cleanup,
            delete_branch: cfg.worktree.should_delete_branch_on_cleanup(),
            delete_sandbox: cfg.sandbox.auto_cleanup,
            force_delete: true,
            keep_scratch: false,
        };
        match purge_session_artifacts(state, &id, instance, &body, recent_entry).await {
            Ok((_removed, _messages)) => tracing::info!(
                target: "http.api.sessions",
                session = %id,
                "auto-purged expired trashed session"
            ),
            Err(e) => tracing::warn!(
                target: "http.api.sessions",
                session = %id,
                "auto-purge of expired trash failed: {e}"
            ),
        }
    }
}

pub async fn delete_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<DeleteSessionBody>>,
) -> impl IntoResponse {
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    if state.read_only {
        return crate::server::api::read_only_response();
    }

    let body = body.map(|Json(b)| b).unwrap_or_default();

    // Serialize concurrent mutations, prompt submission first: a queue drain
    // snapshots an idle turn under that guard and never takes `instance_lock`,
    // so without it delivery runs against a worker, worktree and transcript
    // this is tearing down (#3650). Both guards are owned so they move into the
    // detached task below and stay held until the bookkeeping finishes.
    let Some(submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return session_not_found();
    };
    let lock = state.instance_lock(&id).await;
    let guard = lock.lock_owned().await;

    let instance = find_instance(&state, &id).await;

    let Some(instance) = instance else {
        return session_not_found();
    };

    // Captured before `instance` moves into the deletion task; recorded into
    // the recent-projects store only once the delete succeeds, so the project
    // survives in the wizard Recent tab (#2141).
    let recent_entry = crate::session::recent_project_entry_for(&instance);

    // Run teardown and bookkeeping in a detached task. The git / docker / tmux
    // teardown is irreversible once started, so if the client disconnects
    // mid-delete, dropping the request future would abandon the disk-removal
    // and in-memory cleanup and strand the session greyed-out in "Deleting"
    // forever. A detached task is not cancelled when the request future drops;
    // the owned lock guard moves in and is held until the bookkeeping finishes.
    let join = tokio::spawn(async move {
        let _guard = guard;
        let _submission = submission;

        // Mark as Deleting so polling clients see the status change
        {
            let mut instances = state.instances.write().await;
            if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                inst.status = Status::Deleting;
            }
        }

        match purge_session_artifacts(&state, &id, instance, &body, recent_entry).await {
            Ok((removed, messages)) => (
                StatusCode::OK,
                Json(serde_json::json!({
                    // A concurrent restore can keep the row (removed=false); do
                    // not claim it was deleted in that case.
                    "status": if removed { "deleted" } else { "kept" },
                    "messages": messages,
                })),
            ),
            Err(msg) => {
                mark_delete_error(&state, &id, msg.clone()).await;
                tracing::error!(target: "http.api.sessions", "delete failed: {msg}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": "deletion_failed",
                        "message": msg,
                    })),
                )
            }
        }
    });

    match join.await {
        Ok(resp) => resp.into_response(),
        Err(e) => {
            tracing::error!(target: "http.api.sessions",
                "Deletion task panicked or was cancelled: {e}");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Deletion task failed",
            )
        }
    }
}

// --- Delete workspace (atomic multi-session) ---

/// Body for `DELETE /api/workspaces`. `session_ids` are sessions of one web-UI
/// workspace, sharing a git worktree and branch; they need not be all of them.
/// The cleanup flags mirror [`DeleteSessionBody`]. The worktree and branch are
/// cleaned up once, on the first listed session that manages a worktree, and
/// kept with a message while any session outside the request still uses them.
#[derive(Default, Deserialize)]
pub struct DeleteWorkspaceBody {
    #[serde(default)]
    pub session_ids: Vec<String>,
    #[serde(default)]
    pub delete_worktree: bool,
    #[serde(default)]
    pub delete_branch: bool,
    #[serde(default)]
    pub delete_sandbox: bool,
    #[serde(default)]
    pub force_delete: bool,
    #[serde(default)]
    pub keep_scratch: bool,
}

#[derive(Serialize)]
pub(super) struct WorkspaceDeleteFailure {
    pub(super) id: String,
    pub(super) error: String,
}

/// Drop duplicate session ids, preserving first-seen order. With
/// `["owner", "owner"]` the first pass would delete the owner using the
/// record-only sibling flags and the second would skip the missing row,
/// returning success without removing the shared worktree (#2536 review).
pub(super) fn dedupe_session_ids(ids: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    ids.iter()
        .filter(|id| seen.insert((*id).clone()))
        .cloned()
        .collect()
}

/// Build the per-session deletion order for a workspace delete. All sessions
/// share one worktree and branch, so that cleanup must run exactly once: the
/// owner carries the caller's worktree/branch flags and is deleted LAST, every
/// sibling first with worktree/branch removal forced off.
///
/// Owner-last is the safety property. Siblings hold only a record and container,
/// so a sibling failure aborts before the worktree is touched. Deleting the
/// owner first and then failing on a sibling would strand a live record pointing
/// at a deleted worktree (#2536).
pub(super) fn order_workspace_deletion(
    session_ids: &[String],
    body: &DeleteWorkspaceBody,
) -> Vec<(String, DeleteSessionBody)> {
    let Some((owner, siblings)) = session_ids.split_first() else {
        return Vec::new();
    };
    let sibling_body = DeleteSessionBody {
        delete_worktree: false,
        delete_branch: false,
        delete_sandbox: body.delete_sandbox,
        force_delete: body.force_delete,
        keep_scratch: body.keep_scratch,
    };
    let owner_body = DeleteSessionBody {
        delete_worktree: body.delete_worktree,
        delete_branch: body.delete_branch,
        delete_sandbox: body.delete_sandbox,
        force_delete: body.force_delete,
        keep_scratch: body.keep_scratch,
    };
    let mut plan: Vec<(String, DeleteSessionBody)> = siblings
        .iter()
        .map(|id| (id.clone(), sibling_body.clone()))
        .collect();
    plan.push((owner.clone(), owner_body));
    plan
}

/// Owner-worktree dirty preflight for a workspace delete, mirroring the
/// per-session gate in `perform_deletion` so dirty plus non-force stays
/// all-or-nothing. A worktree kept for a session outside `session_ids` is not
/// removed, so its dirtiness does not block. Returns the first dirty message.
async fn workspace_dirty_message(instance: Instance, session_ids: Vec<String>) -> Option<String> {
    tokio::task::spawn_blocking(move || {
        let ids: Vec<&str> = session_ids.iter().map(String::as_str).collect();
        let kept = crate::session::deletion::paths_in_use_except(&ids);
        workspace_dirty_message_blocking(&instance, &kept)
    })
    .await
    .unwrap_or_else(|error| Some(format!("dirty check failed: {error}")))
}

fn workspace_dirty_message_blocking(
    instance: &Instance,
    kept: &crate::session::deletion::PathsInUse,
) -> Option<String> {
    if let Some(wt) = &instance.worktree_info {
        let path = std::path::PathBuf::from(&instance.project_path);
        if wt.managed_by_aoe && !kept.covers(&path) {
            if let Some(msg) = crate::git::cleanup::dirty_worktree_message(&path) {
                return Some(msg);
            }
        }
    }
    if let Some(ws) = &instance.workspace_info {
        if ws.cleanup_on_delete && !kept.covers(std::path::Path::new(&ws.workspace_dir)) {
            for repo in &ws.repos {
                if repo.managed_by_aoe {
                    let path = std::path::PathBuf::from(&repo.worktree_path);
                    if let Some(msg) = crate::git::cleanup::dirty_worktree_message(&path) {
                        return Some(format!("{}: {}", repo.name, msg));
                    }
                }
            }
        }
    }
    None
}

/// Tear down every session in a workspace: record-only siblings first, then the
/// shared-worktree owner last (see [`order_workspace_deletion`]), each through
/// [`purge_session_artifacts`].
///
/// The owner's submission guard and instance lock are held for the whole
/// teardown and the dirty gate is re-checked under them, so dirty plus
/// non-force stays all-or-nothing even if the worktree is dirtied after the
/// handler preflight. This cannot deadlock: a session belongs to exactly one
/// workspace, and sibling locks are taken one at a time. A session already gone
/// is skipped, not failed; a row a concurrent restore kept (`removed == false`)
/// is reported neither deleted nor failed.
pub(super) async fn purge_workspace_artifacts(
    state: &Arc<AppState>,
    owner_id: String,
    plan: Vec<(String, DeleteSessionBody)>,
    owner_needs_dirty_check: bool,
) -> (Vec<String>, Vec<WorkspaceDeleteFailure>, Vec<String>) {
    let mut deleted = Vec::new();
    let mut failed = Vec::new();
    let mut messages = Vec::new();

    // Hold the owner's locks across the entire teardown (see doc comment).
    // `None` only when the owner row already vanished; the plan loop skips it.
    let _owner_submission = state
        .session_service
        .prompt_submission_for_session(&owner_id)
        .await;
    let owner_lock = state.instance_lock(&owner_id).await;
    let _owner_guard = owner_lock.lock_owned().await;

    // Authoritative dirty re-check under the owner lock, before any sibling is
    // torn down: if the worktree went dirty since the preflight, abort with
    // nothing deleted (#2536 review).
    if owner_needs_dirty_check {
        let owner = {
            let instances = state.instances.read().await;
            instances.iter().find(|i| i.id == owner_id).cloned()
        };
        if let Some(owner) = owner {
            let ids = plan.iter().map(|(id, _)| id.clone()).collect();
            if let Some(msg) = workspace_dirty_message(owner, ids).await {
                failed.push(WorkspaceDeleteFailure {
                    id: owner_id,
                    error: format!("Workspace: {msg}"),
                });
                return (deleted, failed, messages);
            }
        }
    }

    for (id, body) in plan {
        // The owner's locks are already held; re-locking here would deadlock.
        let _sibling_locks = if id == owner_id {
            None
        } else {
            Some((
                state
                    .session_service
                    .prompt_submission_for_session(&id)
                    .await,
                state.instance_lock(&id).await.lock_owned().await,
            ))
        };

        let instance = find_instance(state, &id).await;
        let Some(instance) = instance else {
            // A concurrent retention auto-purge won the race, so the row we
            // were asked to delete is gone. A no-op, not a failure.
            continue;
        };

        {
            let mut instances = state.instances.write().await;
            if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                inst.status = Status::Deleting;
            }
        }

        let recent_entry = crate::session::recent_project_entry_for(&instance);
        match purge_session_artifacts(state, &id, instance, &body, recent_entry).await {
            Ok((removed, mut msgs)) => {
                messages.append(&mut msgs);
                // A concurrent restore can keep the row, so only actually
                // removed rows are reported deleted; otherwise the client drops
                // local state for a session that survived.
                if removed {
                    deleted.push(id.clone());
                }
            }
            Err(msg) => {
                mark_delete_error(state, &id, msg.clone()).await;
                failed.push(WorkspaceDeleteFailure {
                    id: id.clone(),
                    error: msg,
                });
                // Stop before the remaining plan entries. The owner is last, so
                // a sibling failure leaves the shared worktree intact with its
                // owning session still present.
                break;
            }
        }
    }

    (deleted, failed, messages)
}

/// `DELETE /api/workspaces`: atomic multi-session workspace delete, replacing
/// the web client's per-session fan-out with one call that tears the workspace
/// down in order under a single detached task, so a mid-delete disconnect
/// cannot leave it half-removed (#2536).
pub async fn delete_workspace(
    State(state): State<Arc<AppState>>,
    body: Option<Json<DeleteWorkspaceBody>>,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }

    let body = body.map(|Json(b)| b).unwrap_or_default();
    // Dedupe up front so a repeated id cannot have the owner deleted with
    // sibling flags and then skipped (#2536 review).
    let mut session_ids = dedupe_session_ids(&body.session_ids);
    if session_ids.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "session_ids must not be empty",
        );
    }
    // The owner is whichever session manages the worktree, not the client's first id.
    {
        let instances = state.instances.read().await;
        if let Some(index) = session_ids.iter().position(|id| {
            instances
                .iter()
                .any(|i| &i.id == id && i.has_managed_worktree_or_workspace())
        }) {
            session_ids[..=index].rotate_right(1);
        }
    }
    let owner_id = session_ids[0].clone();

    // CityHall: `purge_workspace_artifacts` tears down EVERY id, so every one
    // must be a structured session this mode created; otherwise a client could
    // smuggle a foreign plain session in as a sibling (#7).
    if let Some(resp) = cityhall_block_any_non_structured(&state, &session_ids).await {
        return resp;
    }

    let owner_needs_dirty_check = body.delete_worktree && !body.force_delete;

    // Preflight: refuse a non-force delete of a dirty shared worktree before
    // tearing down any session. A fast early 409;
    // `purge_workspace_artifacts` re-checks authoritatively under the owner lock.
    if owner_needs_dirty_check {
        let owner = {
            let instances = state.instances.read().await;
            instances.iter().find(|i| i.id == owner_id).cloned()
        };
        if let Some(owner) = owner {
            if let Some(msg) = workspace_dirty_message(owner, session_ids.clone()).await {
                return api_error(StatusCode::CONFLICT, "dirty_worktree", msg);
            }
        }
    }

    let plan = order_workspace_deletion(&session_ids, &body);

    // Detached task, mirroring `delete_session`: teardown must run to
    // completion even if the client disconnects mid-delete.
    let join = tokio::spawn(async move {
        purge_workspace_artifacts(&state, owner_id, plan, owner_needs_dirty_check).await
    });

    match join.await {
        Ok((deleted, failed, messages)) => {
            if deleted.is_empty() && !failed.is_empty() {
                let msg = failed
                    .iter()
                    .map(|f| f.error.clone())
                    .collect::<Vec<_>>()
                    .join("; ");
                tracing::error!(target: "http.api.sessions", "workspace delete failed: {msg}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({
                        "error": "deletion_failed",
                        "message": msg,
                        "failed": failed,
                    })),
                )
                    .into_response();
            }
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "status": if failed.is_empty() { "deleted" } else { "partial" },
                    "deleted": deleted,
                    "failed": failed,
                    "messages": messages,
                })),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.sessions",
                "Workspace deletion task panicked or was cancelled: {e}");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "Workspace deletion task failed",
            )
        }
    }
}
