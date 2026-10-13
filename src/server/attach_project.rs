//! Daemon-side orchestration for attaching a repo to a live session.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::session::attach_project::{AttachOutcome, ExistingBranch};

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
    Rejected(anyhow::Error),
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
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(id)
        .await
    else {
        return Err(AttachError::NotFound);
    };
    let lock = state.instance_lock(id).await;
    let _guard = lock.lock().await;
    let profile = state
        .instances
        .read()
        .await
        .iter()
        .find(|row| row.id == id)
        .map(|row| row.source_profile.clone())
        .ok_or(AttachError::NotFound)?;
    let was_running = matches!(
        state.acp_supervisor.worker_state(id).await,
        crate::daemon::AcpWorkerState::Running
    );
    if was_running {
        let store = state.acp_event_store.clone();
        let probe_id = id.to_owned();
        let in_flight = tokio::task::spawn_blocking(move || store.has_in_flight_turn(&probe_id))
            .await
            .map_err(|error| AttachError::Rejected(error.into()))?;
        if in_flight {
            return Err(AttachError::TurnInFlight);
        }
    }
    // Plan through the bound native store. Validation happens before stopping
    // workers or touching git; attach_planned repeats it under lifecycle exclusion.
    let plan_id = id.to_owned();
    let repo = repo_path.to_path_buf();
    let (instance, plan, restarts) = run_blocking(state, &profile, id, move |storage| {
        let instance = storage
            .load()?
            .into_iter()
            .find(|row| row.id == plan_id)
            .ok_or(crate::session::SessionGone)?;
        let plan = crate::session::attach_project::plan(
            &instance,
            storage.storage().profile(),
            &repo,
            on_existing,
        )?;
        let restarts =
            crate::session::attach_project::needs_restart(&plan, instance.is_sandboxed());
        Ok((instance, plan, restarts))
    })
    .await
    .map_err(|error| {
        if error.is::<crate::session::SessionGone>() {
            AttachError::NotFound
        } else {
            AttachError::Rejected(error)
        }
    })?;
    if restarts && was_running {
        state
            .acp_supervisor
            .shutdown_and_wait(id, std::time::Duration::from_secs(5))
            .await
            .map_err(|error| {
                AttachError::Rejected(anyhow::anyhow!(
                    "could not stop the current worker: {error}"
                ))
            })?;
    }
    let quiesced = if restarts {
        let instance = instance.clone();
        run_blocking(state, &profile, id, move |storage| {
            crate::session::attach_project::quiesce_for_conversion(storage, &instance)
        })
        .await
        .map_err(AttachError::Rejected)?
    } else {
        crate::session::attach_project::Quiesced::default()
    };
    let attach_id = id.to_owned();
    let result = run_blocking(state, &profile, id, move |storage| {
        crate::session::attach_project::attach_planned(
            storage,
            &attach_id,
            &instance,
            plan,
            quiesced.lifecycle_generation,
        )
    })
    .await;
    let mut outcome = match result {
        Ok(outcome) => outcome,
        Err(error) => {
            // Only restore a rejected/precommit conversion. A durable write
            // whose canonical adoption failed must not be 'undone' on disk.
            if !error.is::<crate::session::SessionCommitApplied>()
                && !error.is::<crate::session::NativeStoreUnavailable>()
                && !error.is::<crate::session::attach_project::AttachRollbackIncomplete>()
            {
                let warnings =
                    restore_after_failure(state, id, &profile, quiesced, was_running && restarts)
                        .await;
                if !warnings.is_empty() {
                    return Err(AttachError::Rejected(error.context(format!(
                        "restoring the previous session after the failed attachment also failed: {}", warnings.join("; ")))));
                }
            }
            return Err(AttachError::Rejected(error));
        }
    };
    // Every write above has already adopted a complete canonical profile.
    // There is no hand-patched cache row and no second unlocked disk read.
    if !restarts {
        return Ok((outcome, WorkerOutcome::NotRunning));
    }
    let resume_id = id.to_owned();
    let pane_warnings = run_blocking(state, &profile, id, move |storage| {
        Ok(crate::session::attach_project::resume_after_conversion(
            storage, &resume_id, quiesced,
        ))
    })
    .await
    .unwrap_or_else(|error| vec![format!("{error:#}")]);
    if !pane_warnings.is_empty() {
        let message = pane_warnings.join("; ");
        outcome.warnings.extend(pane_warnings);
        return Ok((outcome, WorkerOutcome::RestartFailed(message)));
    }
    let worker = if was_running {
        spawn_worker(state, id).await
    } else {
        WorkerOutcome::NotRunning
    };
    Ok((outcome, worker))
}

async fn run_blocking<T, F>(
    state: &Arc<AppState>,
    profile: &str,
    id: &str,
    action: F,
) -> anyhow::Result<T>
where
    T: Send + 'static,
    F: FnOnce(&dyn crate::session::SessionStore) -> anyhow::Result<T> + Send + 'static,
{
    let worker_state = state.clone();
    let profile = profile.to_owned();
    let id = id.to_owned();
    let failure_profile = profile.clone();
    let result = tokio::task::spawn_blocking(move || {
        let store =
            super::session_store::NativeSessionStore::open(worker_state, &profile, Some(id))?;
        action(&store)
    })
    .await?;
    if result
        .as_ref()
        .is_err_and(|error| error.is::<crate::session::attach_project::AttachRollbackIncomplete>())
    {
        state
            .mark_reload_failure(crate::daemon::RuntimeHealth::Degraded {
                code: crate::daemon::ReloadFailureCode::ProfileData,
                profiles: vec![failure_profile],
            })
            .await;
    }
    result
}

async fn restore_after_failure(
    state: &Arc<AppState>,
    id: &str,
    profile: &str,
    quiesced: crate::session::attach_project::Quiesced,
    respawn_worker: bool,
) -> Vec<String> {
    let resume_id = id.to_owned();
    let mut warnings = run_blocking(state, profile, id, move |storage| {
        Ok(crate::session::attach_project::resume_after_conversion(
            storage, &resume_id, quiesced,
        ))
    })
    .await
    .unwrap_or_else(|error| vec![format!("{error:#}")]);
    let can_respawn = warnings.is_empty();
    for warning in &warnings {
        tracing::warn!(target: "session.attach", session = %id,
            "could not restore the session after a failed attach: {warning}");
    }
    if respawn_worker && can_respawn {
        if let WorkerOutcome::RestartFailed(error) = spawn_worker(state, id).await {
            tracing::warn!(target: "session.attach", session = %id,
                "could not restart the worker after a failed attach: {error}");
            warnings.push(error);
        }
    }
    warnings
}

/// Start the session's worker again, in the workspace the conversion produced.
async fn spawn_worker(state: &Arc<AppState>, id: &str) -> WorkerOutcome {
    let request = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return WorkerOutcome::RestartFailed("session disappeared mid-restart".to_string());
        };
        crate::acp::supervisor::SpawnRequest {
            launch_admission: None,
            expected_lifecycle_generation: inst.lifecycle_generation,
            session_id: id.to_string(),
            agent: inst.tool.clone(),
            tool: inst.tool.clone(),
            // The canonical instance, so this is the workspace directory when the
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

    match state.acp_supervisor.spawn(request).await {
        Ok(()) => WorkerOutcome::Restarted,
        Err(e) => WorkerOutcome::RestartFailed(format!("worker respawn failed: {e}")),
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
        let (launcher, launches) = support::counting_failing_launcher();
        let state = support::build_test_app_state_with_launcher(vec![inst], launcher);

        let restart = tokio::spawn({
            let state = Arc::clone(&state);
            let id = id.clone();
            async move { spawn_worker(&state, &id).await }
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
}
