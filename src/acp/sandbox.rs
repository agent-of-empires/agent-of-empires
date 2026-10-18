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

/// The sandbox login admission of a launch, without bringing anything up: a refusal leaves
/// the session's worker and container as they are. It resolves a `before_start` hook the
/// cheap decision cannot see, once per session, so the launch that follows finds the
/// resolution. The hook runs with the launch locks released, as the come-up's does.
pub(crate) async fn admit_sandbox_launch(
    store: Arc<NativeSessionStore>,
    mut instance: Instance,
    mut exclusion: LaunchExclusion,
    launch_agent: Option<String>,
    request_env: Vec<(String, String)>,
) -> Result<LaunchExclusion> {
    tokio::task::spawn_blocking(move || {
        validate_launch(&*store, &instance)?;
        if instance.is_sandboxed() {
            instance.ensure_sandbox_launchable_for(
                &*store,
                launch_agent.as_deref(),
                &request_env,
                &mut |instance: &mut Instance, config: &crate::session::LaunchConfig| {
                    exclusion.mint_before_start(instance, config, &store)
                },
            )?;
        }
        Ok(exclusion)
    })
    .await
    .context("sandbox login admission task failed to join")?
}

pub(crate) async fn ensure_container_for_session(
    store: Arc<NativeSessionStore>,
    instance: Instance,
    exclusion: LaunchExclusion,
) -> Result<(Option<crate::session::SandboxInfo>, LaunchExclusion)> {
    ensure_container_for_session_with_request_env(store, instance, exclusion, None, Vec::new())
        .await
}

/// [`ensure_container_for_session`] for a launch that carries a request's `provider_env` and
/// the agent the request selected. The sandbox login admission counts a credential the
/// request supplies for this launch, as the launch forwards it, and reads the allowlist of
/// `launch_agent`'s adapter, the one the launch forwards host variables by (`None` is the
/// stored agent). Both live in memory for the check only: they are neither stored nor
/// logged, and a later launch without the request is admitted on what is stored.
pub(crate) async fn ensure_container_for_session_with_request_env(
    store: Arc<NativeSessionStore>,
    mut instance: Instance,
    mut exclusion: LaunchExclusion,
    launch_agent: Option<String>,
    request_env: Vec<(String, String)>,
) -> Result<(Option<crate::session::SandboxInfo>, LaunchExclusion)> {
    tokio::task::spawn_blocking(move || {
        validate_launch(&*store, &instance)?;
        if !instance.is_sandboxed() {
            return Ok((None, exclusion));
        }
        instance.ensure_sandbox_launchable_for(
            &*store,
            launch_agent.as_deref(),
            &request_env,
            &mut |instance: &mut Instance, config: &crate::session::LaunchConfig| {
                exclusion.mint_before_start(instance, config, &store)
            },
        )?;
        reconcile_provider_container_with(&mut instance, &*store, |id| {
            crate::containers::DockerContainer::from_session_id(id).discard()
        })?;
        let (container, created) = instance
            .ensure_container_with_hook_in(
                &*store,
                &tokio_util::sync::CancellationToken::new(),
                |instance, config| exclusion.mint_before_start(instance, config, &store),
            )
            .context("ensuring sandbox container")?;
        publish_built_sandbox(&store, &instance, &container, created)?;
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

fn publish_built_sandbox(
    store: &NativeSessionStore,
    instance: &Instance,
    container: &crate::containers::DockerContainer,
    created: bool,
) -> Result<()> {
    if let Err(error) = record_built_sandbox(store, instance) {
        if created
            && !error.is::<crate::session::SessionCommitApplied>()
            && (error.is::<crate::session::LifecycleReservationError>()
                || error.is::<crate::session::SessionGone>())
        {
            let id = instance
                .sandbox_info
                .as_ref()
                .and_then(|sandbox| sandbox.container_id.as_deref())
                .expect("successful construction records its container ID");
            if let Err(cleanup) = container.remove_created(id) {
                anyhow::bail!("Sandbox construction was superseded: {error:#}; created container {id} requires cleanup: {cleanup}");
            }
        }
        return Err(error);
    }
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
    store
        .adopt_runtime_fields(instance)
        .context(crate::session::SessionCommitApplied)
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
            before_start_hook_env: Vec::new(),
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

    #[cfg(unix)]
    fn construction_instance(project: &std::path::Path) -> Instance {
        let mut instance = sandboxed(None);
        instance.project_path = project.to_string_lossy().into_owned();
        instance.tool = "bash".into();
        instance.agent_provider = None;
        instance.sandbox_store_generation =
            crate::session::config::container_config::CURRENT_SANDBOX_STORE_GENERATION;
        let sandbox = instance.sandbox_info.as_mut().unwrap();
        sandbox.container_id = None;
        sandbox.container_name = crate::containers::DockerContainer::generate_name(&instance.id);
        sandbox.container_workdir = None;
        instance
    }

    #[cfg(unix)]
    struct ConstructionHookCleanup(String);

    #[cfg(unix)]
    impl Drop for ConstructionHookCleanup {
        fn drop(&mut self) {
            crate::hooks::cleanup_hook_status_dir(&self.0);
        }
    }

    #[cfg(unix)]
    async fn construct_for_test(
        store: Arc<NativeSessionStore>,
        mut instance: Instance,
    ) -> (Instance, crate::containers::DockerContainer, bool) {
        tokio::task::spawn_blocking(move || -> Result<_> {
            let (container, created) = instance.ensure_container_with_hook_in(
                &*store,
                &tokio_util::sync::CancellationToken::new(),
                Instance::mint_before_start_env,
            )?;
            Ok((instance, container, created))
        })
        .await
        .unwrap()
        .unwrap()
    }

    #[cfg(unix)]
    fn construction_runtime() -> (tempfile::TempDir, crate::session::test_support::EnvGuard) {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        std::fs::create_dir(root.path().join("resources")).unwrap();
        std::fs::write(root.path().join("next-id"), "built-original").unwrap();
        // IDs own resource directories. Names are mutable attributes, not deletion keys.
        // Both ID and name lookup work, so name-based cleanup would remove a replacement.
        let script = r#"#!/bin/sh
set -eu
root=@ROOT@
find_resource() {
    if [ -d "$root/resources/$1" ]; then
        resource="$root/resources/$1"
        return 0
    fi
    for candidate in "$root"/resources/*; do
        [ -d "$candidate" ] || continue
        if [ "$(cat "$candidate/name")" = "$1" ]; then
            resource="$candidate"
            return 0
        fi
    done
    echo "Error: No such container: $1" >&2
    return 1
}
case "$1" in
    image) [ "$2" = inspect ]; exit 0 ;;
    volume) [ "$2" = ls ]; exit 0 ;;
    exec) exit 1 ;;
    run)
        shift
        name= workdir= generation= agent= fingerprint=
        while [ "$#" -gt 0 ]; do
            case "$1" in
                --name) name=$2; shift 2 ;;
                -w) workdir=$2; shift 2 ;;
                --label)
                    case "$2" in
                        com.agent-of-empires.sandbox-store-generation=*) generation=${2#*=} ;;
                        com.agent-of-empires.agent-tool=*) agent=${2#*=} ;;
                        com.agent-of-empires.mount-fingerprint=*) fingerprint=${2#*=} ;;
                    esac
                    shift 2 ;;
                *) shift ;;
            esac
        done
        [ -n "$name" ]
        if find_resource "$name" 2>/dev/null; then
            echo 'name already in use' >&2
            exit 1
        fi
        id=$(cat "$root/next-id")
        resource="$root/resources/$id"
        mkdir "$resource"
        printf '%s' "$name" > "$resource/name"
        printf '%s' "$workdir" > "$resource/workdir"
        printf '%s' "$generation" > "$resource/generation"
        printf '%s' "$agent" > "$resource/agent"
        printf '%s' "$fingerprint" > "$resource/fingerprint"
        printf '%s' true > "$resource/running"
        printf '%s\n' "$id"
        ;;
    container)
        [ "$2" = inspect ]
        for target do :; done
        find_resource "$target" || exit 1
        id=${resource##*/}
        if [ "$#" -eq 3 ]; then
            printf '[{"Id":"%s","State":{"Running":true},"Mounts":[]}]\n' "$id"
        else
            case "$4" in
                '{{.Id}}') printf '%s\n' "$id" ;;
                '{{.State.Running}}') cat "$resource/running" ;;
                '{{.Config.WorkingDir}}') cat "$resource/workdir" ;;
                *sandbox-store-generation*) cat "$resource/generation" ;;
                *agent-tool*) cat "$resource/agent" ;;
                *mount-fingerprint*) cat "$resource/fingerprint" ;;
                *) echo 'unsupported inspect format' >&2; exit 64 ;;
            esac
        fi
        ;;
    rm)
        for target do :; done
        find_resource "$target" || exit 1
        if [ -e "$root/deny-remove" ]; then
            echo 'fixture removal refused' >&2
            exit 1
        fi
        rm -r "$resource"
        ;;
    *) echo 'unsupported runtime command' >&2; exit 64 ;;
esac
"#
        .replace(
            "@ROOT@",
            &crate::session::environment::shell_escape(&root.path().to_string_lossy()),
        );
        let docker = bin.join("docker");
        std::fs::write(&docker, script).unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = crate::session::test_support::path_prepended(&bin);
        (root, path)
    }

    #[cfg(unix)]
    fn seed_running_resource(root: &std::path::Path, id: &str, name: &str) {
        let resource = root.join("resources").join(id);
        std::fs::create_dir(&resource).unwrap();
        let generation =
            crate::session::config::container_config::CURRENT_SANDBOX_STORE_GENERATION.to_string();
        for (field, value) in [
            ("name", name),
            ("workdir", "/workspace/project"),
            ("generation", generation.as_str()),
            ("agent", ""),
            ("fingerprint", ""),
            ("running", "true"),
        ] {
            std::fs::write(resource.join(field), value).unwrap();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    async fn reusing_a_container_replaces_a_stale_stored_id() {
        let _app = crate::session::test_support::isolate_app_dir();
        crate::session::config::update_config(|config| {
            config.sandbox.container_runtime = crate::session::ContainerRuntimeName::Docker;
        })
        .unwrap();
        let (runtime, _path) = construction_runtime();
        let project = runtime.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let mut instance = construction_instance(&project);
        let _hooks = ConstructionHookCleanup(instance.id.clone());
        let sandbox = instance.sandbox_info.as_mut().unwrap();
        sandbox.container_id = Some("replaced-long-ago".into());
        let name = sandbox.container_name.clone();
        seed_running_resource(runtime.path(), "live-id", &name);
        let (_state, store) = native(&instance).await;
        let (built, _container, created) = construct_for_test(store, instance).await;
        assert!(!created);
        assert_eq!(
            built.sandbox_info.unwrap().container_id.as_deref(),
            Some("live-id")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    async fn unpublished_sandbox_cleanup_preserves_resource_ownership() {
        for case in ["created", "reused", "replacement", "gone", "cleanup-fails"] {
            let _app = crate::session::test_support::isolate_app_dir();
            crate::session::config::update_config(|config| {
                config.sandbox.container_runtime = crate::session::ContainerRuntimeName::Docker;
            })
            .unwrap();
            let (runtime, _path) = construction_runtime();
            let project = runtime.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let instance = construction_instance(&project);
            let _hooks = ConstructionHookCleanup(instance.id.clone());
            let name = instance
                .sandbox_info
                .as_ref()
                .unwrap()
                .container_name
                .clone();
            seed_running_resource(runtime.path(), "unrelated", "another-session");
            if case == "reused" {
                seed_running_resource(runtime.path(), "built-original", &name);
            }
            let (state, store) = native(&instance).await;
            let (built, container, created) = construct_for_test(store.clone(), instance).await;
            assert_eq!(created, case != "reused", "{case}");
            let id = built
                .sandbox_info
                .as_ref()
                .unwrap()
                .container_id
                .as_deref()
                .unwrap();
            assert_eq!(id, "built-original", "{case}");
            let original = runtime.path().join("resources").join(id);
            assert!(
                original.is_dir(),
                "{case}: factory must create or reuse a live resource"
            );
            if matches!(case, "replacement" | "cleanup-fails") {
                std::fs::write(original.join("name"), format!("{name}-detached")).unwrap();
                seed_running_resource(runtime.path(), "same-name-replacement", &name);
            }
            if case == "cleanup-fails" {
                std::fs::write(runtime.path().join("deny-remove"), "").unwrap();
            }
            let superseded = store.clone();
            tokio::task::spawn_blocking(move || {
                superseded.commit(&mut |rows, _| {
                    if case == "gone" {
                        rows.clear();
                    } else {
                        rows[0].lifecycle_generation += 1;
                    }
                    Ok(())
                })
            })
            .await
            .unwrap()
            .unwrap();
            let disk_before = std::fs::read(store.storage().sessions_path()).unwrap();
            let canonical_before = serde_json::to_value(&*state.instances.read().await).unwrap();
            let error = tokio::task::spawn_blocking(move || {
                publish_built_sandbox(&store, &built, &container, created)
            })
            .await
            .unwrap()
            .unwrap_err();
            if case == "cleanup-fails" {
                assert!(format!("{error:#}").contains("built-original"));
            } else {
                assert!(
                    error.is::<crate::session::LifecycleReservationError>()
                        || error.is::<crate::session::SessionGone>(),
                    "{case}: {error:#}"
                );
            }
            assert_eq!(
                original.is_dir(),
                matches!(case, "reused" | "cleanup-fails"),
                "{case}"
            );
            assert!(
                runtime.path().join("resources/unrelated").is_dir(),
                "{case}"
            );
            if matches!(case, "replacement" | "cleanup-fails") {
                let replacement = runtime.path().join("resources/same-name-replacement");
                assert!(replacement.is_dir(), "{case}");
                assert_eq!(
                    std::fs::read_to_string(replacement.join("name")).unwrap(),
                    name
                );
            }
            let disk = crate::session::Storage::open_unwatched("default").unwrap();
            assert_eq!(
                std::fs::read(disk.sessions_path()).unwrap(),
                disk_before,
                "{case}"
            );
            assert_eq!(
                serde_json::to_value(&*state.instances.read().await).unwrap(),
                canonical_before,
                "{case}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    async fn committed_sandbox_survives_canonical_adoption_failure() {
        let _app = crate::session::test_support::isolate_app_dir();
        crate::session::config::update_config(|config| {
            config.sandbox.container_runtime = crate::session::ContainerRuntimeName::Docker;
        })
        .unwrap();
        let (runtime, _path) = construction_runtime();
        let project = runtime.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let instance = construction_instance(&project);
        let _hooks = ConstructionHookCleanup(instance.id.clone());
        let (state, store) = native(&instance).await;
        let (built, container, created) = construct_for_test(store.clone(), instance).await;
        assert!(created);
        let failing_state = state.clone();
        let error = tokio::task::spawn_blocking(move || {
            crate::server::session_store::before_next_adoption_for_test(move || {
                failing_state
                    .canonical_metadata
                    .blocking_write()
                    .profiles
                    .clear();
            });
            publish_built_sandbox(&store, &built, &container, created)
        })
        .await
        .unwrap()
        .unwrap_err();
        assert!(
            error.is::<crate::session::SessionCommitApplied>(),
            "{error:#}"
        );
        assert!(runtime.path().join("resources/built-original").is_dir());
        let disk = crate::server::test_support::load_instances_from_disk_for_test("default");
        assert_eq!(
            disk[0]
                .sandbox_info
                .as_ref()
                .unwrap()
                .container_id
                .as_deref(),
            Some("built-original")
        );
        assert!(state.instances.read().await[0]
            .sandbox_info
            .as_ref()
            .unwrap()
            .container_id
            .is_none());
        assert!(matches!(
            *state.canonical_health.read().await,
            crate::daemon::RuntimeHealth::Degraded { .. }
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    #[serial_test::serial(hook_base)]
    async fn unpublished_sandbox_does_not_cross_a_replaced_physical_profile() {
        let _app = crate::session::test_support::isolate_app_dir();
        crate::session::config::update_config(|config| {
            config.sandbox.container_runtime = crate::session::ContainerRuntimeName::Docker;
        })
        .unwrap();
        let (runtime, _path) = construction_runtime();
        let project = runtime.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let instance = construction_instance(&project);
        let _hooks = ConstructionHookCleanup(instance.id.clone());
        let (state, store) = native(&instance).await;
        let (built, container, created) = construct_for_test(store.clone(), instance).await;
        assert!(created);
        let profile = store
            .storage()
            .sessions_path()
            .parent()
            .unwrap()
            .to_path_buf();
        let detached = profile.with_file_name("detached-default");
        std::fs::rename(&profile, &detached).unwrap();
        let replacement = crate::session::Storage::new_unwatched("default").unwrap();
        replacement
            .update(|rows, _| {
                rows.push(Instance::new("replacement profile", "/tmp/replacement"));
                Ok(())
            })
            .unwrap();
        let replacement_before = std::fs::read(replacement.sessions_path()).unwrap();
        let detached_before = std::fs::read(detached.join("sessions.json")).unwrap();
        let error = tokio::task::spawn_blocking(move || {
            publish_built_sandbox(&store, &built, &container, created)
        })
        .await
        .unwrap()
        .unwrap_err();
        assert!(
            !error.is::<crate::session::SessionCommitApplied>(),
            "{error:#}"
        );
        assert!(runtime.path().join("resources/built-original").is_dir());
        assert_eq!(
            std::fs::read(replacement.sessions_path()).unwrap(),
            replacement_before
        );
        assert_eq!(
            std::fs::read(detached.join("sessions.json")).unwrap(),
            detached_before
        );
        assert!(state.instances.read().await[0]
            .sandbox_info
            .as_ref()
            .unwrap()
            .container_id
            .is_none());
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
