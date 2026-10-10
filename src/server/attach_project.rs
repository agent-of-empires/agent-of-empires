//! Daemon-side orchestration for attaching a repo to a live session.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::session::attach_project::{AttachOutcome, ExistingBranch};
use crate::session::Storage;

use super::AppState;

/// What happened to the session's worker after the repo was recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkerOutcome {
    /// Nothing had to be stopped, because nothing had to move.
    NotRunning,
    /// Stopped for the conversion and started again against the stored ACP session id, so
    /// the transcript is intact and the agent comes up in the workspace.
    Restarted,
    /// The repo is recorded but the session could not be started again.
    RestartFailed(String),
}

#[derive(Debug)]
pub(crate) enum AttachError {
    NotFound,
    /// A turn is in flight.
    TurnInFlight,
    /// Validation, git, or persistence failure from the session-domain half.
    Rejected(String),
}

impl std::fmt::Display for AttachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AttachError::NotFound => write!(f, "session not found"),
            AttachError::TurnInFlight => write!(
                f,
                "the agent is mid-turn; wait for it to finish or cancel the turn, \
                 then attach the project again"
            ),
            AttachError::Rejected(m) => write!(f, "{m}"),
        }
    }
}

/// Attach `repo_path` to session `id`, stopping and starting it around the
/// conversion when the conversion moves it.
pub(crate) async fn attach_project(
    state: &Arc<AppState>,
    id: &str,
    repo_path: &Path,
    on_existing: ExistingBranch,
) -> Result<(AttachOutcome, WorkerOutcome), AttachError> {
    // `instance_lock` alone stopped being the whole barrier once prompt submission moved to
    // its own authority.
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(id)
        .await
    else {
        return Err(AttachError::NotFound);
    };
    let inst_lock = state.instance_lock(id).await;
    // Held across the turn probe, the stop, the persist and the start.
    let _guard = inst_lock.lock().await;

    let (profile, was_running, original) = {
        let instances = state.instances.read().await;
        let inst = instances
            .iter()
            .find(|i| i.id == id)
            .ok_or(AttachError::NotFound)?;
        (
            inst.source_profile.clone(),
            matches!(
                state.acp_supervisor.worker_state(id).await,
                crate::daemon::AcpWorkerState::Running
            ),
            inst.clone(),
        )
    };

    if was_running {
        let store = state.acp_event_store.clone();
        let id_owned = id.to_string();
        let in_flight = tokio::task::spawn_blocking(move || store.has_in_flight_turn(&id_owned))
            .await
            .unwrap_or(false);
        if in_flight {
            return Err(AttachError::TurnInFlight);
        }
    }

    // Validation first, with nothing stopped and nothing written.
    let (storage, instance, plan, restarts) = {
        let profile = profile.clone();
        let id_owned = id.to_string();
        let repo = repo_path.to_path_buf();
        let file_watch = state.file_watch.clone();
        tokio::task::spawn_blocking(move || {
            let storage = Storage::open(&profile, file_watch).map_err(|e| e.to_string())?;
            let instances = storage.load().map_err(|e| format!("{e:#}"))?;
            let instance = instances
                .into_iter()
                .find(|i| i.id == id_owned)
                .ok_or_else(|| format!("session not found: {id_owned}"))?;
            if !crate::session::attach_project::same_checkout(&instance, &original) {
                return Err("session checkout changed before attach admission".to_string());
            }
            let mut plan =
                crate::session::attach_project::plan(&instance, &profile, &repo, on_existing)
                    .map_err(|e| format!("{e:#}"))?;
            let instance = crate::session::attach_project::reserve_attach(&storage, &mut plan)
                .map_err(|e| format!("{e:#}"))?;
            let restarts =
                crate::session::attach_project::needs_restart(&plan, instance.is_sandboxed());
            Ok::<_, String>((Arc::new(storage), instance, plan, restarts))
        })
        .await
        .map_err(|e| AttachError::Rejected(format!("attach task panicked: {e}")))?
        .map_err(AttachError::Rejected)?
    };

    // Order is load-bearing.
    if restarts && was_running {
        if let Err(e) = state
            .acp_supervisor
            .shutdown_and_wait(id, std::time::Duration::from_secs(5))
            .await
        {
            return Err(AttachError::Rejected(format!(
                "could not stop the current worker: {e}"
            )));
        }
    }

    let (plan, quiesced) = if restarts {
        // The worker registry entry is already gone, so this takes down the tmux
        // pane and the sandbox container and reports only what it stopped.
        match run_blocking(storage.clone(), {
            move |storage| {
                let quiesced =
                    crate::session::attach_project::quiesce_for_conversion(storage, &plan)
                        .map_err(|e| format!("{e:#}"))?;
                Ok((plan, quiesced))
            }
        })
        .await
        {
            Ok((plan, q)) => {
                clear_sandbox_pins(state, id).await;
                (plan, q)
            }
            Err(e) => return Err(AttachError::Rejected(e)),
        }
    } else {
        (plan, crate::session::attach_project::Quiesced::default())
    };

    let outcome = {
        let id_owned = id.to_string();
        let admitted_instance = instance.clone();
        match run_blocking(storage.clone(), move |storage| {
            crate::session::attach_project::attach_planned(
                storage,
                &id_owned,
                &admitted_instance,
                plan,
            )
            .map_err(|e| format!("{e:#}"))
        })
        .await
        {
            Ok(outcome) => outcome,
            Err(e) => {
                // Put the session back.
                restore_after_failure(
                    state,
                    &instance,
                    storage.clone(),
                    quiesced,
                    was_running && restarts,
                )
                .await;
                return Err(AttachError::Rejected(e));
            }
        }
    };

    // Persist landed, so mirror it into the live state before anything reads the instance
    // again.
    mirror_conversion(state, id, &outcome).await;

    if !restarts {
        return Ok((outcome, WorkerOutcome::NotRunning));
    }

    // The tmux pane, when there was one.
    let pane_warnings = run_blocking(storage.clone(), {
        let instance = instance.clone();
        let outcome = outcome.clone();
        move |storage| {
            Ok(crate::session::attach_project::resume_after_conversion(
                storage,
                &instance,
                quiesced,
                Some(&outcome),
            ))
        }
    })
    .await
    .unwrap_or_else(|e| vec![e]);
    if let Some(first) = pane_warnings.into_iter().next() {
        return Ok((outcome, WorkerOutcome::RestartFailed(first)));
    }

    if !was_running {
        return Ok((outcome, WorkerOutcome::NotRunning));
    }
    let worker = spawn_worker(state, storage, &instance, quiesced, Some(&outcome)).await;
    Ok((outcome, worker))
}

/// Run a closure that needs a `Storage` for this profile on a blocking thread.
async fn run_blocking<T, F>(storage: Arc<Storage>, f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&Storage) -> Result<T, String> + Send + 'static,
{
    tokio::task::spawn_blocking(move || f(&storage))
        .await
        .unwrap_or_else(|e| Err(format!("attach task panicked: {e}")))
}

/// Bring the session back after an attach that failed with it stopped.
async fn restore_after_failure(
    state: &Arc<AppState>,
    instance: &crate::session::Instance,
    storage: Arc<Storage>,
    quiesced: crate::session::attach_project::Quiesced,
    respawn_worker: bool,
) {
    let id = instance.id.clone();
    let resume_original = instance.clone();
    let warnings = run_blocking(storage.clone(), move |storage| {
        Ok(crate::session::attach_project::resume_after_conversion(
            storage,
            &resume_original,
            quiesced,
            None,
        ))
    })
    .await
    .unwrap_or_else(|e| vec![e]);
    let refused = !warnings.is_empty();
    for warning in warnings {
        tracing::warn!(
            target: "session.attach",
            session = %id,
            "could not restore the session after a failed attach: {warning}"
        );
    }
    if respawn_worker && !refused {
        if let WorkerOutcome::RestartFailed(e) =
            spawn_worker(state, storage, instance, quiesced, None).await
        {
            tracing::warn!(
                target: "session.attach",
                session = %id,
                "could not restart the worker after a failed attach: {e}"
            );
        }
    }
}

/// Mirror the persisted conversion into the live instance map.
async fn mirror_conversion(state: &Arc<AppState>, id: &str, outcome: &AttachOutcome) {
    let mut instances = state.instances.write().await;
    if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
        inst.workspace_info = Some(outcome.workspace_info.clone());
        if let Some(moved_to) = &outcome.moved_to {
            inst.project_path = moved_to.clone();
            inst.worktree_info = None;
        }
    }
}

/// Start the session's worker again, in the workspace the conversion produced.
async fn spawn_worker(
    state: &Arc<AppState>,
    storage: Arc<Storage>,
    original: &crate::session::Instance,
    quiesced: crate::session::attach_project::Quiesced,
    outcome: Option<&AttachOutcome>,
) -> WorkerOutcome {
    let original = original.clone();
    let outcome = outcome.cloned();
    let admitted = run_blocking(storage.clone(), move |storage| {
        let ownership =
            crate::session::storage::acquire_ownership_lock().map_err(|e| format!("{e:#}"))?;
        let _lifecycle = storage
            .acquire_instance_lifecycle_lock_with_ownership(&ownership, &original.id)
            .map_err(|e| format!("{e:#}"))?;
        let generation = quiesced
            .attach_generation
            .checked_add(u64::from(
                quiesced.pane_was_live && !original.is_structured(),
            ))
            .ok_or_else(|| "attach restart generation overflow".to_string())?;
        // The strict reader and original profile witness run under the same fence as Launch admission.
        let mut expected = original.clone();
        if let Some(outcome) = &outcome {
            expected.workspace_info = Some(outcome.workspace_info.clone());
            if let Some(path) = &outcome.moved_to {
                expected.project_path = path.clone();
                expected.worktree_info = None;
            }
        }
        storage
            .load_strict_for_worktree_ownership()
            .map_err(|e| format!("{e:#}"))?;
        storage
            .update_with_ownership(&ownership, |rows, _| {
                let row = rows
                    .iter_mut()
                    .find(|row| row.id == expected.id)
                    .ok_or_else(|| anyhow::anyhow!("session disappeared before worker restart"))?;
                anyhow::ensure!(
                    crate::session::attach_project::same_checkout(row, &expected)
                        && row.lifecycle_generation == generation
                        && row.lifecycle_reservation.is_none()
                        && !row.is_trashed()
                        && !row.is_archived(),
                    "session changed before worker restart"
                );
                row.try_acquire_lifecycle_reservation(
                    crate::session::LifecycleOperation::Launch,
                    crate::session::Instance::LIFECYCLE_RESERVATION_TTL,
                    chrono::Utc::now(),
                )?;
                Ok(row.clone())
            })
            .map_err(|e| format!("{e:#}"))
    })
    .await;
    let inst = match admitted {
        Ok(instance) => instance,
        Err(error) => return WorkerOutcome::RestartFailed(error),
    };
    let id = inst.id.as_str();
    let request = {
        crate::acp::supervisor::SpawnRequest {
            session_id: id.to_string(),
            agent: inst.tool.clone(),
            tool: inst.tool.clone(),
            // The mirrored instance, so this is the workspace directory when the
            // attach converted the session, not the path it started from.
            cwd: PathBuf::from(&inst.project_path),
            additional_dirs: vec![],
            provider_env: vec![],
            provider: inst.agent_provider.clone(),
            model: inst.agent_model.clone(),
            effort: None,
            effort_explicit: false,
            // The whole point of taking the session down and bringing it back.
            stored_acp_session_id: inst.acp_session_id.clone(),
            // Threaded for the same continuity reason as the stored session id.
            fork_from: inst.fork_pending.clone(),
            sandbox_continuation: crate::acp::supervisor::SandboxContinuation::Persisted,
            sandbox_info: inst.sandbox_info.clone(),
            source_profile: Some(inst.source_profile.clone()),
            yolo_mode: inst.yolo_mode,
            acp_mode_id: inst.acp_mode_id.clone(),
            agent_command_override: crate::server::acp_reconciler::command_override_for_spawn(
                &inst.tool,
                &inst.command,
            ),
            seed_history_replay: false,
            claude_store_pin: inst.selected_claude_store_pin(),
        }
    };

    let spawned = state.acp_supervisor.spawn(request).await;
    let committed = run_blocking(storage, move |storage| {
        complete_worker_restart(storage, &inst).map_err(|error| format!("{error:#}"))
    })
    .await;
    match (spawned, committed) {
        (Ok(()), Ok(true)) => WorkerOutcome::Restarted,
        (Ok(()), Ok(false)) => {
            WorkerOutcome::RestartFailed("session checkout changed during worker restart".into())
        }
        (Err(error), _) => WorkerOutcome::RestartFailed(format!("worker respawn failed: {error}")),
        (Ok(()), Err(error)) => WorkerOutcome::RestartFailed(error),
    }
}

fn complete_worker_restart(
    storage: &Storage,
    instance: &crate::session::Instance,
) -> anyhow::Result<bool> {
    let mut cleanup = crate::session::claim::ReservationCleanup::new(
        storage,
        instance,
        crate::session::LifecycleOperation::Launch,
        instance.lifecycle_generation,
    );
    let ownership = crate::session::storage::acquire_ownership_lock()?;
    let _lifecycle =
        storage.acquire_instance_lifecycle_lock_with_ownership(&ownership, &instance.id)?;
    storage.load_strict_for_worktree_ownership()?;
    let checkout_matches = storage.update_with_ownership(&ownership, |rows, _| {
        let row = rows
            .iter_mut()
            .find(|row| row.id == instance.id)
            .ok_or_else(|| anyhow::anyhow!("session disappeared during worker restart"))?;
        anyhow::ensure!(
            row.created_at == instance.created_at,
            "session incarnation changed during worker restart"
        );
        let checkout_matches = crate::session::attach_project::same_checkout(row, instance);
        anyhow::ensure!(
            row.release_lifecycle_reservation_if_owned(
                crate::session::LifecycleOperation::Launch,
                instance.lifecycle_generation,
            ),
            "worker restart reservation was superseded"
        );
        Ok(checkout_matches)
    })?;
    cleanup.disarm();
    Ok(checkout_matches)
}

/// Drop the create-time container pins from the live instance.
async fn clear_sandbox_pins(state: &Arc<AppState>, id: &str) {
    let mut instances = state.instances.write().await;
    if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
        if let Some(sandbox) = inst.sandbox_info.as_mut() {
            sandbox.container_id = None;
            sandbox.container_workdir = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::test_support as support;

    /// #4116: an archive committed while the restarted worker's `before_session` hook runs
    /// refuses the respawn instead of launching the archived row.
    #[tokio::test]
    #[serial_test::serial]
    async fn worker_restart_refuses_a_row_archived_while_the_hook_runs() {
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let barrier = tempfile::tempdir().unwrap();
        let hook = support::install_blocking_before_session_hook(barrier.path(), "attach");
        let mut inst =
            crate::session::Instance::new("attach-4116", barrier.path().to_str().unwrap());
        inst.view = crate::session::View::Structured;
        inst.status = crate::session::Status::Idle;
        let id = inst.id.clone();
        let profile = inst.source_profile.clone();
        support::seed_instances_on_disk_for_test(&profile, vec![inst.clone()]);
        let storage = Arc::new(Storage::new_unwatched(&profile).unwrap());
        let quiesced = crate::session::attach_project::Quiesced {
            attach_generation: inst.lifecycle_generation,
            ..Default::default()
        };
        let (launcher, launches) = support::counting_failing_launcher();
        let state = support::build_test_app_state_with_launcher(vec![inst.clone()], launcher);

        let restart = tokio::spawn({
            let state = Arc::clone(&state);
            async move { spawn_worker(&state, storage, &inst, quiesced, None).await }
        });
        let archived = support::archive_while_hook_waits(&hook, &profile, |row| row.id == id).await;
        let outcome = restart.await.unwrap();

        assert!(archived, "before_session hook did not run");
        match outcome {
            WorkerOutcome::RestartFailed(error) => {
                assert!(error.contains("archived"), "{error}")
            }
            _ => panic!("an archived row must not restart its worker"),
        }
        assert_eq!(launches.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    #[serial_test::serial]
    fn ownership_review_restart_commit_releases_changed_checkout_but_not_replacement() {
        for change in ["checkout", "generation", "incarnation", "corrupt"] {
            let _home = crate::session::test_support::isolate_app_dir();
            let storage = Storage::new_unwatched("default").unwrap();
            let mut original = crate::session::Instance::new("restart", "/checkout/original");
            original.view = crate::session::View::Structured;
            original
                .try_acquire_lifecycle_reservation(
                    crate::session::LifecycleOperation::Launch,
                    crate::session::Instance::LIFECYCLE_RESERVATION_TTL,
                    chrono::Utc::now(),
                )
                .unwrap();
            let mut current = original.clone();
            match change {
                "checkout" => current.project_path = "/checkout/changed".into(),
                "generation" => {
                    current.release_lifecycle_reservation_if_owned(
                        crate::session::LifecycleOperation::Launch,
                        original.lifecycle_generation,
                    );
                    current
                        .try_acquire_lifecycle_reservation(
                            crate::session::LifecycleOperation::Launch,
                            crate::session::Instance::LIFECYCLE_RESERVATION_TTL,
                            chrono::Utc::now(),
                        )
                        .unwrap();
                }
                "incarnation" => current.created_at += chrono::Duration::seconds(1),
                _ => {}
            }
            storage
                .update(|rows, _| {
                    rows.push(current);
                    Ok(())
                })
                .unwrap();
            if change == "corrupt" {
                std::fs::write(storage.sessions_path(), b"[{broken").unwrap();
            }
            let before = std::fs::read(storage.sessions_path()).unwrap();
            let result = complete_worker_restart(&storage, &original);
            if change == "checkout" {
                assert!(!result.unwrap());
                let row = storage
                    .load_strict_for_worktree_ownership()
                    .unwrap()
                    .remove(0);
                assert_eq!(row.project_path, "/checkout/changed");
                assert!(row.lifecycle_reservation.is_none());
            } else {
                assert!(result.is_err(), "{change}");
                assert_eq!(
                    std::fs::read(storage.sessions_path()).unwrap(),
                    before,
                    "{change}"
                );
            }
        }
    }
}
