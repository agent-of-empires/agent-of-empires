//! Session and workspace deletion, plus worktree/trash reconciliation.

use super::*;

/// Callers retain namespace, submission and instance exclusion through cleanup.
async fn purge_session_artifacts(
    state: &Arc<AppState>,
    id: &str,
    instance: Instance,
    body: &DeleteSessionBody,
    recent_entry: Option<crate::session::RecentProjectEntry>,
) -> anyhow::Result<PurgeOutcome> {
    use crate::session::deletion::{DeletionDisposition, PurgeReservation, PurgeTransaction};
    if state.cityhall_mode && !instance.is_structured() {
        anyhow::bail!(super::lifecycle::LifecycleTargetError::CityHall);
    }
    let profile = instance.source_profile.clone();
    anyhow::ensure!(!profile.is_empty(), "Session has no source profile");
    let request = crate::session::deletion::DeletionRequest {
        session_id: id.to_owned(),
        instance,
        delete_worktree: body.delete_worktree,
        delete_branch: body.delete_branch,
        delete_sandbox: body.delete_sandbox,
        force_delete: body.force_delete,
        detach_hooks: true,
        keep_scratch: body.keep_scratch,
    };
    let worker_state = state.clone();
    let status_id = id.to_owned();
    let reservation = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let store = crate::server::session_store::NativeSessionStore::open(
            worker_state,
            &profile,
            Some(status_id),
        )?;
        PurgeTransaction::reserve(store, request, None)
    })
    .await??;
    let transaction = match reservation {
        PurgeReservation::Reserved(transaction) => transaction,
        PurgeReservation::Rejected(result) => {
            return match result.disposition {
                DeletionDisposition::AlreadyGone => {
                    state.instance_locks.write().await.remove(id);
                    state.session_service.forget_prompt_lock(id).await;
                    Ok(PurgeOutcome::Deleted {
                        messages: result.errors,
                        cleanup_errors: Vec::new(),
                    })
                }
                DeletionDisposition::KeptRestored => Ok(PurgeOutcome::Kept {
                    messages: result.errors,
                    teardown_started: false,
                }),
                DeletionDisposition::Busy => {
                    Err(crate::session::LifecycleReservationError::Superseded.into())
                }
                DeletionDisposition::Failed | DeletionDisposition::Removed => {
                    Err(anyhow::anyhow!(result.errors.join("; ")))
                }
            }
        }
    };
    let transaction = tokio::task::spawn_blocking(move || transaction.run_hooks()).await??;
    let transcript_purged = transaction.instance().is_structured();
    let result = if transcript_purged {
        match tokio::task::spawn_blocking(move || transaction.begin_irreversible()).await? {
            Err(result) => *result,
            Ok(committed) => {
                // Commit removal before destroying a transcript that cannot be restored.
                if let Err(error) = finish_structured_purge(state, id).await {
                    drop(committed);
                    return Ok(PurgeOutcome::Deleted {
                        messages: Vec::new(),
                        cleanup_errors: vec![format!("Session removed, but resources retained because structured shutdown is unproven: {error}")],
                    });
                }
                tokio::task::spawn_blocking(move || committed.finish()).await?
            }
        }
    } else {
        tokio::task::spawn_blocking(move || transaction.complete()).await?
    };
    let mut messages = result.messages;
    match result.disposition {
        DeletionDisposition::KeptRestored => {
            messages.extend(result.errors);
            return Ok(PurgeOutcome::Kept {
                messages,
                teardown_started: result.teardown_started,
            });
        }
        DeletionDisposition::Busy => {
            anyhow::bail!(crate::session::LifecycleReservationError::Superseded)
        }
        DeletionDisposition::Failed => anyhow::bail!(result.errors.join("; ")),
        DeletionDisposition::Removed | DeletionDisposition::AlreadyGone => {}
    }
    let cleanup_errors = if result.disposition == DeletionDisposition::AlreadyGone {
        messages.extend(result.errors);
        Vec::new()
    } else if !result.success {
        result.errors
    } else {
        Vec::new()
    };
    state.instance_locks.write().await.remove(id);
    state.session_service.forget_prompt_lock(id).await;
    if let Some(entry) = recent_entry {
        if let Err(error) = crate::session::record_recent_project(entry) {
            tracing::warn!(target: "http.api.sessions", %error, "recording recent project after delete failed");
        }
    }
    Ok(PurgeOutcome::Deleted {
        messages,
        cleanup_errors,
    })
}

async fn finish_structured_purge(state: &AppState, id: &str) -> anyhow::Result<()> {
    match state.acp_supervisor.shutdown_and_delete(id).await {
        Ok(()) | Err(crate::acp::supervisor::SupervisorError::UnknownSession(_)) => {}
        Err(error) => return Err(error.into()),
    }
    state.acp_supervisor.forget_session(id);
    state.acp_event_store.delete_session(id);
    Ok(())
}

/// Repair moved worktree references through complete native profile publication.
pub(crate) async fn reconcile_worktree_paths(state: &Arc<AppState>) {
    use crate::session::SessionStore;

    if state.read_only {
        return;
    }
    let _namespace = state.profile_namespace.read().await;
    if *state.canonical_health.read().await != crate::daemon::RuntimeHealth::Healthy {
        return;
    }
    let candidates: Vec<String> = state
        .instances
        .read()
        .await
        .iter()
        .filter(|row| {
            row.worktree_info
                .as_ref()
                .is_some_and(|info| info.managed_by_aoe)
        })
        .map(|row| row.id.clone())
        .collect();
    for id in candidates {
        let lock = state.instance_lock(&id).await;
        let _guard = lock.lock().await;
        let profile = {
            let rows = state.instances.read().await;
            let Some(row) = rows.iter().find(|row| row.id == id) else {
                continue;
            };
            row.source_profile.clone()
        };
        let worker_state = state.clone();
        let worker_id = id.clone();
        let cityhall = state.cityhall_mode;
        let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let _identity = crate::session::acquire_session_identity_lock()?;
            anyhow::ensure!(!profile.is_empty(), "session has no source profile");
            let store = crate::server::session_store::NativeSessionStore::open(
                worker_state,
                &profile,
                None,
            )?;
            let Some(mut row) = store.load()?.into_iter().find(|row| row.id == worker_id) else {
                return Ok(());
            };
            if cityhall && !row.is_structured() {
                anyhow::bail!(super::lifecycle::LifecycleTargetError::CityHall);
            }
            crate::session::worktree_reconcile::reconcile_and_persist_locked(
                &store,
                &mut row,
                &mut Default::default(),
            )?;
            Ok(())
        })
        .await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!(target: "http.api.sessions", session = %id, "worktree path reconcile skipped: {error}")
            }
            Err(error) => {
                tracing::warn!(target: "http.api.sessions", session = %id, "worktree path reconcile join failed: {error}")
            }
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
    if state.read_only {
        return;
    }
    let _namespace = state
        .runtime
        .purge_namespace_lease(&state.profile_namespace)
        .await;
    let now = chrono::Utc::now();
    let candidates: Vec<String> = state
        .instances
        .read()
        .await
        .iter()
        .filter(|row| row.is_trashed())
        .map(|row| row.id.clone())
        .collect();
    for id in candidates {
        let Some(_submission) = state
            .session_service
            .prompt_submission_for_session(&id)
            .await
        else {
            continue;
        };
        let lock = state.instance_lock(&id).await;
        let _guard = lock.lock().await;
        let instance = state
            .instances
            .read()
            .await
            .iter()
            .find(|row| row.id == id && row.is_trashed())
            .cloned();
        let Some(instance) = instance else {
            continue;
        };
        let profile = instance.source_profile.clone();
        let worker_state = state.clone();
        let config = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            use crate::session::SessionStore;
            crate::server::session_store::NativeSessionStore::open(worker_state, &profile, None)?
                .configuration(Some(&profile))
        })
        .await;
        let config = match config {
            Ok(Ok(config)) => config,
            error => {
                tracing::warn!(target: "http.api.sessions", ?error, "retention configuration unavailable");
                continue;
            }
        };
        if !crate::session::trash::is_expired(&instance, config.session.trash_retention_days, now) {
            continue;
        }
        let recent_entry = crate::session::recent_project_entry_for(&instance);
        let body = DeleteSessionBody {
            delete_worktree: config.worktree.auto_cleanup,
            delete_branch: config.worktree.should_delete_branch_on_cleanup(),
            delete_sandbox: config.sandbox.auto_cleanup,
            force_delete: true,
            keep_scratch: false,
        };
        match purge_session_artifacts(state, &id, instance, &body, recent_entry).await {
            Ok(outcome) => {
                tracing::info!(target: "http.api.sessions", session = %id, removed = matches!(outcome, PurgeOutcome::Deleted { .. }), "expired trash purge completed")
            }
            Err(error) => {
                tracing::warn!(target: "http.api.sessions", session = %id, %error, "expired trash purge failed")
            }
        }
    }
}

pub async fn delete_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Option<Json<DeleteSessionBody>>,
) -> impl IntoResponse {
    if state.read_only {
        return crate::server::api::read_only_response();
    }
    let namespace = state
        .runtime
        .purge_namespace_lease(&state.profile_namespace)
        .await;
    if let Some(response) = cityhall_block_non_structured(&state, &id).await {
        return response;
    }
    let body = body.map(|Json(body)| body).unwrap_or_default();
    let Some(submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return crate::server::api::session_not_found();
    };
    let lock = state.instance_lock(&id).await;
    let guard = lock.lock().await;
    let instance = state
        .instances
        .read()
        .await
        .iter()
        .find(|row| row.id == id)
        .cloned();
    let Some(instance) = instance else {
        return crate::server::api::session_not_found();
    };
    let recent_entry = crate::session::recent_project_entry_for(&instance);
    let result = purge_session_artifacts(&state, &id, instance, &body, recent_entry).await;
    drop(guard);
    drop(submission);
    drop(namespace);
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(error) => {
            if let Some(response) = super::lifecycle::lifecycle_rejection(&state, &error) {
                return response;
            }
            if *state.canonical_health.read().await != crate::daemon::RuntimeHealth::Healthy {
                return StatusCode::SERVICE_UNAVAILABLE.into_response();
            }
            tracing::error!(target: "http.api.sessions", session = %id, %error, "purge failed");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "deletion_failed", "message": error.to_string()})),
            )
                .into_response();
        }
    };
    let snapshot = match state.runtime.publish(&state).await {
        Ok(snapshot) => snapshot,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    crate::server::runtime::mutation_response(&snapshot.value.cursor, Json(outcome))
}

pub async fn abandon_purge(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    body: Result<Json<crate::daemon::AbandonPurgeBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    use super::lifecycle::{lifecycle_rejection, LifecycleTargetError};
    use crate::session::{LifecycleOperation, SessionStore};

    if state.read_only {
        return crate::server::api::read_only_response();
    }
    let Json(body) = match body {
        Ok(body) => body,
        Err(error) => return error.into_response(),
    };
    let namespace = state
        .runtime
        .abandon_namespace_lease(&state.profile_namespace)
        .await;
    if let Some(response) = cityhall_block_non_structured(&state, &id).await {
        return response;
    }
    let profile = {
        let rows = state.instances.read().await;
        let Some(row) = rows.iter().find(|row| row.id == id) else {
            return crate::server::api::session_not_found();
        };
        row.source_profile.clone()
    };
    let worker_state = state.clone();
    let persist_id = id.clone();
    let committed = tokio::task::spawn_blocking(move || -> anyhow::Result<Instance> {
        anyhow::ensure!(!profile.is_empty(), "Session has no source profile");
        let cityhall = worker_state.cityhall_mode;
        let store =
            crate::server::session_store::NativeSessionStore::open(worker_state, &profile, None)?;
        let mut removed = None;
        // Abandon must not wait on the operation guards whose work it is forgetting.
        store.commit(&mut |rows, _| {
            let index = rows
                .iter()
                .position(|row| row.id == persist_id)
                .ok_or(LifecycleTargetError::Missing)?;
            let row = &rows[index];
            anyhow::ensure!(
                !cityhall || row.is_structured(),
                LifecycleTargetError::CityHall
            );
            anyhow::ensure!(
                row.lifecycle_reservation_is_owned(
                    LifecycleOperation::Purge,
                    body.expected_generation.get()
                ),
                LifecycleTargetError::Busy
            );
            removed = Some(rows.remove(index));
            Ok(())
        })?;
        removed.ok_or_else(|| LifecycleTargetError::Missing.into())
    })
    .await;
    let instance = match committed {
        Ok(Ok(instance)) => instance,
        Ok(Err(error)) => {
            if let Some(response) = lifecycle_rejection(&state, &error) {
                return response;
            }
            tracing::error!(target: "http.api.sessions", session = %id, %error, "purge abandon commit failed");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
        Err(error) => {
            tracing::error!(target: "http.api.sessions", session = %id, %error, "purge abandon task failed");
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };
    let cleanup_state = state.clone();
    state.runtime.work.spawn("abandoned purge cleanup", async move {
        let _namespace = namespace;
        if instance.is_structured() {
            if let Err(error) = finish_structured_purge(&cleanup_state, &instance.id).await {
                tracing::warn!(target: "http.api.sessions", session = %instance.id, %error, "abandoned structured shutdown remains unproven");
            }
        }
        if let Err(error) = tokio::task::spawn_blocking(move || {
            crate::session::deletion::cleanup_abandoned_session(instance);
        }).await {
            tracing::error!(target: "http.api.sessions", %error, "abandoned purge cleanup task failed");
        }
    });
    state.instance_locks.write().await.remove(&id);
    state.session_service.forget_prompt_lock(&id).await;
    let snapshot = match state.runtime.publish(&state).await {
        Ok(snapshot) => snapshot,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    crate::server::runtime::mutation_response(&snapshot.value.cursor, StatusCode::ACCEPTED)
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

/// Purge record-only siblings before their shared-worktree owner.
/// Hold submission guards, then instance guards, in stable id order so
/// overlapping batches cannot invert their locks. Reject a dirty owner before
/// touching siblings; stop before the owner if a sibling fails or was restored.
pub(super) async fn purge_workspace_artifacts(
    state: &Arc<AppState>,
    owner_id: String,
    plan: Vec<(String, DeleteSessionBody)>,
    owner_needs_dirty_check: bool,
) -> (
    Vec<String>,
    Vec<String>,
    Vec<WorkspaceDeleteFailure>,
    Vec<String>,
) {
    let _namespace = state
        .runtime
        .purge_namespace_lease(&state.profile_namespace)
        .await;
    let mut deleted = Vec::new();
    let mut failed = Vec::new();
    let mut messages = Vec::new();
    let mut kept = Vec::new();

    let mut ids: Vec<&str> = plan
        .iter()
        .map(|(id, _)| id.as_str())
        .chain(std::iter::once(owner_id.as_str()))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let mut submission_guards = Vec::with_capacity(ids.len());
    for &id in &ids {
        if let Some(guard) = state
            .session_service
            .prompt_submission_for_session(id)
            .await
        {
            submission_guards.push(guard);
        }
    }
    let mut instance_guards = Vec::with_capacity(ids.len());
    for id in ids {
        instance_guards.push(state.instance_lock(id).await.lock_owned().await);
    }

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
                return (deleted, kept, failed, messages);
            }
        }
    }

    for (id, body) in plan {
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

        let recent_entry = crate::session::recent_project_entry_for(&instance);
        match purge_session_artifacts(state, &id, instance, &body, recent_entry).await {
            Ok(PurgeOutcome::Deleted {
                messages: mut msgs,
                cleanup_errors,
            }) => {
                messages.append(&mut msgs);
                if !cleanup_errors.is_empty() {
                    failed.push(WorkspaceDeleteFailure {
                        id: id.clone(),
                        error: cleanup_errors.join("; "),
                    });
                    deleted.push(id);
                    break;
                }
                deleted.push(id);
            }
            Ok(PurgeOutcome::Kept {
                messages: mut msgs, ..
            }) => {
                messages.append(&mut msgs);
                kept.push(id);
                break;
            }
            Err(msg) => {
                failed.push(WorkspaceDeleteFailure {
                    id: id.clone(),
                    error: msg.to_string(),
                });
                // The owner remains intact when a sibling cannot be safely removed.
                break;
            }
        }
    }

    (deleted, kept, failed, messages)
}

/// Delete siblings before their worktree owner under daemon-owned request execution.
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

    let (deleted, kept, failed, messages) =
        purge_workspace_artifacts(&state, owner_id, plan, owner_needs_dirty_check).await;
    if deleted.is_empty() && !failed.is_empty() {
        let message = failed
            .iter()
            .map(|failure| failure.error.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": "deletion_failed", "message": message, "failed": failed,
            })),
        )
            .into_response();
    }
    let snapshot = match state.runtime.publish(&state).await {
        Ok(snapshot) => snapshot,
        Err(_) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
    };
    crate::server::runtime::mutation_response(
        &snapshot.value.cursor,
        Json(serde_json::json!({
            "status": if !failed.is_empty() || (!deleted.is_empty() && !kept.is_empty()) { "partial" } else if !kept.is_empty() { "kept" } else { "deleted" },
            "deleted": deleted, "kept": kept, "failed": failed, "messages": messages,
        })),
    )
}
