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

/// Show a row as `Deleting` for polling clients, returning the status it had.
/// The overlay is memory-only, so a caller that ends up deleting nothing has to
/// put that status back rather than leave the row stuck greyed-out.
async fn mark_delete_in_progress(
    state: &AppState,
    original: &crate::session::LaunchOrigin,
) -> Option<Status> {
    let mut instances = state.instances.write().await;
    let inst = instances
        .iter_mut()
        .find(|instance| original.matches_instance(instance))?;
    Some(std::mem::replace(&mut inst.status, Status::Deleting))
}

async fn restore_delete_status(
    state: &AppState,
    original: &crate::session::LaunchOrigin,
    issued: Option<&crate::session::LaunchOrigin>,
    status: Status,
) {
    let mut instances = state.instances.write().await;
    if let Some(inst) = instances.iter_mut().find(|instance| {
        original.matches_instance(instance)
            || issued.is_some_and(|scope| scope.matches_instance(instance))
    }) {
        inst.status = status;
    }
}

/// Why a purge could not complete. `Retryable` is a transient conflict the
/// client can retry (a runner that is still being torn down); `Fatal` is a
/// hard failure. Collapsing the two would report a live runner as a server
/// error and leave the row marked failed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PurgeRefusal {
    Retryable(String),
    Fatal(String),
}

impl PurgeRefusal {
    fn message(&self) -> &str {
        match self {
            Self::Retryable(message) | Self::Fatal(message) => message,
        }
    }

    fn is_retryable(&self) -> bool {
        matches!(self, Self::Retryable(_))
    }
}

impl From<String> for PurgeRefusal {
    fn from(message: String) -> Self {
        Self::Fatal(message)
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
fn purge_session_artifacts(
    state: &Arc<AppState>,
    id: &str,
    instance: Instance,
    body: &DeleteSessionBody,
    recent_entry: Option<crate::session::RecentProjectEntry>,
) -> impl std::future::Future<Output = Result<(bool, Vec<String>), PurgeRefusal>> + Send + 'static {
    let original = crate::session::LaunchOrigin::capture_baseline(&instance);
    let state = Arc::clone(state);
    let id = id.to_owned();
    let body = body.clone();
    let driver = tokio::spawn(async move {
        let original = original.map_err(|error| PurgeRefusal::Fatal(error.to_string()))?;
        let previous_status = mark_delete_in_progress(&state, &original).await;
        let mut issued = None;
        let outcome =
            purge_session_artifacts_inner(&state, &id, instance, &body, recent_entry, &mut issued)
                .await;
        if !matches!(outcome, Ok((true, _))) {
            if let Some(status) = previous_status {
                restore_delete_status(&state, &original, issued.as_deref(), status).await;
            }
        }
        outcome
    });
    async move {
        driver
            .await
            .map_err(|error| PurgeRefusal::Fatal(format!("Purge owner task failed: {error}")))?
    }
}

/// The purge proper. The caller owns the `Deleting` overlay; this only reports
/// what it removed.
async fn purge_session_artifacts_inner(
    state: &Arc<AppState>,
    id: &str,
    instance: Instance,
    body: &DeleteSessionBody,
    recent_entry: Option<crate::session::RecentProjectEntry>,
    issued: &mut Option<Arc<crate::session::LaunchOrigin>>,
) -> Result<(bool, Vec<String>), PurgeRefusal> {
    let profile = instance.source_profile.clone();
    if profile.is_empty() {
        return Err(PurgeRefusal::Fatal(
            "Session has no source profile; refusing to acquire a default-profile purge lock"
                .to_string(),
        ));
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
    let storage = instance
        .original_storage()
        .map_err(|error| PurgeRefusal::Fatal(error.to_string()))?;
    let reservation = tokio::task::spawn_blocking(
        move || -> Result<crate::session::deletion::PurgeReservation, String> {
            let reservation = crate::session::deletion::PurgeTransaction::reserve(
                storage.as_ref().clone(),
                delete_request,
            )
            .map_err(|e| format!("Failed to reserve session purge: {e}"))?;
            match reservation {
                crate::session::deletion::PurgeReservation::Reserved(transaction) => {
                    match transaction.preflight_ownership() {
                        Ok(transaction) => Ok(
                            crate::session::deletion::PurgeReservation::Reserved(transaction),
                        ),
                        Err(result) => Ok(crate::session::deletion::PurgeReservation::Rejected(
                            *result,
                        )),
                    }
                }
                rejected => Ok(rejected),
            }
        },
    )
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
                    Err(PurgeRefusal::Fatal(
                        "Session is being restored, so it was not purged".to_string(),
                    ))
                }
                // Another purge holds the claim, or the session moved to a
                // newer lifecycle generation. Both are contention that clears,
                // so this is the retry the first 409 already asked for rather
                // than a failure worth reporting as fatal.
                crate::session::deletion::DeletionDisposition::Busy => Err(
                    PurgeRefusal::Retryable(result.errors.first().cloned().unwrap_or_else(|| {
                        "Session is busy with another lifecycle operation, so it was not \
                             purged; retry once that operation finishes"
                            .to_string()
                    })),
                ),
                crate::session::deletion::DeletionDisposition::Failed
                | crate::session::deletion::DeletionDisposition::Removed => {
                    Err(PurgeRefusal::Fatal(result.errors.join("; ")))
                }
            };
        }
    };
    let scope = transaction.native_stop_scope();
    *issued = Some(scope.cancellation_origin());
    let transcript_purged = transaction.instance().is_structured();

    // Every current view can have historical executions. The scoped manager
    // cleanup may request session/delete before stopping its owned connection,
    // but neither its memory nor a missing registry substitutes for journal proof.
    // Release process-wide flocks across both async gates; keep the reservation.
    let transaction = transaction.release_locks_for_teardown();
    let manager_result = if transcript_purged {
        Some(
            state
                .acp_supervisor
                .shutdown_and_delete(scope.clone())
                .await,
        )
    } else {
        None
    };
    let transaction = crate::session::deletion::settle_runner_of(transaction)
        .await
        .map_err(|result| {
            let detail = result.errors.join("; ");
            PurgeRefusal::Retryable(if transcript_purged {
                format!("Agent-side session deletion may already have run. No local purge was committed by this request; this proof refusal concerns local artifacts only: {detail}")
            } else {
                detail
            })
        })?;
    if let Some(result) = manager_result {
        match result {
            Ok(()) => {}
            Err(crate::acp::supervisor::SupervisorError::TeardownPending(_)) => {
                return Err(PurgeRefusal::Retryable(format!(
                    "Session {id} teardown was not completed; its record was kept, but the agent-side transcript may already have been released. Retry after the runner exits."
                )));
            }
            Err(error) => {
                return Err(PurgeRefusal::Fatal(format!(
                    "Structured session cleanup failed; its record was kept: {error}"
                )))
            }
        }
    }

    let transaction = tokio::task::spawn_blocking(move || transaction.run_hooks())
        .await
        .map_err(|e| format!("Deletion hook task failed: {e}"))?;

    // The runner is settled and the hooks have run; from here the purge
    // proceeds to its irreversible commit.

    let mut post_commit_error: Option<String> = None;
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
                let store = state.acp_event_store.clone();
                let cleanup_id = id.to_owned();
                let (cleanup, event_error) = tokio::task::spawn_blocking(move || {
                    let event_error = store
                        .delete_session(&cleanup_id)
                        .err()
                        .map(|error| format!("ACP event deletion failed: {error}"));
                    (committed.finish(), event_error)
                })
                .await
                .map_err(|error| format!("Deletion cleanup task failed: {error}"))?;
                state.acp_supervisor.forget_session(scope.original());
                post_commit_error = event_error;
                cleanup
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
            return Err(PurgeRefusal::Fatal(errs));
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
            return Err(PurgeRefusal::Fatal(errs));
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
        if instances.iter().any(|instance| {
            scope.original().matches_instance(instance)
                || scope.cancellation_origin().matches_instance(instance)
        }) {
            remove_instance(&mut instances, id, &state.mutation_epoch);
        }
    }
    state.instance_locks.write().await.remove(id);
    state.session_service.forget_prompt_lock(id).await;
    if let Some(entry) = recent_entry {
        if let Err(e) = crate::session::record_recent_project(entry) {
            tracing::warn!(target: "http.api.sessions",
                "recording recent project after delete failed: {e}");
        }
    }
    if let Some(error) = post_commit_error {
        // The row is already gone, so this is a leftover, not a failed delete.
        // Name it in the response: the ACP transcript is still on disk.
        tracing::warn!(
            target: "http.api.sessions",
            session = %id,
            "session purged but its ACP transcript was left behind: {error}"
        );
        messages.push(format!(
            "Session removed, but its ACP transcript was not purged: {error}"
        ));
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
    let mut ids: Vec<String> = {
        let instances = state.instances.read().await;
        instances
            .iter()
            .filter(|row| row.is_trashed())
            .map(|row| row.id.clone())
            .collect()
    };
    ids.sort();
    ids.dedup();
    if ids.is_empty() {
        return;
    }
    let mut guards = Vec::with_capacity(ids.len());
    for id in &ids {
        guards.push(state.instance_lock(id).await.lock_owned().await);
    }
    let snapshots: Vec<Instance> = {
        let instances = state.instances.read().await;
        instances
            .iter()
            .filter(|row| row.is_trashed() && ids.binary_search(&row.id).is_ok())
            .cloned()
            .collect()
    };
    let work_state = Arc::clone(state);
    let reconciled = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let mut storages: Vec<Storage> = Vec::new();
        for row in &snapshots {
            let storage = row.original_storage()?;
            if !storages
                .iter()
                .any(|original| original.same_origin_as(&storage))
            {
                storages.push((*storage).clone());
            }
        }
        let reconciled = crate::session::trash::reconcile_trashed_profiles(&storages)?;
        if reconciled.is_empty() {
            return Ok(());
        }
        let _workspace = crate::session::acquire_session_workspace_claim_lock()?;
        let _identity = crate::session::acquire_session_identity_lock()?;
        for (storage, _) in &reconciled {
            storage.verify_profile_identity()?;
        }
        let mut instances = work_state.instances.blocking_write();
        let mut changed = false;
        for (storage, rows) in reconciled {
            for moved in rows {
                let Some(snapshot) = snapshots
                    .iter()
                    .find(|row| row.id == moved.id && row.source_profile == storage.profile())
                else {
                    continue;
                };
                let Some(instance) = instances.iter_mut().find(|row| {
                    row.id == moved.id
                        && row.source_profile == storage.profile()
                        && row.lifecycle_generation == snapshot.lifecycle_generation
                        && row.project_path == snapshot.project_path
                        && row.pre_trash_project_path == snapshot.pre_trash_project_path
                        && row.worktree_info == snapshot.worktree_info
                        && row
                            .workspace_info
                            .as_ref()
                            .map(|ws| (&ws.workspace_dir, &ws.branch, &ws.repos))
                            == snapshot
                                .workspace_info
                                .as_ref()
                                .map(|ws| (&ws.workspace_dir, &ws.branch, &ws.repos))
                        && row.trashed_at == snapshot.trashed_at
                }) else {
                    continue;
                };
                instance.project_path = moved.project_path;
                instance.pre_trash_project_path = moved.pre_trash_project_path;
                instance.lifecycle_generation = moved.lifecycle_generation;
                instance.lifecycle_reservation = moved.lifecycle_reservation;
                changed = true;
            }
        }
        if changed {
            work_state
                .mutation_epoch
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(())
    })
    .await;
    match reconciled {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(target: "http.api.sessions", "trash reconcile skipped: {error}")
        }
        Err(error) => {
            tracing::warn!(target: "http.api.sessions", "trash reconcile join failed: {error}")
        }
    }
}

/// Auto-purge trashed sessions past their retention window
/// (`trashed_at + session.trash_retention_minutes`), on daemon startup and
/// every [`trash_sweep_interval`].
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
                    .trash_retention_minutes
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
                "auto-purge of expired trash failed: {e:?}"
            ),
        }
    }
}

/// Next retention sweep delay, from the global window and every profile's, so
/// a session trashed into any profile is honored within its slack. Merges
/// without [`resolve_config`](crate::session::config::profile_config::resolve_config),
/// which reinstalls and re-warns about status rules on every call.
pub(crate) fn trash_sweep_interval() -> std::time::Duration {
    use crate::session::config::profile_config::{load_profile_config, merge_configs};
    let Ok(global) = crate::session::config::Config::load() else {
        return crate::session::trash::sweep_interval([]);
    };
    let profiles = crate::session::list_profiles().unwrap_or_default();
    let windows = profiles.iter().filter_map(|profile| {
        load_profile_config(profile).ok().map(|pc| {
            merge_configs(global.clone(), &pc)
                .session
                .trash_retention_minutes
        })
    });
    crate::session::trash::sweep_interval(
        std::iter::once(global.session.trash_retention_minutes).chain(windows),
    )
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

        // `purge_session_artifacts` owns the `Deleting` overlay and takes it
        // back itself on every exit that removes nothing.
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
            Err(refusal) if refusal.is_retryable() => {
                // A runner that is still tearing down is a transient conflict,
                // not a server error: nothing was removed and the overlay is
                // already back, so the row is not marked failed either.
                (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({
                        "error": "teardown_pending",
                        "message": refusal.message(),
                    })),
                )
            }
            Err(refusal) => {
                let msg = refusal.message().to_string();
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
    /// The runner is still tearing down, so the client can retry unchanged.
    pub(super) retryable: bool,
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
async fn workspace_dirty_message(
    instance: Instance,
    sessions: Vec<(String, String)>,
) -> Option<String> {
    tokio::task::spawn_blocking(move || {
        let owners: Vec<_> = sessions
            .iter()
            .map(
                |(profile, session_id)| crate::session::deletion::SessionPathOwner {
                    profile,
                    session_id,
                },
            )
            .collect();
        let kept = crate::session::deletion::paths_in_use_except(&owners);
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

/// Acquire all session authorities in stable ID order; tear down the owner last.
pub(super) async fn purge_workspace_artifacts(
    state: &Arc<AppState>,
    owner_id: String,
    plan: Vec<(String, DeleteSessionBody)>,
    owner_needs_dirty_check: bool,
) -> (Vec<String>, Vec<WorkspaceDeleteFailure>, Vec<String>) {
    let mut deleted = Vec::new();
    let mut failed = Vec::new();
    let mut messages = Vec::new();

    let mut lock_order: Vec<_> = plan
        .iter()
        .map(|(id, _)| id.as_str())
        .chain(std::iter::once(owner_id.as_str()))
        .collect();
    lock_order.sort_unstable();
    lock_order.dedup();
    let mut submissions = Vec::with_capacity(lock_order.len());
    for id in &lock_order {
        submissions.push(
            state
                .session_service
                .prompt_submission_for_session(id)
                .await,
        );
    }
    let mut instance_guards = Vec::with_capacity(lock_order.len());
    for id in &lock_order {
        instance_guards.push(state.instance_lock(id).await.lock_owned().await);
    }
    drop(lock_order);

    // Authoritative dirty re-check under the owner lock, before any sibling is
    // torn down: if the worktree went dirty since the preflight, abort with
    // nothing deleted (#2536 review).
    if owner_needs_dirty_check {
        let (owner, selected) = {
            let instances = state.instances.read().await;
            let selected = plan
                .iter()
                .filter_map(|(id, _)| instances.iter().find(|row| row.id == *id))
                .map(|row| (row.source_profile.clone(), row.id.clone()))
                .collect();
            (
                instances.iter().find(|i| i.id == owner_id).cloned(),
                selected,
            )
        };
        if let Some(owner) = owner {
            if let Some(msg) = workspace_dirty_message(owner, selected).await {
                failed.push(WorkspaceDeleteFailure {
                    id: owner_id,
                    error: format!("Workspace: {msg}"),
                    // A dirty worktree is the user's call, not a transient race.
                    retryable: false,
                });
                return (deleted, failed, messages);
            }
        }
    }

    for (id, body) in plan {
        let instance = find_instance(state, &id).await;
        let Some(instance) = instance else {
            // A concurrent retention auto-purge won the race, so the row we
            // were asked to delete is gone. A no-op, not a failure.
            continue;
        };

        // The overlay is owned by `purge_session_artifacts`; only a fatal
        // refusal has anything left to record on the row.
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
            Err(refusal) => {
                let msg = refusal.message().to_string();
                // A retryable refusal removed nothing and already put the status
                // back; marking it failed would be a lie the user has to undo.
                if !refusal.is_retryable() {
                    mark_delete_error(state, &id, msg.clone()).await;
                }
                failed.push(WorkspaceDeleteFailure {
                    id: id.clone(),
                    error: msg,
                    retryable: refusal.is_retryable(),
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
        let (owner, selected) = {
            let instances = state.instances.read().await;
            let selected = session_ids
                .iter()
                .filter_map(|id| instances.iter().find(|row| &row.id == id))
                .map(|row| (row.source_profile.clone(), row.id.clone()))
                .collect();
            (
                instances.iter().find(|i| i.id == owner_id).cloned(),
                selected,
            )
        };
        if let Some(owner) = owner {
            if let Some(msg) = workspace_dirty_message(owner, selected).await {
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
                // Nothing was removed and every refusal is transient, so this
                // is a retryable conflict rather than a server error.
                if failed.iter().all(|f| f.retryable) {
                    tracing::warn!(target: "http.api.sessions", "workspace delete still settling: {msg}");
                    return (
                        StatusCode::CONFLICT,
                        Json(serde_json::json!({
                            "error": "teardown_pending",
                            "message": msg,
                            "failed": failed,
                        })),
                    )
                        .into_response();
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::test_support::build_test_app_state;

    async fn status_of(state: &AppState, id: &str) -> Option<Status> {
        state
            .instances
            .read()
            .await
            .iter()
            .find(|instance| instance.id == id)
            .map(|instance| instance.status)
    }

    /// A purge that removes nothing must put the memory-only `Deleting`
    /// overlay back, whichever way it declines. The overlay is applied inside
    /// `purge_session_artifacts`, so this drives the handler end to end and
    /// checks the property that matters: the row is never left in `Deleting`.
    ///
    /// The refusal a concurrent restore wins is not reproducible without
    /// orchestrating a generation race; the refactor is what removes that exit
    /// from the set of things a caller has to remember.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_refused_purge_never_leaves_the_row_greyed_out() {
        use crate::session::LifecycleOperation;

        let temp = tempfile::tempdir().unwrap();
        let _app_dir = crate::session::test_support::isolate_app_dir_at(temp.path());
        let profile = "overlay-busy";
        crate::session::create_profile(profile).unwrap();

        let mut instance = crate::session::Instance::new("Session", temp.path().to_str().unwrap());
        instance.id = "overlay-busy".to_string();
        instance.source_profile = profile.to_string();
        instance.status = Status::Idle;
        let id = instance.id.clone();
        let storage = crate::session::Storage::new_unwatched(profile).unwrap();
        storage
            .update(|instances, _| {
                instances.push(instance.clone());
                Ok(())
            })
            .unwrap();
        // A reservation of another operation makes the purge decline as busy.
        storage
            .update(|instances, _| {
                let row = instances.iter_mut().find(|i| i.id == id).unwrap();
                row.try_acquire_lifecycle_reservation(
                    LifecycleOperation::Trash,
                    crate::session::Instance::LIFECYCLE_RESERVATION_TTL,
                    chrono::Utc::now(),
                )
                .unwrap();
                Ok(())
            })
            .unwrap();

        let state = build_test_app_state(vec![instance]);
        let response = super::delete_session(
            axum::extract::State(state.clone()),
            axum::extract::Path(id.clone()),
            Some(axum::Json(super::DeleteSessionBody::default())),
        )
        .await;
        assert!(
            !response.into_response().status().is_success(),
            "a row held by another lifecycle operation must not be purged"
        );
        assert_ne!(
            status_of(&state, &id).await,
            Some(Status::Deleting),
            "a refused purge removes nothing, so the row must not stay greyed-out"
        );
    }

    /// A purge that meets a fresh lifecycle reservation is contention, not a
    /// failure: reporting it fatal turns the retry the first 409 asked for into
    /// a 500 the caller has no way to act on.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_contended_purge_stays_retryable() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        crate::session::create_profile("contended").unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let mut instance = crate::session::Instance::new("Contended", project.to_str().unwrap());
        instance.id = "contended-purge".to_string();
        instance.source_profile = "contended".to_string();
        let storage = Storage::new_unwatched("contended").unwrap();
        instance.storage_origin = Some(std::sync::Arc::new(storage.clone()));
        storage
            .update(|instances, _groups| {
                let mut held = instance.clone();
                held.try_acquire_lifecycle_reservation(
                    LifecycleOperation::Trash,
                    crate::session::Instance::LIFECYCLE_RESERVATION_TTL,
                    chrono::Utc::now(),
                )
                .unwrap();
                instances.push(held);
                Ok(())
            })
            .unwrap();

        let state = crate::server::test_support::build_test_app_state(vec![instance.clone()]);
        let refused = purge_session_artifacts(
            &state,
            "contended-purge",
            instance,
            &DeleteSessionBody::default(),
            None,
        )
        .await;

        assert!(
            matches!(&refused, Err(PurgeRefusal::Retryable(_))),
            "contention that clears must stay retryable, got {refused:?}"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn unknown_execution_history_refuses_purge_before_hooks_in_every_view() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        crate::session::create_profile("hooked").unwrap();
        let counter = temp.path().join("hook-runs");
        std::fs::write(
            crate::session::get_app_dir()
                .unwrap()
                .join("profiles/hooked/config.toml"),
            format!(
                "[hooks]\non_destroy = [\"echo run >> {}\"]\n",
                counter.display()
            ),
        )
        .unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let storage = Storage::new_unwatched("hooked").unwrap();
        for (id, view) in [
            ("terminal-history", crate::session::View::Terminal),
            ("structured-history", crate::session::View::Structured),
        ] {
            let mut instance = Instance::new(id, project.to_str().unwrap());
            instance.id = id.into();
            instance.source_profile = "hooked".into();
            instance.view = view;
            instance.runner_journal =
                crate::session::runner_journal::RunnerExecutionJournal::legacy_unknown();
            storage
                .update(|rows, _| {
                    rows.push(instance.clone());
                    Ok(())
                })
                .unwrap();
            let state = build_test_app_state(vec![instance.clone()]);
            let refused =
                purge_session_artifacts(&state, id, instance, &DeleteSessionBody::default(), None)
                    .await;
            assert!(
                refused.is_err(),
                "unknown history must refuse {view:?} purge"
            );
            let durable = storage
                .load()
                .unwrap()
                .into_iter()
                .find(|row| row.id == id)
                .unwrap();
            assert!(durable.lifecycle_reservation.is_none());
            assert_eq!(
                durable.view, view,
                "proof refusal must retain the current durable view"
            );
            assert_ne!(status_of(&state, id).await, Some(Status::Deleting));
            assert!(
                !counter.exists(),
                "proof refusal must precede the user's on_destroy hook"
            );
        }
    }

    #[tokio::test]
    async fn daemon_reconcile_preserves_peers_and_aborts_after_git_exit_23() {
        use std::os::unix::fs::PermissionsExt;
        for fail in [false, true] {
            let home = tempfile::tempdir().unwrap();
            let _home = crate::session::test_support::isolate_app_dir_at(home.path());
            let mut storages = Vec::new();
            let mut cached = Vec::new();
            let mut checkouts = Vec::new();
            for profile in 0..4 {
                let storage = Storage::new_unwatched(&format!("daemon-sweep-{profile}")).unwrap();
                for target in 0..3 {
                    let base = home.path().join(format!("repo-{profile}-{target}"));
                    let main = base.join("main");
                    let checkout = base.join("checkouts/feature");
                    std::fs::create_dir_all(checkout.parent().unwrap()).unwrap();
                    let repository = git2::Repository::init(&main).unwrap();
                    let signature = git2::Signature::now("Fixture", "fixture@example.com").unwrap();
                    let tree = repository
                        .find_tree(repository.index().unwrap().write_tree().unwrap())
                        .unwrap();
                    repository
                        .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
                        .unwrap();
                    let added = std::process::Command::new("git")
                        .args([
                            "worktree",
                            "add",
                            "-b",
                            "feature",
                            checkout.to_str().unwrap(),
                        ])
                        .current_dir(&main)
                        .output()
                        .unwrap();
                    assert!(
                        added.status.success(),
                        "{}",
                        String::from_utf8_lossy(&added.stderr)
                    );
                    std::fs::write(checkout.join("sentinel"), "keep").unwrap();
                    let mut row = Instance::new("Target", checkout.to_str().unwrap());
                    row.id = format!("target-{profile}-{target}");
                    row.source_profile = storage.profile().to_owned();
                    row.storage_origin = Some(Arc::new(storage.clone()));
                    row.worktree_info = Some(crate::session::WorktreeInfo {
                        branch: "feature".into(),
                        main_repo_path: main.to_string_lossy().into_owned(),
                        managed_by_aoe: true,
                        created_at: chrono::Utc::now(),
                        base_branch: None,
                    });
                    row.trash();
                    storage
                        .update(|rows, _| {
                            rows.push(row.clone());
                            Ok(())
                        })
                        .unwrap();
                    cached.push(row);
                    checkouts.push(checkout);
                }
                storages.push(storage);
            }
            let peer = Storage::new_unwatched("peer").unwrap();
            let peer_path = home.path().join("peer-checkout");
            std::fs::create_dir(&peer_path).unwrap();
            std::fs::write(peer_path.join("sentinel"), "peer must survive").unwrap();
            peer.update(|rows, _| {
                rows.push(Instance::new("Peer", peer_path.to_str().unwrap()));
                Ok(())
            })
            .unwrap();
            let peer_before = std::fs::read(peer.sessions_path()).unwrap();
            let state = build_test_app_state(cached);
            let calls = home.path().join("git-failures");
            let bin = home.path().join("bin");
            std::fs::create_dir(&bin).unwrap();
            let real_git = std::process::Command::new("sh")
                .args(["-c", "command -v git"])
                .output()
                .unwrap();
            assert!(real_git.status.success());
            let real_git = String::from_utf8(real_git.stdout).unwrap();
            let script = bin.join("git");
            std::fs::write(&script, format!(
                "#!/bin/sh\nif [ \"$1\" = worktree ] && [ \"$2\" = move ]; then printf \"exit23 %s\\n\" \"$*\" >> \"{}\"; exit 23; fi\nexec \"{}\" \"$@\"\n",
                calls.display(), real_git.trim(),
            )).unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            let _path = fail.then(|| crate::session::test_support::path_prepended(&bin));
            reconcile_trashed_worktrees(&state).await;
            assert_eq!(std::fs::read(peer.sessions_path()).unwrap(), peer_before);
            assert_eq!(
                std::fs::read_to_string(peer_path.join("sentinel")).unwrap(),
                "peer must survive"
            );
            if fail {
                let attempts = std::fs::read_to_string(&calls).unwrap();
                assert_eq!(
                    attempts.lines().count(),
                    1,
                    "the failed real Git move must invalidate the pass before any next target"
                );
                assert!(attempts.starts_with("exit23 worktree move "));
                for checkout in &checkouts {
                    assert_eq!(
                        std::fs::read_to_string(checkout.join("sentinel")).unwrap(),
                        "keep"
                    );
                }
            } else {
                for storage in &storages {
                    for row in storage.load().unwrap() {
                        assert!(
                            row.project_path.contains(".aoe-trash"),
                            "{}",
                            row.project_path
                        );
                        assert_eq!(
                            std::fs::read_to_string(
                                std::path::Path::new(&row.project_path).join("sentinel")
                            )
                            .unwrap(),
                            "keep"
                        );
                        assert!(row.lifecycle_reservation.is_none());
                    }
                }
                assert_eq!(
                    state
                        .instances
                        .read()
                        .await
                        .iter()
                        .filter(|row| row.project_path.contains(".aoe-trash"))
                        .count(),
                    12
                );
            }
        }
    }
}
