//! Sandbox container lifecycle for structured view sessions.

use crate::server::session_store::NativeSessionStore;
use crate::session::{Instance, SessionStore};
use anyhow::{Context, Result};
use std::sync::Arc;

pub(crate) struct LaunchExclusion {
    state: Arc<crate::server::AppState>,
    namespace: Option<tokio::sync::OwnedRwLockReadGuard<()>>,
    instance: Option<tokio::sync::OwnedMutexGuard<()>>,
    submission: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl LaunchExclusion {
    pub(crate) async fn acquire(
        state: Arc<crate::server::AppState>,
        id: &str,
        submission: bool,
    ) -> Result<Self> {
        let namespace = state.profile_namespace.clone().read_owned().await;
        let submission = if submission {
            Some(
                state
                    .session_service
                    .prompt_submission_for_session(id)
                    .await
                    .ok_or(crate::session::SessionGone)?,
            )
        } else {
            None
        };
        let instance = state.instance_lock(id).await.lock_owned().await;
        Ok(Self {
            state,
            namespace: Some(namespace),
            instance: Some(instance),
            submission,
        })
    }

    fn release_for_hook(&mut self) -> bool {
        let submission = self.submission.is_some();
        self.instance.take();
        self.submission.take();
        self.namespace.take();
        submission
    }

    pub(crate) fn take_submission(&mut self) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        self.submission.take()
    }

    fn mint_before_start(
        &mut self,
        instance: &mut Instance,
        config: &crate::session::LaunchConfig,
        store: &NativeSessionStore,
    ) -> Result<()> {
        let submission = self.release_for_hook();
        let result = instance.mint_before_start_env(config);
        *self = tokio::runtime::Handle::current().block_on(Self::acquire(
            self.state.clone(),
            &instance.id,
            submission,
        ))?;
        validate_launch(store, instance)?;
        result
    }
}

pub(crate) async fn ensure_container_for_session(
    store: Arc<NativeSessionStore>,
    mut instance: Instance,
    mut exclusion: LaunchExclusion,
) -> Result<(Option<crate::session::SandboxInfo>, LaunchExclusion)> {
    tokio::task::spawn_blocking(move || {
        validate_launch(&*store, &instance)?;
        if !instance.is_sandboxed() {
            return Ok((None, exclusion));
        }
        reconcile_provider_container_with(&mut instance, &*store, |id| {
            crate::containers::DockerContainer::from_session_id(id).discard()
        })?;
        instance
            .ensure_container_with_hook_in(
                &*store,
                &tokio_util::sync::CancellationToken::new(),
                |instance, config| exclusion.mint_before_start(instance, config, &store),
            )
            .context("ensuring sandbox container")?;
        record_built_sandbox(&store, &instance)?;
        Ok((instance.sandbox_info, exclusion))
    })
    .await
    .context("docker ensure task failed to join")?
}

fn validate_launch(store: &dyn SessionStore, expected: &Instance) -> Result<()> {
    let row = store
        .load()?
        .into_iter()
        .find(|row| row.id == expected.id)
        .ok_or(crate::session::LifecycleReservationError::Superseded)?;
    anyhow::ensure!(
        store.storage().profile() == expected.source_profile
            && row.lifecycle_generation == expected.lifecycle_generation
            && row.title == expected.title
            && row.agent_provider == expected.agent_provider
            && row.is_structured()
            && row.launch_is_finalized(),
        crate::session::LifecycleReservationError::Superseded
    );
    row.ensure_startable()?;
    Ok(())
}

fn record_built_sandbox(store: &NativeSessionStore, instance: &Instance) -> Result<()> {
    let _lifecycle = store
        .storage()
        .acquire_instance_lifecycle_lock(&instance.id)?;
    validate_launch(store, instance)?;
    store.commit(&mut |rows, _| {
        let row = rows
            .iter_mut()
            .find(|row| row.id == instance.id)
            .ok_or(crate::session::LifecycleReservationError::Superseded)?;
        anyhow::ensure!(
            row.lifecycle_generation == instance.lifecycle_generation
                && row.title == instance.title
                && row.agent_provider == instance.agent_provider,
            crate::session::LifecycleReservationError::Superseded
        );
        row.sandbox_info = instance.sandbox_info.clone();
        Ok(())
    })?;
    store.adopt_runtime_fields(instance)
}

/// Discard first, persist the new mount stamp second, then build. A failed
/// discard retains the old stamp; a failed build can retry the new mounts.
fn reconcile_provider_container_with(
    instance: &mut Instance,
    store: &dyn SessionStore,
    discard: impl FnOnce(&str) -> crate::containers::Teardown,
) -> Result<bool> {
    let _lifecycle = store
        .storage()
        .acquire_instance_lifecycle_lock(&instance.id)?;
    validate_launch(store, instance)?;
    let Some(sandbox) = instance.sandbox_info.as_ref() else {
        return Ok(false);
    };
    if sandbox.provider == instance.agent_provider {
        return Ok(false);
    }
    if let crate::containers::Teardown::Failed(error) = discard(&instance.id) {
        anyhow::bail!(
            "failed to remove sandbox container {} built for a different provider: {error}",
            instance.id
        );
    }
    store
        .commit(&mut |rows, _| {
            let row = rows
                .iter_mut()
                .find(|row| row.id == instance.id)
                .ok_or(crate::session::LifecycleReservationError::Superseded)?;
            anyhow::ensure!(
                row.lifecycle_generation == instance.lifecycle_generation
                    && row.title == instance.title
                    && row.agent_provider == instance.agent_provider,
                crate::session::LifecycleReservationError::Superseded
            );
            let sandbox = row
                .sandbox_info
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("sandbox removed"))?;
            sandbox.provider = instance.agent_provider.clone();
            Ok(())
        })
        .context("recording the provider the sandbox container is rebuilt for")?;
    instance.sandbox_info.as_mut().unwrap().provider = instance.agent_provider.clone();
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::containers::Teardown;
    use crate::session::SandboxInfo;
    use std::sync::atomic::Ordering;

    fn sandboxed(built_for: Option<&str>) -> Instance {
        let mut instance = Instance::new("claude", "/tmp/aoe-provider-stamp");
        instance.source_profile = "default".into();
        instance.view = crate::session::View::Structured;
        instance.agent_provider = Some("vertex".into());
        instance.sandbox_info = Some(SandboxInfo {
            provider: built_for.map(str::to_owned),
            enabled: true,
            container_id: Some("old-container".into()),
            image: "alpine:latest".into(),
            container_name: "aoe-sandbox-stamp".into(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: Some("/old-workdir".into()),
        });
        instance
    }

    async fn native(
        instance: &Instance,
    ) -> (Arc<crate::server::AppState>, Arc<NativeSessionStore>) {
        crate::server::test_support::seed_instances_on_disk_for_test(
            "default",
            vec![instance.clone()],
        );
        let state = crate::server::test_support::build_test_app_state(vec![instance.clone()]);
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let (store, ()) = NativeSessionStore::open_for_launch(state.clone(), instance, ())
            .await
            .unwrap();
        (state, store)
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_rebuild_invalidates_snapshots_read_before_it() {
        let _tmp = crate::session::test_support::isolate_app_dir();
        let old = sandboxed(None);
        let (state, store) = native(&old).await;
        let epoch = state.mutation_epoch.load(Ordering::SeqCst);
        let mut rebuilt = old.clone();
        let sandbox = rebuilt.sandbox_info.as_mut().unwrap();
        sandbox.provider = Some("vertex".into());
        sandbox.container_id = Some("new-container".into());
        sandbox.container_workdir = Some("/new-workdir".into());
        tokio::task::spawn_blocking(move || record_built_sandbox(&store, &rebuilt))
            .await
            .unwrap()
            .unwrap();
        crate::server::test_support::reload_disk_snapshot_at_epoch_for_test(
            &state,
            vec![old],
            epoch,
        )
        .await;
        let rows = state.instances.read().await;
        let sandbox = rows[0].sandbox_info.as_ref().unwrap();
        assert_eq!(sandbox.provider.as_deref(), Some("vertex"));
        assert_eq!(sandbox.container_id.as_deref(), Some("new-container"));
        assert_eq!(sandbox.container_workdir.as_deref(), Some("/new-workdir"));
        let disk = crate::server::test_support::load_instances_from_disk_for_test("default");
        assert_eq!(
            disk[0]
                .sandbox_info
                .as_ref()
                .unwrap()
                .container_id
                .as_deref(),
            Some("new-container")
        );
        assert_eq!(
            disk[0]
                .sandbox_info
                .as_ref()
                .unwrap()
                .container_workdir
                .as_deref(),
            Some("/new-workdir")
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_rebuild_is_recorded_on_disk_before_it_runs() {
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
            let mut instance = sandboxed(None);
            let (state, store) = native(&instance).await;
            tokio::task::spawn_blocking(move || {
                let mut discards = 0;
                let result = reconcile_provider_container_with(&mut instance, &*store, |_| {
                    discards += 1;
                    outcome
                });
                assert_eq!(result.is_ok(), rebuilt, "{case}: {result:?}");
                let disk = store.load().unwrap();
                assert_eq!(
                    disk[0].sandbox_info.as_ref().unwrap().provider.as_deref(),
                    rebuilt.then_some("vertex"),
                    "{case}"
                );
                if rebuilt {
                    let again = reconcile_provider_container_with(&mut instance, &*store, |_| {
                        discards += 1;
                        Teardown::Removed
                    });
                    assert!(
                        !again.unwrap(),
                        "{case}: the rebuilt container must survive"
                    );
                }
                assert_eq!(discards, 1, "{case}");
            })
            .await
            .unwrap();
            drop(state);
        }
    }
}
