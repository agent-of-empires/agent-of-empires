//! Daemon-side orchestration for attaching a repo to a live session (#3103).
//!
//! [`crate::session::attach_project`] does the filesystem and persistence half.
//! This module is the part that needs the daemon: quiescing the session, making
//! the conversion durable, and starting the worker again so the agent comes up in
//! the workspace, without losing the conversation.
//!
//! ## Why a stop and start rather than a refusal
//!
//! The "edit workdir name" endpoint refuses while a session is active, because it
//! moves the directory out from under a running worker and that crash-looped the
//! worker in #2260. Attaching moves the directory too, so the same hazard applies,
//! but refusing would gut the feature: the whole point is that you realize you
//! need the second repo mid-task. So this does what the rename could not, and
//! takes the session down for the move rather than doing it live. #2346 asks for
//! exactly that default.
//!
//! What the sequence needs is a barrier. Between the shutdown and the respawn the
//! session's recorded working directory is changing under it, and checking "is a
//! turn in flight" and then acting is not enough because a turn can start in the
//! gap. So the whole sequence is held under the per-session `instance_lock`, the
//! same lock the tied-worktree rename holds across its `git worktree move` plus
//! metadata write, and the turn probe runs inside it. Since #3621 that takes two
//! locks, not one: prompt submission has its own per-session authority and never
//! takes `instance_lock`, so the barrier holds `prompt_submission` outside it.
//!
//! ## Why the split against the session-domain half
//!
//! `plan` validates without writing, `attach_planned` writes. The daemon needs
//! them separate because its quiesce is async (`shutdown_and_wait` on the
//! supervisor) and cannot run inside the blocking closure that owns `Storage`,
//! and because a refusal must never cost the user a stopped session.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::session::attach_project::{AttachOutcome, ExistingBranch};
use crate::session::Storage;

use super::AppState;

/// What happened to the session's worker after the repo was recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkerOutcome {
    /// Nothing had to be stopped, because nothing had to move: the session was
    /// already a workspace and the new worktree simply appeared inside it.
    NotRunning,
    /// Stopped for the conversion and started again against the stored ACP
    /// session id, so the transcript is intact and the agent comes up in the
    /// workspace.
    Restarted,
    /// The repo is recorded but the session could not be started again.
    /// Deliberately not rolled back: the worktree exists and the user (or their
    /// agent) may already have touched it, so the recoverable state is
    /// "attached, worker down", which the next reconciler tick or an explicit
    /// restart can finish.
    RestartFailed(String),
}

#[derive(Debug)]
pub(crate) enum AttachError {
    NotFound,
    /// A turn is in flight. Bouncing mid-turn would drop the agent's reply, so
    /// the caller is asked to wait or cancel instead.
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
    // `instance_lock` alone stopped being the whole barrier once prompt
    // submission moved to its own authority (#3621): the queue drains and every
    // prompt endpoint serialize on `prompt_submission` and never take
    // `instance_lock`, so a reconciler tick could otherwise deliver a queued
    // follow-up to the worker this is about to stop and retire the row as sent.
    // Claimed first, which is the order `prompt_submission` documents, and in
    // the admission form so an unknown id allocates neither lock.
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(id)
        .await
    else {
        return Err(AttachError::NotFound);
    };
    let inst_lock = state.instance_lock(id).await;
    // Held across the turn probe, the stop, the persist and the start. Releasing
    // it between any two of those is what would let a prompt land against a
    // worker whose working directory is about to move.
    let _guard = inst_lock.lock().await;

    let (profile, was_running) = {
        let instances = state.instances.read().await;
        let inst = instances
            .iter()
            .find(|i| i.id == id)
            .ok_or(AttachError::NotFound)?;
        (
            inst.source_profile.clone(),
            matches!(
                state.acp_supervisor.worker_state(id).await,
                crate::acp::supervisor::AcpWorkerState::Running
            ),
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

    // Validation first, with nothing stopped and nothing written: a refusal must
    // not cost the user a running session. The instance comes back owned because
    // the persist below runs in a second blocking task, after the async quiesce.
    let (instance, plan, restarts) = {
        let profile = profile.clone();
        let id_owned = id.to_string();
        let repo = repo_path.to_path_buf();
        let file_watch = state.file_watch.clone();
        tokio::task::spawn_blocking(move || {
            let storage = Storage::new(&profile, file_watch).map_err(|e| e.to_string())?;
            let instances = storage.load().map_err(|e| format!("{e:#}"))?;
            let instance = instances
                .into_iter()
                .find(|i| i.id == id_owned)
                .ok_or_else(|| format!("session not found: {id_owned}"))?;
            let plan =
                crate::session::attach_project::plan(&instance, &profile, &repo, on_existing)
                    .map_err(|e| format!("{e:#}"))?;
            let restarts =
                crate::session::attach_project::needs_restart(&plan, instance.is_sandboxed());
            Ok::<_, String>((instance, plan, restarts))
        })
        .await
        .map_err(|e| AttachError::Rejected(format!("attach task panicked: {e}")))?
        .map_err(AttachError::Rejected)?
    };

    // Order is load-bearing: the worker exits before the container is removed,
    // because removing it under a live agent kills it mid-turn, and the container
    // is gone before anything is renamed, because its bind mount makes `rename(2)`
    // on the worktree fail `EBUSY`.
    //
    // `shutdown_and_wait`, not `shutdown`: the runner has to exit and release its
    // unix socket before the replacement binds the same path, which is the same
    // reason the agent-switch endpoint waits.
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

    let quiesced = if restarts {
        // The worker registry entry is already gone, so this takes down the tmux
        // pane and the sandbox container and reports only what it stopped.
        match run_blocking(state, &profile, {
            let instance = instance.clone();
            move |storage| {
                crate::session::attach_project::quiesce_for_conversion(storage, &instance)
                    .map_err(|e| format!("{e:#}"))
            }
        })
        .await
        {
            Ok(q) => {
                clear_sandbox_pins(state, id).await;
                q
            }
            Err(e) => return Err(AttachError::Rejected(e)),
        }
    } else {
        crate::session::attach_project::Quiesced::default()
    };

    let outcome = {
        let id_owned = id.to_string();
        let instance = instance.clone();
        match run_blocking(state, &profile, move |storage| {
            crate::session::attach_project::attach_planned(storage, &id_owned, &instance, plan)
                .map_err(|e| format!("{e:#}"))
        })
        .await
        {
            Ok(outcome) => outcome,
            Err(e) => {
                // Put the session back: the session-domain rollback already undid
                // the filesystem half, so a stopped session is the only damage
                // that would otherwise stick.
                restore_after_failure(state, id, &profile, quiesced, was_running && restarts).await;
                return Err(AttachError::Rejected(e));
            }
        }
    };

    // Persist landed, so mirror it into the live state before anything reads the
    // instance again. The disk watcher would get here eventually, but the respawn
    // below reads the instance to build its mount set and its agent cwd.
    mirror_conversion(state, id, &outcome).await;

    if !restarts {
        return Ok((outcome, WorkerOutcome::NotRunning));
    }

    // The tmux pane, when there was one. Its shell was launched in the directory
    // the conversion moved, so it is recreated rather than left alone.
    let pane_warnings = run_blocking(state, &profile, {
        let id_owned = id.to_string();
        move |storage| {
            Ok(crate::session::attach_project::resume_after_conversion(
                storage, &id_owned, quiesced,
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
    let worker = spawn_worker(state, id).await;
    Ok((outcome, worker))
}

/// Run a closure that needs a `Storage` for this profile on a blocking thread.
///
/// Every step of the attach that touches disk goes through here, so the panic
/// and storage-open failures are phrased once rather than at four call sites.
async fn run_blocking<T, F>(state: &Arc<AppState>, profile: &str, f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce(&Storage) -> Result<T, String> + Send + 'static,
{
    let profile = profile.to_string();
    let file_watch = state.file_watch.clone();
    tokio::task::spawn_blocking(move || {
        let storage = Storage::new(&profile, file_watch).map_err(|e| e.to_string())?;
        f(&storage)
    })
    .await
    .unwrap_or_else(|e| Err(format!("attach task panicked: {e}")))
}

/// Bring the session back after an attach that failed with it stopped.
///
/// Best effort throughout: the attach error is the one worth reporting, and this
/// exists so a refused attach does not also leave the session down.
async fn restore_after_failure(
    state: &Arc<AppState>,
    id: &str,
    profile: &str,
    quiesced: crate::session::attach_project::Quiesced,
    respawn_worker: bool,
) {
    let id_owned = id.to_string();
    let warnings = run_blocking(state, profile, move |storage| {
        Ok(crate::session::attach_project::resume_after_conversion(
            storage, &id_owned, quiesced,
        ))
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
        if let WorkerOutcome::RestartFailed(e) = spawn_worker(state, id).await {
            tracing::warn!(
                target: "session.attach",
                session = %id,
                "could not restart the worker after a failed attach: {e}"
            );
        }
    }
}

/// Mirror the persisted conversion into the live instance map.
///
/// The persist has already landed and is authoritative, so this assigns rather
/// than splicing: `workspace_info` is written whole by `Storage::update` under
/// both lock layers, so there is no per-entry race to merge. Without the mirror
/// the respawn below would build its mount set and agent cwd from the pre-attach
/// instance, which no longer describes where the repos are.
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
///
/// Resume, not a fresh session: `stored_acp_session_id` is threaded through so
/// the handshake sends `session/load` and the transcript survives. Never
/// `shutdown_and_delete`, which fires a protocol `session/delete` and destroys
/// resumability (#1710).
async fn spawn_worker(state: &Arc<AppState>, id: &str) -> WorkerOutcome {
    let request = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return WorkerOutcome::RestartFailed("session disappeared mid-restart".to_string());
        };
        crate::acp::supervisor::SpawnRequest {
            session_id: id.to_string(),
            agent: inst.tool.clone(),
            tool: inst.tool.clone(),
            // The mirrored instance, so this is the workspace directory when the
            // attach converted the session, not the path it started from.
            cwd: PathBuf::from(&inst.project_path),
            additional_dirs: vec![],
            provider_env: vec![],
            model: inst.agent_model.clone(),
            effort: None,
            // The whole point of taking the session down and bringing it back:
            // resume the same conversation.
            stored_acp_session_id: inst.acp_session_id.clone(),
            // Threaded for the same continuity reason as the stored session id:
            // a session whose structured fork has not completed its first
            // connect still needs session/fork on the respawn, or the restart
            // loses the linkage to its parent.
            fork_from: inst.fork_pending.clone(),
            sandbox_info: inst.sandbox_info.clone(),
            source_profile: Some(inst.source_profile.clone()),
            yolo_mode: inst.yolo_mode,
            acp_mode_id: inst.acp_mode_id.clone(),
            agent_command_override: crate::server::acp_reconciler::command_override_for_spawn(
                &inst.tool,
                &inst.command,
            ),
            seed_history_replay: false,
        }
    };

    match state.acp_supervisor.spawn(request).await {
        Ok(()) => WorkerOutcome::Restarted,
        Err(e) => WorkerOutcome::RestartFailed(format!("worker respawn failed: {e}")),
    }
}

/// Drop the create-time container pins from the live instance.
///
/// `session::attach_project::quiesce_for_conversion` removes the container and
/// clears the pins on disk, shared with the CLI and the TUI so all three surfaces
/// cannot drift. What the daemon adds is the in-memory mirror: the respawn above
/// reads the live instance to build its mount set, not the file on disk.
async fn clear_sandbox_pins(state: &Arc<AppState>, id: &str) {
    let mut instances = state.instances.write().await;
    if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
        if let Some(sandbox) = inst.sandbox_info.as_mut() {
            sandbox.container_id = None;
            sandbox.container_workdir = None;
        }
    }
}
