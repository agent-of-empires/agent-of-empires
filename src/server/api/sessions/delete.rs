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
    /// For scratch sessions, keep the scratch directory on disk instead of
    /// removing it. The session record is still deleted. No effect on
    /// non-scratch sessions.
    #[serde(default)]
    pub keep_scratch: bool,
}

/// Flip a session out of `Status::Deleting` into `Status::Error` so a
/// bookkeeping failure after teardown does not strand it greyed-out and
/// unclickable, the exact state this detached-task delete exists to prevent.
async fn mark_delete_error(state: &AppState, id: &str, message: String) {
    let mut instances = state.instances.write().await;
    if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
        inst.status = Status::Error;
        inst.last_error = Some(message);
    }
}

/// Permanently purge a session: irreversible ACP teardown (structured
/// view), optional sidecar cleanup (worktree/branch/container/scratch per
/// `body`), and removal from both `sessions.json` and the in-memory list.
/// Shared by the `DELETE /api/sessions/{id}` handler and the retention
/// auto-purge worker so the permanent-delete path cannot diverge between the
/// two. Returns user-facing deletion messages on success, or a descriptive
/// error string on failure. Blocking reservation, hook, and completion phases
/// are dispatched internally; no caller-held lifecycle guard crosses an await.
/// The `bool` in the success tuple is `true` when the session row was actually
/// removed, and `false` when a concurrent restore won the race and the row was
/// deliberately kept (see the `kept_restored` branch). Callers must not report
/// a kept row as deleted.
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
        // Commit the row removal before deleting the ACP transcript. A lost
        // restore/generation race therefore leaves both row and transcript
        // intact; a successful commit makes later cleanup failures
        // non-restorable by construction.
        let committed = tokio::task::spawn_blocking(move || transaction.begin_irreversible())
            .await
            .map_err(|e| format!("Irreversible deletion commit task failed: {e}"))?;
        match committed {
            Err(result) => *result,
            Ok(committed) => {
                // Remove the local mirror before awaiting ACP so the reconciler
                // cannot surface a durable row that no longer exists. Bumps the
                // epoch under the same lock: the ACP teardown below is slow, and
                // a reload landing inside it would otherwise restore the row.
                remove_instance(
                    &mut *state.instances.write().await,
                    id,
                    &state.mutation_epoch,
                );

                // The worker may still use the worktree, so ACP teardown stays
                // ahead of sidecar cleanup. The durable row is already gone.
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
        // The row is now gone from both disk and memory, so any reloader still
        // carrying a `sessions.json` snapshot that predates either removal must
        // drop it rather than fold the deleted row back in. `remove_instance`
        // bumps while still holding the `instances` write lock: a reloader
        // checks the epoch under that same lock, so the removal and the bump
        // land as one step and a reload cannot slip between them. See
        // invariant 8 on `reload_state_instances_from_disk`.
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
/// exists because the directory was moved outside aoe, rewriting it from git's
/// own worktree listing. Runs once on daemon startup, so every later
/// path-derived decision (worker cwd, diff, the rename pre-flight gates) acts
/// on the live location. See #2002.
///
/// The recorded path existing short-circuits the whole pass inside
/// [`crate::session::worktree_reconcile::reconcile_and_persist`], so a healthy
/// session costs one `stat` and never shells out to git. Every non-move outcome
/// leaves the row untouched.
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
        // `exists()` and the git listing are blocking filesystem work, so the
        // whole reconcile runs off the runtime and only the resulting path is
        // reapplied under the write lock.
        let reconciled = match tokio::task::spawn_blocking(move || {
            let mut instance = snapshot;
            // An empty profile resolves to the *default* profile rather than
            // failing, which would aim the persist at another profile's
            // sessions.json. The compare-and-set inside the reconcile makes
            // that a no-op, but refuse outright rather than lean on it.
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

/// Relocate any trashed managed worktree still sitting in the active dir into
/// the holding area, and heal a pointer left stale by a crash between the move
/// and its persist. Backfills rows trashed before relocation existed. Runs
/// once on daemon startup, best-effort and per-session locked; a failure on one
/// session logs and moves on. The git move is blocking, so it runs off the
/// async runtime.
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

/// Auto-purge trashed sessions whose retention window has elapsed
/// (`trashed_at + session.trash_retention_days`). Runs on daemon startup and
/// hourly thereafter. Routed through [`purge_session_artifacts`] so the
/// permanent-delete path matches `DELETE` exactly. Each candidate is
/// per-instance locked and its trashed+expired state re-validated under the
/// lock, so a concurrent restore wins the race and is never purged. See
/// #2489.
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

        // Submission authority before `instance_lock`, as the permanent
        // `DELETE` path takes them: teardown must not start under an in-flight
        // queue drain (#3650). A row that vanished since the snapshot is
        // skipped here rather than below.
        let Some(_submission) = state
            .session_service
            .prompt_submission_for_session(&id)
            .await
        else {
            continue;
        };
        let lock = state.instance_lock(&id).await;
        let _guard = lock.lock().await;

        // Re-validate under the lock: a restore (or an earlier purge) may
        // have landed since the snapshot.
        let (instance, recent_entry) = {
            let instances = state.instances.read().await;
            match instances.iter().find(|i| i.id == id) {
                Some(inst) if crate::session::trash::is_expired(inst, retention, now) => {
                    (inst.clone(), crate::session::recent_project_entry_for(inst))
                }
                _ => continue,
            }
        };

        // Permanent retention purge cleans sidecars per the profile defaults,
        // but forces removal so a dirty worktree can't keep an expired
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
    // CityHall: only act on structured sessions this mode created; refuse a
    // non-structured (or unknown) target so a locked-down client cannot
    // respawn/destroy/edit an enumerated plain session. See #7.
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    if state.read_only {
        return crate::server::api::read_only_response();
    }

    let body = body.map(|Json(b)| b).unwrap_or_default();

    // Serialize concurrent mutations. Prompt submission first, per
    // `prompt_submission`: a queue drain snapshots an idle turn under that
    // guard and never takes `instance_lock`, so without it the delivery runs
    // against a worker, worktree and transcript this is tearing down (#3650).
    // Both guards are owned so they move into the detached deletion task below
    // and stay held until the bookkeeping finishes, rather than only until
    // this request future is dropped.
    let Some(submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return crate::server::api::session_not_found();
    };
    let lock = state.instance_lock(&id).await;
    let guard = lock.lock_owned().await;

    // Find and clone the instance (need the full Instance for deletion)
    let instance = {
        let instances = state.instances.read().await;
        instances.iter().find(|i| i.id == id).cloned()
    };

    let Some(instance) = instance else {
        return crate::server::api::session_not_found();
    };

    // Captured before `instance` moves into the deletion task; recorded into
    // the persisted recent-projects store only once the delete fully
    // succeeds, so the project survives in the wizard Recent tab (#2141).
    let recent_entry = crate::session::recent_project_entry_for(&instance);

    // Run the whole teardown + bookkeeping in a detached task. The
    // git / docker / tmux teardown below is irreversible once it starts, but
    // the disk-removal and in-memory cleanup that must follow it live in this
    // request future. If the client disconnects mid-delete (e.g. closes the
    // tab during a multi-second worktree removal), dropping the request future
    // would abandon that bookkeeping after the session was already physically
    // gone, stranding it greyed-out in the "Deleting" state forever. A
    // detached task is not cancelled when the request future drops, so it
    // always runs to completion; the owned lock guard moves in and is held
    // until the bookkeeping finishes.
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
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "internal",
                    "message": "Deletion task failed",
                })),
            )
                .into_response()
        }
    }
}

// --- Delete workspace (atomic multi-session) ---

/// Body for `DELETE /api/workspaces`. `session_ids` is the full set of
/// sessions in one web-UI workspace, all sharing a single git worktree +
/// branch, ordered so the first id is the worktree owner (the web
/// `sessions[0]` primary). The cleanup flags mirror [`DeleteSessionBody`]:
/// they apply to the whole workspace, and the shared worktree/branch is
/// removed exactly once, on the owner.
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

/// Drop duplicate session ids while preserving first-seen order. A workspace
/// delete must never list the same session twice: with `["owner", "owner"]`
/// the first pass would delete the owner using the record-only sibling flags
/// and the second pass would skip the now-missing row, returning success
/// without ever removing the shared worktree or branch (#2536 review).
pub(super) fn dedupe_session_ids(ids: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    ids.iter()
        .filter(|id| seen.insert((*id).clone()))
        .cloned()
        .collect()
}

/// Build the per-session deletion order for a workspace delete. All sessions
/// in a workspace share one git worktree + branch, so worktree/branch cleanup
/// must run exactly once. The owner (`session_ids[0]`, the web primary)
/// carries the caller's worktree/branch flags and is deleted LAST; every
/// sibling is deleted first with worktree/branch removal forced off.
///
/// Owner-last is the safety property. Siblings hold only a record + container,
/// never the shared worktree, so tearing them down while the worktree is still
/// present lets a sibling failure abort before the worktree is touched, leaving
/// nothing orphaned. Deleting the owner first (worktree gone) and then failing
/// on a sibling would strand a live record pointing at a deleted worktree, the
/// exact failure #2536 exists to remove.
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

/// Owner-worktree dirty preflight for a workspace delete. Mirrors the per-
/// session dirty gate in `perform_deletion` so a non-force delete of a dirty
/// shared worktree is refused before any session is torn down, keeping dirty +
/// non-force all-or-nothing. Returns the first dirty message found.
fn workspace_dirty_message(instance: &Instance) -> Option<String> {
    if let Some(wt) = &instance.worktree_info {
        if wt.managed_by_aoe {
            let path = std::path::PathBuf::from(&instance.project_path);
            if let Some(msg) = crate::git::cleanup::dirty_worktree_message(&path) {
                return Some(msg);
            }
        }
    }
    if let Some(ws) = &instance.workspace_info {
        if ws.cleanup_on_delete {
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
/// shared-worktree owner last (see [`order_workspace_deletion`]). Each session
/// goes through the shared [`purge_session_artifacts`].
///
/// The owner's submission guard and instance lock are acquired up front and
/// held for the whole teardown, and the dirty-worktree gate is re-checked
/// under them right before any sibling is torn down. This serializes the dirty
/// check with the teardown so dirty + non-force stays all-or-nothing even if
/// the worktree is dirtied between the handler preflight and now, and it
/// cannot deadlock: a session belongs to exactly one workspace, so two
/// workspace deletes never contend for each other's locks, and single-session
/// deletes only ever hold one session's locks at a time. Sibling locks are
/// then taken one session at a time. A session already gone (a retention purge
/// won the race) is skipped, not failed; a
/// pre-owner failure aborts before the worktree is removed, so the shared
/// worktree keeps its live owning session rather than being orphaned. A
/// session whose row a concurrent restore kept (`removed == false`) is reported
/// neither deleted nor failed.
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
    // torn down (#2536 review). If the worktree went dirty since the handler
    // preflight, abort with nothing deleted.
    if owner_needs_dirty_check {
        let owner = {
            let instances = state.instances.read().await;
            instances.iter().find(|i| i.id == owner_id).cloned()
        };
        if let Some(owner) = owner {
            if let Some(msg) = workspace_dirty_message(&owner) {
                failed.push(WorkspaceDeleteFailure {
                    id: owner_id,
                    error: format!("Workspace: {msg}"),
                });
                return (deleted, failed, messages);
            }
        }
    }

    for (id, body) in plan {
        // The owner's locks are already held; only siblings need their own,
        // one at a time. Re-locking the owner here would self-deadlock.
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

        let instance = {
            let instances = state.instances.read().await;
            instances.iter().find(|i| i.id == id).cloned()
        };
        let Some(instance) = instance else {
            // Already deleted (a concurrent retention auto-purge won the race).
            // The row we were asked to delete is gone, so this is a no-op, not
            // a failure.
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
                // A concurrent restore can keep the row (removed=false); only
                // report rows that were actually removed as deleted, so the
                // client never drops local state for a session that survived.
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
                // a sibling failure here leaves the shared worktree intact with
                // its owning session still present, never orphaned.
                break;
            }
        }
    }

    (deleted, failed, messages)
}

/// `DELETE /api/workspaces`: atomic multi-session workspace delete. Replaces
/// the web client's N-call fan-out (one `DELETE /api/sessions/:id` per session)
/// with a single call that tears the whole workspace down in the correct order
/// under one detached task, so a mid-delete client disconnect can no longer
/// leave the workspace half-removed. See #2536.
pub async fn delete_workspace(
    State(state): State<Arc<AppState>>,
    body: Option<Json<DeleteWorkspaceBody>>,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }

    let body = body.map(|Json(b)| b).unwrap_or_default();
    // Dedupe up front so a repeated id can't have the owner deleted with
    // sibling flags and then skipped (#2536 review).
    let session_ids = dedupe_session_ids(&body.session_ids);
    let Some(owner_id) = session_ids.first().cloned() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid_request",
                "message": "session_ids must not be empty",
            })),
        )
            .into_response();
    };

    // CityHall: `purge_workspace_artifacts` tears down EVERY id in the list, not
    // just the owner, so every id (not only `session_ids.first()`) must be a
    // structured session this mode created. Otherwise a client could smuggle a
    // foreign plain session in as a sibling and have it destroyed. See #7.
    if let Some(resp) = cityhall_block_any_non_structured(&state, &session_ids).await {
        return resp;
    }

    let owner_needs_dirty_check = body.delete_worktree && !body.force_delete;

    // Preflight: refuse a non-force delete of a dirty shared worktree before
    // tearing down any session, so dirty + non-force stays all-or-nothing. The
    // owner (session_ids[0]) is the session that carries the shared worktree.
    // This is a fast early 409 for the common case; `purge_workspace_artifacts`
    // re-checks authoritatively under the owner lock.
    if owner_needs_dirty_check {
        let owner = {
            let instances = state.instances.read().await;
            instances.iter().find(|i| i.id == owner_id).cloned()
        };
        if let Some(owner) = owner {
            if let Some(msg) = workspace_dirty_message(&owner) {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "error": "dirty_worktree",
                        "message": msg,
                    })),
                )
                    .into_response();
            }
        }
    }

    let plan = order_workspace_deletion(&session_ids, &body);

    // Detached task, mirroring `delete_session`: the teardown must run to
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
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "internal",
                    "message": "Workspace deletion task failed",
                })),
            )
                .into_response()
        }
    }
}
