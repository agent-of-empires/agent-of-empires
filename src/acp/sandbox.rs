//! Sandbox container lifecycle for structured view sessions.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::{Mutex, RwLock};

use crate::acp::runner_lifecycle::ExecutionAdmission;
use crate::session::{Instance, SandboxInfo};

/// The owned driver retains this admission's preparation across mutex waits and effects.
pub fn ensure_container_for_session(
    instances: &Arc<RwLock<Vec<Instance>>>,
    mutation_epoch: &Arc<AtomicU64>,
    instance_lock: &Arc<Mutex<()>>,
    admission: ExecutionAdmission,
    run_on_launch_hooks: bool,
) -> impl Future<Output = Result<Option<SandboxInfo>>> + Send + 'static {
    let instances = instances.clone();
    let mutation_epoch = mutation_epoch.clone();
    let instance_lock = instance_lock.clone();
    let custody = admission.begin_job();
    let driver = tokio::spawn(async move {
        let _custody = custody;
        let _guard = instance_lock.lock_owned().await;
        ensure_container_for_session_locked(
            &instances,
            &mutation_epoch,
            admission,
            run_on_launch_hooks,
        )
        .await
    });
    async move { driver.await.context("owned sandbox ensure driver")? }
}

/// The caller owns its transition mutex; every blocking job independently retains custody.
pub fn ensure_container_for_session_locked(
    instances: &Arc<RwLock<Vec<Instance>>>,
    mutation_epoch: &Arc<AtomicU64>,
    admission: ExecutionAdmission,
    run_on_launch_hooks: bool,
) -> impl Future<Output = Result<Option<SandboxInfo>>> + Send + 'static {
    let instances = instances.clone();
    let mutation_epoch = mutation_epoch.clone();
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
        let (sandbox_info, container_workdir, hooks, hook_env, rebuilt) =
            tokio::task::spawn_blocking(move || {
                let _custody = custody;
                effect_admission.check_active()?;
                effect_origin.with_storage(|storage, mut instance| {
                    if !instance.is_sandboxed() {
                        return Ok((None, String::new(), None, Vec::new(), false));
                    }
                    let rebuild = instance
                        .sandbox_info
                        .as_ref()
                        .is_some_and(|sandbox| sandbox.provider != instance.agent_provider);
                    let rebuilt = if rebuild {
                        storage.update_under_workspace_claim_lock(|rows, _| {
                            let row = rows
                                .iter_mut()
                                .find(|row| row.id == effect_origin.session_id())
                                .context(
                                    "original sandbox row disappeared before provider rebuild",
                                )?;
                            effect_origin.validate_baseline_at(row, effect_origin.generation())?;
                            effect_admission.commit_effect(|| {
                                let rebuilt = reconcile_provider_container_with(row, |id| {
                                    crate::containers::DockerContainer::from_session_id(id)
                                        .discard()
                                })?;
                                instance.sandbox_info.clone_from(&row.sandbox_info);
                                Ok(rebuilt)
                            })
                        })?
                    } else {
                        false
                    };
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
                    Ok((instance.sandbox_info, workdir, hooks, env, rebuilt))
                })
            })
            .await
            .context("original sandbox ensure job")??;

        // Keep physical fences through the cache publication.
        let publication_origin = origin.clone();
        let publication_info = sandbox_info.as_ref().map(|info| {
            (
                info.container_id.clone(),
                info.before_start_env.clone(),
                info.provider.clone(),
            )
        });
        let publication_admission = admission.clone();
        let custody = admission.begin_job();
        tokio::task::spawn_blocking(move || {
            let _custody = custody;
            publication_origin.with_storage(|_, stored| {
                publication_admission.commit_effect(|| {
                    let mut rows = instances.blocking_write();
                    let instance = rows
                        .iter_mut()
                        .find(|row| row.id == publication_origin.session_id())
                        .context("sandbox original view row disappeared")?;
                    anyhow::ensure!(
                        baseline.recognizes_published_instance(instance)
                            || publication_origin.recognizes_published_instance(instance),
                        "sandbox result no longer owns its original view row"
                    );
                    instance.merge_post_start(&stored);
                    if let (Some((container_id, before_start_env, provider)), Some(sandbox)) =
                        (publication_info, instance.sandbox_info.as_mut())
                    {
                        if rebuilt || sandbox.container_id.is_none() {
                            sandbox.container_id = container_id;
                        }
                        sandbox.before_start_env = before_start_env;
                        if rebuilt || sandbox.provider != provider {
                            sandbox.provider = provider;
                        }
                    }
                    mutation_epoch.fetch_add(1, Ordering::SeqCst);
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

/// The caller commits this stamp on the original row after discard and before rebuild.
fn reconcile_provider_container_with(
    instance: &mut Instance,
    discard: impl FnOnce(&str) -> crate::containers::Teardown,
) -> Result<bool> {
    let Some(sandbox) = instance.sandbox_info.as_mut() else {
        return Ok(false);
    };
    if sandbox.provider == instance.agent_provider {
        return Ok(false);
    }
    if let crate::containers::Teardown::Failed(error) = discard(&instance.id) {
        anyhow::bail!("failed to remove sandbox container built for a different provider: {error}");
    }
    sandbox.provider.clone_from(&instance.agent_provider);
    Ok(true)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::containers::Teardown;

    #[test]
    #[serial_test::serial]
    fn a_rebuild_is_recorded_on_disk_before_it_runs() {
        for (outcome, rebuilt) in [
            (Teardown::Removed, true),
            (Teardown::AlreadyGone, true),
            (
                Teardown::Failed(crate::containers::error::DockerError::DaemonNotRunning),
                false,
            ),
        ] {
            let _home = crate::session::test_support::isolate_app_dir();
            let mut row = Instance::new("claude", "/tmp/aoe-provider-stamp");
            row.agent_provider = Some("vertex".into());
            row.sandbox_info = Some(SandboxInfo {
                provider: None,
                enabled: true,
                container_id: None,
                image: "alpine:latest".into(),
                container_name: "aoe-sandbox-stamp".into(),
                extra_env: None,
                custom_instruction: None,
                before_start_env: Vec::new(),
                container_workdir: None,
            });
            let id = row.id.clone();
            crate::server::test_support::seed_instances_on_disk_for_test("default", vec![row]);
            let original = crate::session::runner_journal::capture_unique_origin(&id).unwrap();
            let mut discards = 0;
            let result = original.update_storage(
                |_, stored| {
                    reconcile_provider_container_with(stored, |_| {
                        discards += 1;
                        outcome
                    })
                },
                Ok,
            );
            assert_eq!(result.is_ok(), rebuilt, "{result:?}");
            let stored = crate::server::test_support::load_instances_from_disk_for_test("default")
                .into_iter()
                .find(|row| row.id == id)
                .unwrap();
            assert_eq!(
                stored.sandbox_info.unwrap().provider.as_deref(),
                rebuilt.then_some("vertex")
            );
            if rebuilt {
                let repeated = original
                    .update_storage(
                        |_, stored| {
                            reconcile_provider_container_with(stored, |_| {
                                discards += 1;
                                Teardown::Removed
                            })
                        },
                        Ok,
                    )
                    .unwrap();
                assert!(!repeated, "a matching rebuilt container must survive");
                assert_eq!(discards, 1);
            }
        }
    }
}
