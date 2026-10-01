//! Sandbox container lifecycle for structured view sessions.

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::{Mutex, RwLock};

use crate::session::{Instance, SandboxInfo};

/// Ensure the sandbox container for the named session is running.
pub async fn ensure_container_for_session(
    instances: &RwLock<Vec<Instance>>,
    instance_lock: &Arc<Mutex<()>>,
    session_id: &str,
    run_on_launch_hooks: bool,
) -> Result<Option<SandboxInfo>> {
    let _guard = instance_lock.lock().await;

    ensure_container_for_session_locked(instances, session_id, run_on_launch_hooks).await
}

/// Ensure the sandbox container while the caller already holds the
/// per-session instance mutex.
pub async fn ensure_container_for_session_locked(
    instances: &RwLock<Vec<Instance>>,
    session_id: &str,
    run_on_launch_hooks: bool,
) -> Result<Option<SandboxInfo>> {
    // Phase 1: short read on the instance list.
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
    // blocking thread.
    let session_id_owned = session_id.to_string();
    let (sandbox_info, container_workdir, hooks, hook_env) =
        tokio::task::spawn_blocking(move || -> Result<_> {
            reconcile_provider_container(&mut instance_clone)?;
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
            let env = crate::session::config::repo_config::lifecycle_env_vars(&instance_clone);
            Ok((instance_clone.sandbox_info.clone(), workdir, hooks, env))
        })
        .await
        .context("docker ensure task failed to join")??;

    if let Some(info) = &sandbox_info {
        let mut guard = instances.write().await;
        if let Some(inst) = guard.iter_mut().find(|i| i.id == session_id_owned) {
            if let Some(ref mut sb) = inst.sandbox_info {
                if sb.container_id.is_none() {
                    sb.container_id = info.container_id.clone();
                }
                sb.before_start_env = info.before_start_env.clone();
                sb.provider = info.provider.clone();
            }
        }
    }

    // Phase 4: run on_launch hooks outside the instances lock (the
    // per-session mutex is still held for the call's lifetime).
    if let (Some(cmds), Some(info)) = (hooks, sandbox_info.as_ref()) {
        if !cmds.is_empty() {
            let container_name = info.container_name.clone();
            let workdir = container_workdir.clone();
            let sid = session_id.to_string();
            match tokio::task::spawn_blocking(move || {
                crate::session::config::repo_config::execute_hooks_in_container_best_effort(
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

/// Rebuild the container when the session's provider pick no longer matches the
/// provider it was built for, then stamp the new pick so the build picks up the
/// matching credential mounts.
///
/// The GCP ADC bind mount is decided at container-create time, so a session
/// switched to Vertex inside a container built without it would get the routing
/// flag and no credentials. Recreating in the other direction matters too: it
/// drops a mount the session no longer routes through. A container built before
/// the pick existed records `None`, which matches no explicit pick and so is
/// rebuilt once on the first switch.
fn reconcile_provider_container(instance: &mut Instance) -> Result<()> {
    let Some(sandbox) = instance.sandbox_info.as_mut() else {
        return Ok(());
    };
    if sandbox.provider == instance.agent_provider {
        return Ok(());
    }
    let built_for = sandbox.provider.clone();
    sandbox.provider = instance.agent_provider.clone();

    let container = crate::containers::DockerContainer::from_session_id(&instance.id);
    match container.discard() {
        crate::containers::Teardown::Removed | crate::containers::Teardown::AlreadyGone => {
            tracing::info!(
                target: "acp.sandbox",
                session = %instance.id,
                built_for = ?built_for,
                provider = ?instance.agent_provider,
                "recreating sandbox container for the new provider"
            );
            Ok(())
        }
        crate::containers::Teardown::Failed(e) => anyhow::bail!(
            "failed to remove sandbox container {} built for a different provider; remove it \
             before switching, or the session keeps the old provider's credential mounts: {e}",
            container.name
        ),
    }
}
