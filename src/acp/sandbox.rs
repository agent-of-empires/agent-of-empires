//! Sandbox container lifecycle for structured view sessions.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::{Mutex, RwLock};

use crate::session::{Instance, SandboxInfo};

/// Ensure the sandbox container for the named session is running.
pub async fn ensure_container_for_session(
    instances: &RwLock<Vec<Instance>>,
    mutation_epoch: &AtomicU64,
    instance_lock: &Arc<Mutex<()>>,
    session_id: &str,
    run_on_launch_hooks: bool,
) -> Result<Option<SandboxInfo>> {
    let _guard = instance_lock.lock().await;

    ensure_container_for_session_locked(instances, mutation_epoch, session_id, run_on_launch_hooks)
        .await
}

/// Ensure the sandbox container while the caller already holds the
/// per-session instance mutex.
pub async fn ensure_container_for_session_locked(
    instances: &RwLock<Vec<Instance>>,
    mutation_epoch: &AtomicU64,
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
        record_built_sandbox(instances, mutation_epoch, &session_id_owned, info).await;
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

/// Copy what the build learned onto the live row.
async fn record_built_sandbox(
    instances: &RwLock<Vec<Instance>>,
    mutation_epoch: &AtomicU64,
    session_id: &str,
    info: &SandboxInfo,
) {
    let mut guard = instances.write().await;
    let Some(sb) = guard
        .iter_mut()
        .find(|i| i.id == session_id)
        .and_then(|i| i.sandbox_info.as_mut())
    else {
        return;
    };
    if sb.container_id.is_none() {
        sb.container_id = info.container_id.clone();
    }
    sb.before_start_env = info.before_start_env.clone();
    if sb.provider != info.provider {
        sb.provider = info.provider.clone();
        // A disk snapshot read before the rebuild would restore the old stamp,
        // and the next ensure would discard this container.
        mutation_epoch.fetch_add(1, Ordering::SeqCst);
    }
}

/// Rebuild the container when the session's provider pick no longer matches the
/// provider it was built for. The GCP ADC bind mount is decided at
/// container-create time, so a container built for another provider has the
/// wrong credential mounts. A container built before the pick existed records
/// `None`, which matches no explicit pick and so is rebuilt once.
fn reconcile_provider_container(instance: &mut Instance) -> Result<()> {
    reconcile_provider_container_with(instance, |id| {
        crate::containers::DockerContainer::from_session_id(id).discard()
    })
    .map(|_| ())
}

/// Returns whether the old container was discarded.
///
/// The new stamp reaches disk after the discard and before the rebuild. A
/// reload restores the disk row, so a stamp held only in memory would have the
/// next resume discard the rebuilt, correct container again. Written any
/// earlier, a failed discard would leave the stamp claiming mounts the old
/// container lacks. A failed write leaves no container, so the next resume
/// retries; a failed rebuild leaves the stamp, so the next resume builds for it.
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
    let built_for = sandbox.provider.clone();
    if let crate::containers::Teardown::Failed(e) = discard(&instance.id) {
        anyhow::bail!(
            "failed to remove sandbox container {} built for a different provider; remove it \
             before switching, or the session keeps the old provider's credential mounts: {e}",
            crate::containers::DockerContainer::from_session_id(&instance.id).name
        );
    }
    sandbox.provider = instance.agent_provider.clone();
    instance
        .persist_sandbox_provider()
        .context("recording the provider the sandbox container is rebuilt for")?;
    tracing::info!(
        target: "acp.sandbox",
        session = %instance.id,
        built_for = ?built_for,
        provider = ?instance.agent_provider,
        "recreating sandbox container for the new provider"
    );
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::containers::Teardown;

    fn sandboxed(provider_built_for: Option<&str>) -> Instance {
        let mut inst = Instance::new("claude", "/tmp/aoe-provider-stamp");
        inst.agent_provider = Some("vertex".to_string());
        inst.sandbox_info = Some(SandboxInfo {
            provider: provider_built_for.map(str::to_string),
            enabled: true,
            container_id: None,
            image: "alpine:latest".into(),
            container_name: "aoe-sandbox-stamp".into(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        });
        inst
    }

    fn stored_stamp(id: &str) -> Option<String> {
        crate::server::test_support::load_instances_from_disk_for_test("default")
            .into_iter()
            .find(|i| i.id == id)
            .and_then(|i| i.sandbox_info)
            .and_then(|s| s.provider)
    }

    /// A changed stamp invalidates disk snapshots read before it; an unchanged
    /// one leaves reloads alone.
    #[tokio::test]
    async fn a_changed_stamp_invalidates_older_snapshots() {
        let inst = sandboxed(None);
        let id = inst.id.clone();
        let instances = RwLock::new(vec![inst]);
        let epoch = AtomicU64::new(0);
        let mut built = sandboxed(Some("vertex")).sandbox_info.unwrap();

        for (case, expected_epoch) in [("rebuilt", 1), ("unchanged", 1)] {
            record_built_sandbox(&instances, &epoch, &id, &built).await;
            assert_eq!(epoch.load(Ordering::SeqCst), expected_epoch, "{case}");
            let stamp = instances.read().await[0]
                .sandbox_info
                .as_ref()
                .and_then(|s| s.provider.clone());
            assert_eq!(stamp.as_deref(), Some("vertex"), "{case}");
            built.container_id = Some("c1".into());
        }
    }

    /// The path every resume takes, not only a switch: a rebuild survives a
    /// reload and the next same-provider resume leaves it alone, while a
    /// failed discard records nothing.
    #[test]
    #[serial_test::serial]
    fn a_rebuild_is_recorded_on_disk_before_it_runs() {
        for (case, outcome, rebuilt) in [
            ("removed", Teardown::Removed, true),
            ("already gone", Teardown::AlreadyGone, true),
            (
                "discard failed",
                Teardown::Failed(crate::containers::error::DockerError::DaemonNotRunning),
                false,
            ),
        ] {
            let _tmp = crate::session::test_support::isolate_app_dir();
            let mut inst = sandboxed(None);
            let id = inst.id.clone();
            crate::server::test_support::seed_instances_on_disk_for_test(
                "default",
                vec![inst.clone()],
            );

            let mut discards = 0;
            let result = reconcile_provider_container_with(&mut inst, |_| {
                discards += 1;
                outcome
            });
            assert_eq!(result.is_ok(), rebuilt, "{case}: {result:?}");
            let expected = rebuilt.then(|| "vertex".to_string());
            assert_eq!(stored_stamp(&id), expected, "{case}");
            if !rebuilt {
                continue;
            }

            let mut reloaded =
                crate::server::test_support::load_instances_from_disk_for_test("default")
                    .into_iter()
                    .find(|i| i.id == id)
                    .expect("seeded row");
            let again = reconcile_provider_container_with(&mut reloaded, |_| {
                discards += 1;
                Teardown::Removed
            });
            assert!(
                !again.unwrap(),
                "{case}: the rebuilt container must survive"
            );
            assert_eq!(discards, 1, "{case}");
        }
    }
}
