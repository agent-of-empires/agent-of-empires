//! Daemon-side orchestration for attaching a repo to a live session (#3103).
//!
//! [`crate::session::attach_project`] does the filesystem and persistence half.
//! This module is the part that needs the daemon: quiescing the session, making
//! the attachment durable, and bouncing the ACP worker so the agent actually
//! sees the new root, without losing the conversation.
//!
//! ## Why a bounce rather than a refusal
//!
//! The "edit workdir name" endpoint refuses while a session is active, which is
//! right there: it moves the directory out from under a running worker, and
//! doing that crash-looped the worker in #2260. Nothing moves here. The primary
//! `cwd` is untouched and a new sibling directory appears, so the failure mode
//! that justified refusing does not apply, and #2346 already asks for the
//! opposite default. Refusing would also gut the feature, because the whole
//! point is that you need the repo mid-task.
//!
//! What a bounce does need is a barrier. An already-running worker's filesystem
//! policy and its agent's `additional_directories` are both fixed at handshake
//! time, so between the persist and the respawn the session is in a state where
//! the repo is recorded but unusable. Checking "is a turn in flight" and then
//! restarting is not enough, because a turn can start in the gap. So the whole
//! sequence is held under the per-session `instance_lock`, the same lock the
//! tied-worktree rename holds across its `git worktree move` plus metadata
//! write, and the turn probe runs inside it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::session::attach_project::{AttachOutcome, ExistingBranch};
use crate::session::{AttachedRepo, Storage};

use super::AppState;

/// What happened to the session's worker after the repo was recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkerOutcome {
    /// No worker was running, so there was nothing to bounce. The roots apply
    /// when the session next starts.
    NotRunning,
    /// The caller asked not to restart. The repo is recorded and takes effect
    /// on the next start; the running agent cannot see it until then.
    Deferred,
    /// Worker bounced and resumed against its stored ACP session id, so the
    /// transcript is intact and the agent can see the new root.
    Restarted,
    /// The repo is recorded but the respawn failed. Deliberately not rolled
    /// back: the worktree exists and the user (or their agent) may already have
    /// touched it, so the recoverable state is "attached, worker down", which
    /// the next reconciler tick or an explicit restart can finish.
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

/// Attach `repo_path` to session `id`, bouncing its worker when one is live.
///
/// `restart` false records the repo without touching the worker, for a caller
/// that would rather stop and start the session itself.
pub(crate) async fn attach_project(
    state: &Arc<AppState>,
    id: &str,
    repo_path: &Path,
    on_existing: ExistingBranch,
    restart: bool,
) -> Result<(AttachOutcome, WorkerOutcome), AttachError> {
    let inst_lock = state.instance_lock(id).await;
    // Held across the turn probe, the persist, and the restart. Releasing it
    // between any two of those is what would let a prompt land against a worker
    // whose roots are already stale.
    let _guard = inst_lock.lock().await;

    let (profile, was_running, sandboxed) = {
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
            // The `enabled` flag, not mere presence: a session carrying a
            // disabled SandboxInfo has no container, so taking the
            // discard-and-clear-pins path below would be a pointless Docker
            // round trip at best and would turn a healthy attach into
            // RestartFailed on a host with no container runtime. Matches how
            // `session::deletion` gates its container work.
            inst.sandbox_info.as_ref().is_some_and(|s| s.enabled),
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

    // A sandboxed session only sees a new repo once its container is recreated,
    // and the container cannot be removed while the agent runs inside it. So
    // `restart: false` cannot be honoured here: it would attach the repo and
    // leave a container that keeps hiding it, including across later restarts,
    // since a container is reused by name until something removes it. Refused
    // before the attach so nothing is half-applied.
    if sandboxed && !restart && was_running {
        return Err(AttachError::Rejected(
            "this session is sandboxed, so attaching a repo has to recreate its container, which \
             needs the running agent stopped. Retry with restart enabled."
                .to_string(),
        ));
    }

    let outcome = {
        let profile = profile.clone();
        let id_owned = id.to_string();
        let repo = repo_path.to_path_buf();
        let file_watch = state.file_watch.clone();
        tokio::task::spawn_blocking(move || {
            let storage = Storage::new(&profile, file_watch).map_err(|e| e.to_string())?;
            crate::session::attach_project::attach(
                &storage,
                &profile,
                &id_owned,
                &repo,
                on_existing,
            )
            .map_err(|e| format!("{e:#}"))
        })
        .await
        .map_err(|e| AttachError::Rejected(format!("attach task panicked: {e}")))?
        .map_err(AttachError::Rejected)?
    };

    // Persist landed, so mirror it into the live state before anything reads
    // the instance again. The disk watcher would get here eventually, but the
    // respawn below reads the instance to build its mount set and roots.
    mirror_attached_repo(state, id, outcome.repo.clone()).await;

    // A container is reused by name until something removes it, so the reset has
    // to happen even when there is no worker to bounce, or the next start comes
    // up in a container that still has no mount for the new repo. With a worker
    // running it is `bounce_worker`'s job instead, after the shutdown.
    if !was_running {
        if let Err(e) = reset_sandbox(state, id, &profile, sandboxed).await {
            return Ok((outcome, WorkerOutcome::RestartFailed(e)));
        }
    }

    if !restart {
        return Ok((outcome, WorkerOutcome::Deferred));
    }
    if !was_running {
        return Ok((outcome, WorkerOutcome::NotRunning));
    }

    let worker = bounce_worker(state, id, &profile, sandboxed).await;
    Ok((outcome, worker))
}

async fn mirror_attached_repo(state: &Arc<AppState>, id: &str, repo: AttachedRepo) {
    let mut instances = state.instances.write().await;
    if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
        // Keyed on `worktree_path`, which is unique per attached repo. The persist
        // already landed, so a file-watch reload can win the race to this list and
        // an unconditional push would double the entry: a duplicate sidebar chip
        // and a duplicate `additional_directories` root on the next spawn.
        if !inst
            .attached_repos
            .iter()
            .any(|existing| existing.worktree_path == repo.worktree_path)
        {
            inst.attached_repos.push(repo);
        }
    }
}

/// Stop and respawn the session's worker so the agent picks up the new root.
///
/// Resume, not a fresh session: `stored_acp_session_id` is threaded through so
/// the handshake sends `session/load` and the transcript survives. Never
/// `shutdown_and_delete`, which fires a protocol `session/delete` and destroys
/// resumability (#1710).
async fn bounce_worker(
    state: &Arc<AppState>,
    id: &str,
    profile: &str,
    sandboxed: bool,
) -> WorkerOutcome {
    // `shutdown_and_wait`, not `shutdown`: the runner has to exit and release
    // its unix socket before the replacement binds the same path, which is the
    // same reason the agent-switch endpoint waits.
    if let Err(e) = state
        .acp_supervisor
        .shutdown_and_wait(id, std::time::Duration::from_secs(5))
        .await
    {
        return WorkerOutcome::RestartFailed(format!("could not stop the current worker: {e}"));
    }

    // Only now that the worker is down: removing the container out from under a
    // live agent kills it mid-turn.
    if let Err(e) = reset_sandbox(state, id, profile, sandboxed).await {
        return WorkerOutcome::RestartFailed(e);
    }

    let request = {
        let instances = state.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == id) else {
            return WorkerOutcome::RestartFailed("session disappeared mid-restart".to_string());
        };
        crate::acp::supervisor::SpawnRequest {
            session_id: id.to_string(),
            agent: inst.tool.clone(),
            cwd: PathBuf::from(&inst.project_path),
            additional_dirs: inst.additional_root_paths(),
            provider_env: vec![],
            model: inst.agent_model.clone(),
            effort: None,
            // The whole point of the bounce: resume the same conversation.
            stored_acp_session_id: inst.acp_session_id.clone(),
            // Threaded for the same continuity reason as the stored session id:
            // a session whose structured fork has not completed its first
            // connect still needs session/fork on the respawn, or the bounce
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

/// Remove the sandbox container and drop its create-time pins, so the next start
/// rebuilds it with a mount for the newly attached repo.
///
/// The removal and the on-disk pin clear are
/// [`session::attach_project::reset_sandbox_container`], shared with the CLI and
/// the TUI so all three surfaces cannot drift. What the daemon adds is the
/// in-memory mirror: the respawn below reads the live instance to build its
/// mount set, not the file on disk.
///
/// A no-op for an unsandboxed session, so the common path pays no `docker`.
async fn reset_sandbox(
    state: &Arc<AppState>,
    id: &str,
    profile: &str,
    sandboxed: bool,
) -> Result<(), String> {
    if !sandboxed {
        return Ok(());
    }

    let id_owned = id.to_string();
    let profile_owned = profile.to_string();
    let file_watch = state.file_watch.clone();
    let done = tokio::task::spawn_blocking(move || {
        let storage = Storage::new(&profile_owned, file_watch).map_err(|e| e.to_string())?;
        crate::session::attach_project::reset_sandbox_container(&storage, &id_owned, true)
            .map_err(|e| format!("{e:#}"))
    })
    .await;
    match done {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(e),
        Err(e) => return Err(format!("container reset panicked: {e}")),
    }

    let mut instances = state.instances.write().await;
    if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
        if let Some(sandbox) = inst.sandbox_info.as_mut() {
            sandbox.container_id = None;
            sandbox.container_workdir = None;
        }
    }
    Ok(())
}
