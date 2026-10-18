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
    ensure_container_for_session_with_request_env_locked(
        instances,
        mutation_epoch,
        session_id,
        run_on_launch_hooks,
        None,
        Vec::new(),
    )
    .await
}

/// [`ensure_container_for_session_locked`] for a launch that carries a request's
/// `provider_env` and the agent the request selected. The sandbox login admission counts a
/// credential the request supplies for this launch, as the launch forwards it, and reads
/// the allowlist of `launch_agent`'s adapter, the one the launch forwards host variables
/// by (`None` is the stored agent). Both live in memory for the check only: they are
/// neither stored nor logged, and a later launch without the request is admitted on what
/// is stored.
pub async fn ensure_container_for_session_with_request_env_locked(
    instances: &RwLock<Vec<Instance>>,
    mutation_epoch: &AtomicU64,
    session_id: &str,
    run_on_launch_hooks: bool,
    launch_agent: Option<String>,
    request_env: Vec<(String, String)>,
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
    let (sandbox_info, container_workdir, hooks, hook_env, rebuilt) =
        tokio::task::spawn_blocking(move || -> Result<_> {
            instance_clone.ensure_sandbox_launchable_for(launch_agent.as_deref(), &request_env)?;
            let rebuilt = reconcile_provider_container(&mut instance_clone)?;
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
            Ok((
                instance_clone.sandbox_info.clone(),
                workdir,
                hooks,
                env,
                rebuilt,
            ))
        })
        .await
        .context("docker ensure task failed to join")??;

    if let Some(info) = &sandbox_info {
        record_built_sandbox(instances, mutation_epoch, &session_id_owned, info, rebuilt).await;
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
    rebuilt: bool,
) {
    let mut guard = instances.write().await;
    let Some(sb) = guard
        .iter_mut()
        .find(|i| i.id == session_id)
        .and_then(|i| i.sandbox_info.as_mut())
    else {
        return;
    };
    let id_changed = sb.container_id != info.container_id;
    if id_changed {
        sb.container_id.clone_from(&info.container_id);
    }
    sb.before_start_env = info.before_start_env.clone();
    // Reject snapshots read before a changed container identity or provider.
    if rebuilt || id_changed || sb.provider != info.provider {
        sb.provider.clone_from(&info.provider);
        mutation_epoch.fetch_add(1, Ordering::SeqCst);
    }
}

/// The environment the sandboxed launch of `agent` runs with, as far as its authentication
/// goes: `sandbox_env`, then the request's `provider_env`, then the adapter's allowlisted
/// host variables, exactly as the launch claims them. The adapter spec is resolved as the
/// supervisor resolves it: the registry entry, else a custom `agent_acp_cmd` (no allowlist),
/// else the `agent_detect_as` base.
pub(crate) fn effective_launch_env(
    agent: &str,
    session: &crate::session::config::SessionConfig,
    sandbox_env: &[crate::containers::EnvEntry],
    request_env: &[(String, String)],
) -> Vec<crate::containers::EnvEntry> {
    let registry = crate::acp::AgentRegistry::with_defaults();
    let spec = registry.get(agent).or_else(|| {
        if session.agent_acp_cmd.contains_key(agent) {
            return None;
        }
        let base = crate::acp::inherited_acp_base(agent, &session.agent_detect_as)?;
        registry.get(&base)
    });
    crate::acp::acp_client::effective_launch_env(
        sandbox_env,
        request_env,
        spec.and_then(|s| s.env_allowlist.as_deref()),
    )
}

/// Rebuild the container when the session's provider pick no longer matches the
/// provider it was built for. The GCP ADC bind mount is decided at
/// container-create time, so a container built for another provider has the
/// wrong credential mounts. A container built before the pick existed records
/// `None`, which matches no explicit pick and so is rebuilt once.
fn reconcile_provider_container(instance: &mut Instance) -> Result<bool> {
    reconcile_provider_container_with(instance, |id| {
        crate::containers::DockerContainer::from_session_id(id).discard()
    })
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

    /// A poll that read the pre-rebuild row must not land after the build, even
    /// when the watcher already applied the new stamp from disk.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_rebuild_invalidates_snapshots_read_before_it() {
        let _tmp = crate::session::test_support::isolate_app_dir();
        let old = sandboxed(None);
        let id = old.id.clone();
        let state = crate::server::test_support::build_test_app_state(vec![old.clone()]);
        let mut rebuilt = old.clone();
        rebuilt.sandbox_info.as_mut().unwrap().provider = Some("vertex".to_string());
        let reload = |snapshot: &Instance, read_epoch| {
            crate::server::reload::reload_state_instances_from_disk(
                &state,
                vec![snapshot.clone()].into(),
                Vec::new(),
                crate::server::state::StatusSource::DiskOnly,
                read_epoch,
            )
        };

        let stale_epoch = state.mutation_epoch.load(Ordering::SeqCst);
        reload(&rebuilt, stale_epoch).await;
        record_built_sandbox(
            &state.instances,
            &state.mutation_epoch,
            &id,
            rebuilt.sandbox_info.as_ref().unwrap(),
            true,
        )
        .await;
        reload(&old, stale_epoch).await;

        let stamp = state.instances.read().await[0]
            .sandbox_info
            .as_ref()
            .and_then(|s| s.provider.clone());
        assert_eq!(stamp.as_deref(), Some("vertex"));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn changed_container_identity_survives_a_stale_disk_snapshot() {
        let _tmp = crate::session::test_support::isolate_app_dir();
        let mut old = sandboxed(None);
        old.sandbox_info.as_mut().unwrap().container_id = Some("retired".to_string());
        let state = crate::server::test_support::build_test_app_state(vec![old.clone()]);
        let stale_epoch = state.mutation_epoch.load(Ordering::SeqCst);
        let mut current = old.sandbox_info.as_ref().unwrap().clone();
        current.container_id = Some("current".to_string());
        record_built_sandbox(
            &state.instances,
            &state.mutation_epoch,
            &old.id,
            &current,
            false,
        )
        .await;
        crate::server::reload::reload_state_instances_from_disk(
            &state,
            vec![old].into(),
            Vec::new(),
            crate::server::state::StatusSource::DiskOnly,
            stale_epoch,
        )
        .await;
        let instances = state.instances.read().await;
        assert_eq!(
            instances[0]
                .sandbox_info
                .as_ref()
                .unwrap()
                .container_id
                .as_deref(),
            Some("current")
        );
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
