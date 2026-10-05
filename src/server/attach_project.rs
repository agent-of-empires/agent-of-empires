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
    original: Arc<crate::session::LaunchOrigin>,
    repo_path: &Path,
    on_existing: ExistingBranch,
) -> Result<(AttachOutcome, WorkerOutcome), AttachError> {
    let id = original.session_id();
    let instance = {
        let instances = state.instances.read().await;
        let instance = instances
            .iter()
            .find(|row| row.id == id)
            .ok_or(AttachError::NotFound)?;
        let cached = crate::session::LaunchOrigin::capture(instance)
            .map_err(|error| AttachError::Rejected(error.to_string()))?;
        if !original.recognizes_published_snapshot(&cached) {
            return Err(AttachError::Rejected(
                "original attach cache row was replaced or superseded before dispatch".into(),
            ));
        }
        instance.clone()
    };
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

    let original_storage = instance
        .original_storage()
        .map_err(|error| AttachError::Rejected(error.to_string()))?;
    let profile = instance.source_profile.clone();
    let was_running = matches!(
        state.acp_supervisor.worker_state(id).await,
        crate::daemon::AcpWorkerState::Running
    );

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
    let (storage, instance, plan, reserved, restarts, worker_record) = {
        let profile = profile.clone();
        let id_owned = id.to_string();
        let repo = repo_path.to_path_buf();
        let original_storage = original_storage.clone();
        let original = original.clone();
        tokio::task::spawn_blocking(move || {
            let storage = original_storage.as_ref().clone();
            storage
                .verify_profile_identity()
                .map_err(|error| error.to_string())?;
            let instance = original
                .with_storage(|_, row| Ok(row))
                .map_err(|error| error.to_string())?;
            let mut plan =
                crate::session::attach_project::plan(&instance, &profile, &repo, on_existing)
                    .map_err(|e| format!("{e:#}"))?;
            let worker_record = crate::process::worker_registry::load_strict(&id_owned)
                .map_err(|e| format!("{e:#}"))?;
            let reserved =
                crate::session::attach_project::reserve_attach(&storage, &id_owned, &mut plan)
                    .map_err(|e| format!("{e:#}"))?;
            let restarts =
                crate::session::attach_project::needs_restart(&plan, instance.is_sandboxed());
            Ok::<_, String>((storage, instance, plan, reserved, restarts, worker_record))
        })
        .await
        .map_err(|e| AttachError::Rejected(format!("attach task panicked: {e}")))?
        .map_err(AttachError::Rejected)?
    };

    // The reservation rejects late runner authorization. The journal's durable
    // proof, rather than the supervisor's in-memory map, governs conversion.
    if restarts {
        let stopped = if was_running {
            // Cancel the original SDK admission before journal settlement can deliver
            // EOF to its drain task and initiate a competing respawn.
            state
                .acp_supervisor
                .shutdown_and_wait(
                    plan.native_scope()
                        .map_err(|error| AttachError::Rejected(error.to_string()))?,
                    std::time::Duration::from_secs(5),
                )
                .await
                .map_err(|error| error.to_string())
        } else {
            crate::session::attach_project::settle_for_conversion(
                &storage,
                id,
                worker_record.as_ref(),
                &plan,
            )
            .await
            .map_err(|error| format!("{error:#}"))
        };
        if let Err(error) = stopped {
            let _ = tokio::task::spawn_blocking(move || {
                crate::session::attach_project::release_attach(&plan)
            })
            .await;
            return Err(AttachError::Rejected(format!(
                "could not stop the current worker: {error}"
            )));
        }
    }
    let reservation_ack = reserved.clone();

    let (plan, quiesced, reserved) = if restarts {
        match run_blocking(original_storage.clone(), {
            let instance = instance.clone();
            let acknowledged = reserved.clone();
            move |storage| {
                let (quiesced, acknowledged) =
                    crate::session::attach_project::quiesce_for_conversion(
                        storage,
                        &instance,
                        &plan,
                        acknowledged,
                    )
                    .map_err(|e| format!("{e:#}"))?;
                Ok((plan, quiesced, acknowledged))
            }
        })
        .await
        {
            Ok(result) => result,
            Err(error) => return Err(AttachError::Rejected(error)),
        }
    } else {
        (
            plan,
            crate::session::attach_project::Quiesced::default(),
            reserved,
        )
    };

    let outcome = {
        let id_owned = id.to_string();
        let instance = instance.clone();
        match run_blocking(original_storage.clone(), move |storage| {
            crate::session::attach_project::attach_planned(storage, &id_owned, &instance, plan)
                .map_err(|e| format!("{e:#}"))
        })
        .await
        {
            Ok(outcome) => outcome,
            Err(e) => {
                // Put the session back.
                restore_after_failure(
                    state,
                    [original.clone(), reservation_ack.clone(), reserved.clone()],
                    reserved.clone(),
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
    if let Err(error) = mirror_conversion(
        state,
        [original.clone(), reservation_ack, reserved.clone()],
        outcome.original.clone(),
    )
    .await
    {
        return Ok((outcome, WorkerOutcome::RestartFailed(error)));
    }

    if !restarts {
        return Ok((outcome, WorkerOutcome::NotRunning));
    }

    // The tmux pane, when there was one.
    let pane_warnings = run_blocking(original_storage.clone(), {
        let acknowledged = outcome.original.clone();
        move |_| {
            Ok(crate::session::attach_project::resume_after_conversion(
                acknowledged,
                quiesced,
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
    let worker = spawn_worker(state, outcome.original.clone()).await;
    Ok((outcome, worker))
}

/// Run against the retained physical original, never reopen a mutable profile name.
async fn run_blocking<T, F>(storage: Arc<Storage>, f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&Storage) -> Result<T, String> + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        storage
            .verify_profile_identity()
            .map_err(|error| error.to_string())?;
        f(&storage)
    })
    .await
    .unwrap_or_else(|e| Err(format!("attach task panicked: {e}")))
}

/// Bring the session back after an attach that failed with it stopped.
async fn restore_after_failure(
    state: &Arc<AppState>,
    earlier: [Arc<crate::session::LaunchOrigin>; 3],
    acknowledged: Arc<crate::session::LaunchOrigin>,
    quiesced: crate::session::attach_project::Quiesced,
    respawn_worker: bool,
) {
    let id = acknowledged.session_id().to_owned();
    let warnings = run_blocking(Arc::new(acknowledged.storage().clone()), {
        let acknowledged = acknowledged.clone();
        move |_| {
            Ok(crate::session::attach_project::resume_after_conversion(
                acknowledged,
                quiesced,
            ))
        }
    })
    .await
    .unwrap_or_else(|e| vec![e]);
    for warning in warnings {
        tracing::warn!(
            target: "session.attach",
            session = %id,
            "could not restore the session after a failed attach: {warning}"
        );
    }
    if respawn_worker {
        let result = match mirror_conversion(state, earlier, acknowledged.clone()).await {
            Ok(()) => spawn_worker(state, acknowledged).await,
            Err(error) => WorkerOutcome::RestartFailed(error),
        };
        if let WorkerOutcome::RestartFailed(e) = result {
            tracing::warn!(
                target: "session.attach",
                session = %id,
                "could not restart the worker after a failed attach: {e}"
            );
        }
    }
}

/// Publish only the attach transaction's actual ACK into its still-original cache slot.
async fn mirror_conversion(
    state: &Arc<AppState>,
    earlier: [Arc<crate::session::LaunchOrigin>; 3],
    acknowledged: Arc<crate::session::LaunchOrigin>,
) -> Result<(), String> {
    let mut instances = state.instances.clone().write_owned().await;
    tokio::task::spawn_blocking(move || {
        acknowledged.with_storage(|_, stored| {
            let slot = instances
                .iter_mut()
                .find(|row| row.id == acknowledged.session_id())
                .ok_or_else(|| anyhow::anyhow!("original attach cache row disappeared"))?;
            let cached = crate::session::LaunchOrigin::capture(slot)?;
            anyhow::ensure!(
                earlier
                    .iter()
                    .any(|source| source.recognizes_published_snapshot(&cached))
                    || acknowledged.recognizes_published_snapshot(&cached),
                "original attach cache row was replaced or superseded"
            );
            *slot = super::reload::merge_runtime_fields(slot, stored);
            Ok(())
        })
    })
    .await
    .map_err(|error| error.to_string())?
    .map_err(|error| error.to_string())
}

/// Restart from the actual attach ACK, not whichever row now has this ID.
async fn spawn_worker(
    state: &Arc<AppState>,
    original: Arc<crate::session::LaunchOrigin>,
) -> WorkerOutcome {
    let request = match tokio::task::spawn_blocking(move || {
        original.with_storage(|_, inst| {
            let request = crate::acp::supervisor::SpawnRequest {
                session_id: original.session_id().to_owned(),
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
                origin: Some(original.clone()),
                yolo_mode: inst.yolo_mode,
                acp_mode_id: inst.acp_mode_id.clone(),
                agent_command_override: crate::server::acp_reconciler::command_override_for_spawn(
                    &inst.tool,
                    &inst.command,
                ),
                seed_history_replay: false,
                claude_store_pin: inst.selected_claude_store_pin(),
            };
            Ok(request)
        })
    })
    .await
    {
        Ok(Ok(request)) => request,
        Ok(Err(error)) => return WorkerOutcome::RestartFailed(error.to_string()),
        Err(error) => return WorkerOutcome::RestartFailed(error.to_string()),
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
            let original = {
                let rows = state.instances.read().await;
                crate::session::LaunchOrigin::capture(&rows[0]).unwrap()
            };
            async move { spawn_worker(&state, original).await }
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
