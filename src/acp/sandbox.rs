//! Sandbox container lifecycle for structured view sessions.
//!
//! The structured view spawn path mirrors the tmux path's sandbox handling: if
//! the session's `Instance` has `sandbox_info`, create + start the
//! Docker container, optionally run on_launch hooks inside it, and
//! hand the `SandboxInfo` back to the caller so the supervisor can
//! wrap the agent argv in `docker exec`.
//!
//! This lives in its own module so every structured view-spawn entry point
//! (auto-spawn after create, view switch, manual `POST /spawn`,
//! reconciler reattach fallback) goes through the same code path.

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::{Mutex, RwLock};

use crate::session::{Instance, SandboxInfo};

/// Ensure the sandbox container for the named session is running.
///
/// `run_on_launch_hooks` controls whether profile/repo `on_launch`
/// hooks fire after the container is up. Only the initial
/// `auto-spawn after create` path passes `true`; every other entry
/// point (view switch, manual `/spawn`, reconciler resume)
/// passes `false` so hooks don't re-run on every reattach.
///
/// `instance_lock` is the per-session `state.instance_lock(id)`
/// mutex. We hold it for the duration of the call so a concurrent
/// `delete_session` (which acquires the same mutex) can't tear down
/// the instance mid-create and leave us with an orphan docker
/// container. Docker work runs on a blocking thread; the in-memory
/// `Instance` is otherwise locked only for a short read snapshot
/// and a short write to stamp the resolved `container_id`.
///
/// Returns the session's `sandbox_info` (with `container_id`
/// populated); for non-sandboxed sessions it returns `None`.
pub async fn ensure_container_for_session(
    instances: &RwLock<Vec<Instance>>,
    instance_lock: &Arc<Mutex<()>>,
    session_id: &str,
    run_on_launch_hooks: bool,
) -> Result<Option<SandboxInfo>> {
    // Hold the per-session mutex for the whole call so delete_session
    // (which acquires the same lock) cannot interleave between the
    // existence check and the container create.
    let _guard = instance_lock.lock().await;

    // Phase 1: short read on the instance list. Clone the Instance so
    // the docker work can run on a blocking thread without holding
    // any tokio lock.
    let mut instance_clone = {
        let guard = instances.read().await;
        let Some(inst) = guard.iter().find(|i| i.id == session_id) else {
            anyhow::bail!("session {session_id} not found");
        };
        if !inst.is_sandboxed() {
            return Ok(None);
        }
        inst.clone()
    };

    // Phase 2: docker create/start + workdir/hook resolution on a
    // blocking thread. `get_container_for_instance` may pull an image
    // and create the container, both of which can take many seconds;
    // running it here keeps the tokio worker free for other requests.
    let session_id_owned = session_id.to_string();
    let (sandbox_info, container_workdir, hooks, hook_env) =
        tokio::task::spawn_blocking(move || -> Result<_> {
            let _container = instance_clone
                .get_container_for_instance()
                .context("ensuring sandbox container")?;
            let workdir = instance_clone.container_workdir();
            let hooks = if run_on_launch_hooks {
                let profile = instance_clone.source_profile.clone();
                instance_clone.resolve_on_launch_hooks(false, &profile)
            } else {
                None
            };
            let env = crate::session::repo_config::lifecycle_env_vars(&instance_clone);
            Ok((instance_clone.sandbox_info.clone(), workdir, hooks, env))
        })
        .await
        .context("docker ensure task failed to join")??;

    // Phase 3: brief write lock to stamp the resolved container_id (and the
    // host-minted before_start env) back onto the live Instance so subsequent
    // reads observe them. Carrying `before_start_env` back matters because the
    // mint ran on the cloned instance above; without this, the next
    // ensure_container_for_session would clone an empty cache and re-run the
    // before_start hook on every view switch / spawn, re-minting credentials.
    if let Some(info) = &sandbox_info {
        let mut guard = instances.write().await;
        if let Some(inst) = guard.iter_mut().find(|i| i.id == session_id_owned) {
            if let Some(ref mut sb) = inst.sandbox_info {
                if sb.container_id.is_none() {
                    sb.container_id = info.container_id.clone();
                }
                sb.before_start_env = info.before_start_env.clone();
            }
        }
    }

    // Phase 4: run on_launch hooks outside the instances lock (the
    // per-session mutex is still held for the call's lifetime). Hooks
    // are shell commands that can themselves take seconds, so wrap in
    // spawn_blocking. Best-effort: failures are logged and the spawn
    // proceeds.
    if let (Some(cmds), Some(info)) = (hooks, sandbox_info.as_ref()) {
        if !cmds.is_empty() {
            let container_name = info.container_name.clone();
            let workdir = container_workdir.clone();
            let sid = session_id.to_string();
            match tokio::task::spawn_blocking(move || {
                crate::session::repo_config::execute_hooks_in_container_best_effort(
                    &cmds,
                    &container_name,
                    &workdir,
                    true,
                    &hook_env,
                )
            })
            .await
            {
                Ok(errors) => {
                    for err in errors {
                        tracing::warn!(
                            target: "acp.sandbox",
                            session = %sid,
                            "on_launch hook failed: {err}"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        target: "acp.sandbox",
                        session = %sid,
                        "on_launch hook task failed to join: {e}"
                    );
                }
            }
        }
    }

    Ok(sandbox_info)
}
