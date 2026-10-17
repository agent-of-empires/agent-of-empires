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
    // Submission and instance admission are ordered before quiescence.
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(id)
        .await
    else {
        return Err(AttachError::NotFound);
    };
    let state = Arc::clone(state);
    let id = id.to_owned();
    let repo_path = repo_path.to_owned();
    tokio::spawn(
        async move { complete_attach(&state, &id, &repo_path, on_existing, _submission).await },
    )
    .await
    .map_err(|error| AttachError::Rejected(format!("attach task panicked: {error}")))?
}

async fn complete_attach(
    state: &Arc<AppState>,
    id: &str,
    repo_path: &Path,
    on_existing: ExistingBranch,
    _submission: tokio::sync::OwnedMutexGuard<()>,
) -> Result<(AttachOutcome, WorkerOutcome), AttachError> {
    let inst_lock = state.instance_lock(id).await;
    // Release these gates before Git hooks can reenter this daemon.
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
            let message = format!("could not stop the current worker: {e}");
            let message = run_blocking(storage.clone(), move |_| {
                Ok(format!(
                    "{:#}",
                    crate::session::attach_project::cancel_attach(&plan, anyhow::anyhow!(message))
                ))
            })
            .await
            .map_err(AttachError::Rejected)?;
            return Err(AttachError::Rejected(message));
        }
    }

    let (plan, quiesced) = if restarts {
        // The supervisor's worker is gone; quiescence handles the remaining pane.
        match run_blocking(storage.clone(), {
            move |storage| {
                let quiesced =
                    crate::session::attach_project::quiesce_for_conversion(storage, &plan)
                        .map_err(|error| {
                            format!(
                                "{:#}",
                                crate::session::attach_project::cancel_attach(&plan, error)
                            )
                        })?;
                Ok((plan, quiesced))
            }
        })
        .await
        {
            Ok(result) => result,
            Err(error) => {
                restore_after_failure(
                    state,
                    &instance,
                    storage.clone(),
                    crate::session::attach_project::Quiesced {
                        attach_generation: instance.lifecycle_generation,
                        ..Default::default()
                    },
                    was_running,
                )
                .await;
                return Err(AttachError::Rejected(error));
            }
        }
    } else {
        (
            plan,
            crate::session::attach_project::Quiesced {
                attach_generation: instance.lifecycle_generation,
                ..Default::default()
            },
        )
    };

    drop(_guard);
    drop(_submission);
    let id_owned = id.to_string();
    let admitted_instance = instance.clone();
    let (result, quiesced) = run_blocking(storage.clone(), move |storage| {
        let result = crate::session::attach_project::attach_planned(
            storage,
            &id_owned,
            &admitted_instance,
            plan,
        )
        .map_err(|error| format!("{error:#}"));
        Ok((result, quiesced))
    })
    .await
    .map_err(AttachError::Rejected)?;
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(id)
        .await
    else {
        return Err(AttachError::NotFound);
    };
    let _guard = inst_lock.lock().await;
    let outcome = match result {
        Ok(outcome) => outcome,
        Err(error) => {
            restore_after_failure(
                state,
                &instance,
                storage.clone(),
                quiesced,
                was_running && restarts,
            )
            .await;
            return Err(AttachError::Rejected(error));
        }
    };
    mirror_conversion(state, storage.clone(), &instance, &outcome, quiesced).await?;

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

async fn mirror_conversion(
    state: &Arc<AppState>,
    storage: Arc<Storage>,
    original: &crate::session::Instance,
    outcome: &AttachOutcome,
    quiesced: crate::session::attach_project::Quiesced,
) -> Result<(), AttachError> {
    let mut expected = original.clone();
    expected.workspace_info = Some(outcome.workspace_info.clone());
    if let Some(path) = &outcome.moved_to {
        expected.project_path = path.clone();
        expected.worktree_info = None;
    }
    let generation = quiesced.attach_generation;
    let current = run_blocking(storage, move |storage| {
        #[cfg(test)]
        if let Some(sender) = BEFORE_MIRROR_ROOT.lock().take() {
            let _ = sender.send(());
        }
        let root = crate::session::storage::acquire_ownership_read()
            .map_err(|error| format!("{error:#}"))?;
        let _lifecycle = storage
            .acquire_instance_lifecycle_lock_with_ownership(&root, &expected.id)
            .map_err(|error| format!("{error:#}"))?;
        let row = storage
            .load_strict_for_worktree_ownership()
            .map_err(|error| format!("{error:#}"))?
            .into_iter()
            .find(|row| row.id == expected.id)
            .ok_or("session disappeared after attach")?;
        if !crate::session::attach_project::same_checkout(&row, &expected)
            || row.lifecycle_generation != generation
            || row.lifecycle_reservation.is_some()
        {
            return Err("session changed after attach".to_owned());
        }
        Ok(row)
    })
    .await
    .map_err(AttachError::Rejected)?;
    let mut instances = state.instances.write().await;
    let inst = instances
        .iter_mut()
        .find(|inst| inst.id == current.id && inst.created_at == current.created_at)
        .ok_or(AttachError::NotFound)?;
    if inst.lifecycle_generation > current.lifecycle_generation
        || inst.source_profile != original.source_profile
    {
        return Err(AttachError::Rejected(
            "session changed after attach".to_owned(),
        ));
    }
    inst.lifecycle_generation = current.lifecycle_generation;
    inst.lifecycle_reservation = None;
    if let (Some(live), Some(durable)) = (&mut inst.sandbox_info, current.sandbox_info) {
        live.container_id = durable.container_id;
        live.container_workdir = durable.container_workdir;
    }
    inst.workspace_info = current.workspace_info;
    inst.project_path = current.project_path;
    inst.worktree_info = current.worktree_info;
    Ok(())
}

#[cfg(test)]
static BEFORE_MIRROR_ROOT: parking_lot::Mutex<Option<tokio::sync::oneshot::Sender<()>>> =
    parking_lot::Mutex::new(None);

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
        // This strict reader shares Launch admission's namespace fence.
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

    let spawned = state
        .acp_supervisor
        .spawn(
            request,
            crate::process::worker_registry::ResumeAuthority::Explicit,
        )
        .await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::test_support as support;

    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial]
    async fn cancelled_http_awaiter_keeps_initial_attach_admission_owned() {
        let _app = crate::session::test_support::isolate_app_dir();
        let directories = tempfile::tempdir().unwrap();
        let initialize = |path: &Path| {
            let repo = git2::Repository::init(path).unwrap();
            let signature = git2::Signature::now("fixture", "fixture@example.invalid").unwrap();
            let tree = repo.index().unwrap().write_tree().unwrap();
            repo.commit(
                Some("HEAD"),
                &signature,
                &signature,
                "base",
                &repo.find_tree(tree).unwrap(),
                &[],
            )
            .unwrap();
            let branch = repo.head().unwrap().shorthand().unwrap().to_owned();
            branch
        };
        let primary = directories.path().join("primary");
        let additional = directories.path().join("additional");
        let base = initialize(&primary);
        initialize(&additional);
        let workspace = directories.path().join("workspace");
        let checkout = workspace.join("primary");
        crate::git::GitWorktree::new(primary.clone())
            .unwrap()
            .with_init_submodules(false)
            .create_worktree("topic", &checkout, true, Some(&base))
            .unwrap();
        let mut instance =
            crate::session::Instance::new("attach cancellation", workspace.to_str().unwrap());
        instance.source_profile = "default".into();
        instance.view = crate::session::View::Structured;
        instance.status = crate::session::Status::Stopped;
        instance.workspace_info = Some(crate::session::WorkspaceInfo {
            branch: "topic".into(),
            workspace_dir: workspace.to_string_lossy().into_owned(),
            repos: vec![crate::session::WorkspaceRepo {
                name: "primary".into(),
                source_path: primary.to_string_lossy().into_owned(),
                branch: "topic".into(),
                worktree_path: checkout.to_string_lossy().into_owned(),
                main_repo_path: primary.to_string_lossy().into_owned(),
                managed_by_aoe: true,
                branch_preexisting: false,
                base_branch: Some(base),
                base_branch_override: None,
            }],
            created_at: chrono::Utc::now(),
            cleanup_on_delete: true,
        });
        support::seed_instances_on_disk_for_test("default", vec![instance.clone()]);
        let storage = Storage::open_unwatched("default").unwrap();
        let state = support::build_test_app_state(vec![instance.clone()]);
        let root = crate::session::storage::acquire_ownership_lock().unwrap();
        let (contended_tx, contended_rx) = std::sync::mpsc::channel();
        let observer = crate::session::storage::observe_root_contention_for_test(contended_tx);
        let (mirroring_tx, mirroring_rx) = tokio::sync::oneshot::channel();
        *BEFORE_MIRROR_ROOT.lock() = Some(mirroring_tx);
        let awaiter = tokio::spawn({
            let state = Arc::clone(&state);
            let id = instance.id.clone();
            async move { attach_project(&state, &id, &additional, ExistingBranch::Refuse).await }
        });
        tokio::task::spawn_blocking(move || {
            contended_rx.recv_timeout(std::time::Duration::from_secs(3))
        })
        .await
        .unwrap()
        .expect("attach must reach its initial blocking admission");
        awaiter.abort();
        assert!(awaiter.await.unwrap_err().is_cancelled());
        drop(root);
        drop(observer);
        tokio::time::timeout(std::time::Duration::from_secs(5), mirroring_rx)
            .await
            .unwrap()
            .expect("owned attach must reach publication after HTTP cancellation");
        let instance_lock = state.instance_lock(&instance.id).await;
        let _terminal = instance_lock.lock().await;
        let rows = storage.load().unwrap();
        assert!(rows[0].lifecycle_reservation.is_none());
        assert_eq!(rows[0].workspace_info.as_ref().unwrap().repos.len(), 2);
        assert!(crate::session::move_journal::scan().entries.is_empty());
        assert_eq!(
            state.instances.read().await[0]
                .workspace_info
                .as_ref()
                .unwrap()
                .repos
                .len(),
            2
        );
    }

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
    fn restart_commit_releases_changed_checkout_but_not_replacement() {
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

    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial]
    async fn mirror_conversion_waits_for_root_without_holding_cache_and_rejects_newer_identity() {
        let _app = crate::session::test_support::isolate_app_dir();
        let storage = Arc::new(Storage::new_unwatched("default").unwrap());
        for replacement in ["none", "incarnation", "generation", "profile"] {
            let mut original = crate::session::Instance::new("cache title", "/before");
            original.source_profile = "default".into();
            let repo = crate::session::WorkspaceRepo {
                name: "repo".into(),
                source_path: "/repo".into(),
                branch: "topic".into(),
                worktree_path: "/after/repo".into(),
                main_repo_path: "/repo".into(),
                managed_by_aoe: true,
                branch_preexisting: false,
                base_branch: None,
                base_branch_override: None,
            };
            let workspace = crate::session::WorkspaceInfo {
                branch: "topic".into(),
                workspace_dir: "/after".into(),
                repos: vec![repo.clone()],
                created_at: chrono::Utc::now(),
                cleanup_on_delete: true,
            };
            let outcome = AttachOutcome {
                repo,
                warnings: Vec::new(),
                moved_to: Some("/after".into()),
                workspace_info: workspace.clone(),
            };
            let mut durable = original.clone();
            durable.title = "disk title".into();
            durable.project_path = "/after".into();
            durable.workspace_info = Some(workspace);
            storage
                .update(|rows, _| {
                    *rows = vec![durable];
                    Ok(())
                })
                .unwrap();
            let state = support::build_test_app_state(vec![original.clone()]);
            let root = crate::session::storage::acquire_ownership_lock().unwrap();
            let (entered, gate) = tokio::sync::oneshot::channel();
            *BEFORE_MIRROR_ROOT.lock() = Some(entered);
            let task = tokio::spawn({
                let state = Arc::clone(&state);
                let storage = Arc::clone(&storage);
                async move {
                    mirror_conversion(&state, storage, &original, &outcome, Default::default())
                        .await
                }
            });
            tokio::time::timeout(std::time::Duration::from_secs(2), gate)
                .await
                .unwrap()
                .unwrap();
            let cache = tokio::time::timeout(
                std::time::Duration::from_millis(100),
                state.instances.read(),
            )
            .await
            .expect("cache readers must progress while mirror waits on filesystem ownership");
            assert_eq!(cache[0].project_path, "/before");
            drop(cache);
            if replacement != "none" {
                let mut cache = state.instances.write().await;
                match replacement {
                    "incarnation" => cache[0].created_at += chrono::Duration::seconds(1),
                    "generation" => cache[0].lifecycle_generation += 1,
                    "profile" => cache[0].source_profile = "new-profile".into(),
                    _ => unreachable!(),
                }
                cache[0].title = "replacement".into();
            }
            drop(root);
            let result = task.await.unwrap();
            let cache = state.instances.read().await;
            if replacement != "none" {
                if replacement == "incarnation" {
                    assert!(matches!(result, Err(AttachError::NotFound)));
                } else {
                    assert!(matches!(result, Err(AttachError::Rejected(_))));
                }
                assert_eq!(cache[0].project_path, "/before");
                assert_eq!(cache[0].title, "replacement");
            } else {
                result.unwrap();
                assert_eq!(cache[0].project_path, "/after");
                assert_eq!(cache[0].title, "cache title");
                assert!(cache[0].workspace_info.is_some());
            }
        }
    }
}
