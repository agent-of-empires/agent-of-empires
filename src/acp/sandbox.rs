//! Sandbox container lifecycle for structured view sessions.

use std::future::Future;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::{Mutex, RwLock};

use crate::acp::runner_lifecycle::ExecutionAdmission;
use crate::session::{Instance, SandboxInfo};

/// The owned driver retains this admission's preparation across mutex waits and effects.
pub fn ensure_container_for_session(
    instances: &Arc<RwLock<Vec<Instance>>>,
    instance_lock: &Arc<Mutex<()>>,
    admission: ExecutionAdmission,
    run_on_launch_hooks: bool,
) -> impl Future<Output = Result<Option<SandboxInfo>>> + Send + 'static {
    let instances = instances.clone();
    let instance_lock = instance_lock.clone();
    let custody = admission.begin_job();
    let driver = tokio::spawn(async move {
        let _custody = custody;
        let _guard = instance_lock.lock_owned().await;
        ensure_container_for_session_locked(&instances, admission, run_on_launch_hooks).await
    });
    async move { driver.await.context("owned sandbox ensure driver")? }
}

/// The caller owns its transition mutex; every blocking job independently retains custody.
pub fn ensure_container_for_session_locked(
    instances: &Arc<RwLock<Vec<Instance>>>,
    admission: ExecutionAdmission,
    run_on_launch_hooks: bool,
) -> impl Future<Output = Result<Option<SandboxInfo>>> + Send + 'static {
    let instances = instances.clone();
    let custody = admission.begin_job();
    let driver = tokio::spawn(async move {
        let _custody = custody;
        admission.check_active()?;
        let origin = admission
            .origin()
            .context("sandbox has no admitted original authority")?;
        let baseline = admission
            .original_baseline()
            .context("sandbox lost its immutable original baseline")?;
        anyhow::ensure!(
            origin.is_prepared_from(&baseline),
            "sandbox admission has not claimed preparation"
        );
        let effect_origin = origin.clone();
        let effect_admission = admission.clone();
        let custody = admission.begin_job();
        let (sandbox_info, container_workdir, hooks, hook_env) =
            tokio::task::spawn_blocking(move || {
                let _custody = custody;
                effect_admission.check_active()?;
                effect_origin.with_storage(|storage, mut instance| {
                    if !instance.is_sandboxed() {
                        return Ok((None, String::new(), None, Vec::new()));
                    }
                    instance
                        .get_container_for_instance()
                        .context("ensuring sandbox container")?;
                    let workdir = instance.container_workdir();
                    let hooks = if run_on_launch_hooks {
                        instance.resolve_on_launch_hooks(false, storage.profile())
                    } else {
                        None
                    };
                    let env = crate::session::config::repo_config::lifecycle_env_vars(&instance);
                    Ok((instance.sandbox_info, workdir, hooks, env))
                })
            })
            .await
            .context("original sandbox ensure job")??;

        // Reflect only this known CAS. Keep canonical original fences through the short
        // publication so a later Stop or replacement cannot receive an old result.
        let mut rows = instances.write_owned().await;
        let publication_origin = origin.clone();
        let publication_info = sandbox_info
            .as_ref()
            .map(|info| (info.container_id.clone(), info.before_start_env.clone()));
        let publication_admission = admission.clone();
        let custody = admission.begin_job();
        tokio::task::spawn_blocking(move || {
            let _custody = custody;
            publication_origin.with_storage(|_, _stored| {
                publication_admission.commit_effect(|| {
                    let instance = rows
                        .iter_mut()
                        .find(|row| row.id == publication_origin.session_id())
                        .context("sandbox original view row disappeared")?;
                    if baseline.matches_instance(instance) {
                        instance.lifecycle_generation = publication_origin.generation();
                    }
                    anyhow::ensure!(
                        publication_origin.matches_instance(instance),
                        "sandbox result no longer owns its original view row"
                    );
                    if let (Some((container_id, before_start_env)), Some(sandbox)) =
                        (publication_info, instance.sandbox_info.as_mut())
                    {
                        if sandbox.container_id.is_none() {
                            sandbox.container_id = container_id;
                        }
                        sandbox.before_start_env = before_start_env;
                    }
                    Ok(())
                })
            })
        })
        .await
        .context("original sandbox publication job")??;

        if let (Some(commands), Some(info)) = (hooks, sandbox_info.as_ref()) {
            if !commands.is_empty() {
                let container_name = info.container_name.clone();
                let effect_origin = origin.clone();
                let effect_admission = admission.clone();
                let custody = admission.begin_job();
                let errors = tokio::task::spawn_blocking(move || {
                    let _custody = custody;
                    effect_admission.check_active()?;
                    effect_origin.validate()?;
                    Ok::<_, anyhow::Error>(
                        crate::session::config::repo_config::execute_hooks_in_container_best_effort(
                            &commands,
                            &container_name,
                            &container_workdir,
                            true,
                            &hook_env,
                        ),
                    )
                })
                .await
                .context("original sandbox hook job")??;
                for error in errors {
                    tracing::warn!(target: "acp.sandbox", session = %origin.session_id(), "on_launch hook failed: {error}");
                }
            }
        }
        Ok(sandbox_info)
    });
    async move { driver.await.context("owned original sandbox job driver")? }
}
