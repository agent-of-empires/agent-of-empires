//! Shared session restart logic.
//!
//! Restarting a session re-runs the start cascade. For sandboxed sessions that
//! shells out to Docker (image pull with no built-in timeout, container
//! create/start) and runs the `before_start` host hook, any of which can block
//! for seconds. The daemon runs it off the caller's thread; the TUI submits
//! it as a runtime mutation.

use crate::session::{Instance, StartOutcome};

pub struct RestartRequest {
    pub session_id: String,
    /// The instance to restart. `perform_restart` mutates it through the start
    /// cascade and hands the post-cascade snapshot back in `RestartResult`.
    pub instance: Instance,
    pub size: Option<(u16, u16)>,
    /// Skip on_launch hooks that already ran in the background creation poller.
    pub skip_on_launch: bool,
    /// Kill a hook that outlives the recovery hook timeout, so a hung hook
    /// cannot wedge the worker. Unset for the launch behind Enter or a new
    /// session, whose hooks have always run unbounded.
    pub bound_hooks: bool,
    /// Remove the sandbox container before relaunching, so the next start
    /// creates a fresh one. Set on a tool swap: launch recreates a container
    /// labelled for another tool, but not one created before that label (#3959).
    pub discard_sandbox_container: bool,
    /// Copy the conversation into the incoming account's agent config root.
    /// Set on a swap that changes only the account (#4030); planned against the
    /// pre-swap row and run inside the cascade, once the outgoing agent is dead
    /// and before the incoming one starts.
    pub conversation_carry: Option<crate::session::conversation_carry::ConversationCarry>,
}

pub struct RestartResult {
    pub session_id: String,
    /// Pre-cascade snapshot used as a compare-and-swap baseline when merging
    /// peer-writable identity fields back into a live row.
    pub before: Box<Instance>,
    /// Post-cascade instance snapshot. Written back into the TUI's in-memory
    /// copy so `#[serde(skip)]` fields (e.g. `last_start_time`) and the
    /// cascade's mutations (cleared stale `agent_session_id`, container id)
    /// survive without a disk reload.
    pub instance: Box<Instance>,
    pub outcome: Result<StartOutcome, String>,
}

pub fn perform_restart(request: RestartRequest) -> RestartResult {
    let RestartRequest {
        session_id,
        mut instance,
        size,
        skip_on_launch,
        bound_hooks,
        discard_sandbox_container,
        conversation_carry,
    } = request;

    let before = instance.clone();

    // Recovery bounds hooks so a hanging command cannot wedge its worker.
    let outcome = {
        let _scope = bound_hooks.then(|| {
            crate::session::recovery::HookTimeoutScope::new(
                crate::session::recovery::recovery_hook_timeout(),
            )
        });
        instance
            .restart_discarding_sandbox_container(
                size,
                skip_on_launch,
                discard_sandbox_container,
                conversation_carry,
            )
            .map_err(|e| e.to_string())
    };

    RestartResult {
        session_id,
        before: Box::new(before),
        instance: Box::new(instance),
        outcome,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_instance() -> Instance {
        Instance::new("Test Session", "/tmp/test-project")
    }

    /// A tool-swap restart must not remove a container whose session a peer
    /// lifecycle operation still owns (#3972).
    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn tool_swap_restart_removes_container_only_after_owning_launch_reservation() {
        use crate::session::{LifecycleOperation, LifecycleReservation, SandboxInfo};
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let bin = temp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let calls = temp.path().join("runtime-calls");
        // Record every runtime call; only removal succeeds, so an owned
        // relaunch stops at container creation instead of reaching tmux.
        for binary in ["docker", "podman", "container"] {
            let script = bin.join(binary);
            std::fs::write(
                &script,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n\
                     if [ \"$1\" = rm ]; then exit 0; fi\n\
                     echo 'permission denied' >&2\nexit 1\n",
                    calls.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let _path = crate::session::test_support::path_prepended(&bin);

        let profile = "restart-discard-reservation";
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        for (peer_reserved, expected_removals) in [(true, 0), (false, 1)] {
            let mut instance = test_instance();
            instance.source_profile = profile.to_string();
            instance.tool = "codex".to_string();
            instance.sandbox_info = Some(SandboxInfo {
                provider: None,
                enabled: true,
                container_id: None,
                image: "ubuntu:latest".to_string(),
                container_name: "test-container".to_string(),
                extra_env: None,
                custom_instruction: None,
                before_start_env: Vec::new(),
                before_start_hook_env: Vec::new(),
                container_workdir: None,
            });
            if peer_reserved {
                instance.lifecycle_generation = 1;
                instance.lifecycle_reservation = Some(LifecycleReservation {
                    op: LifecycleOperation::Launch,
                    generation: 1,
                    at: crate::time::Timestamp::now(),
                });
            }
            storage
                .update(|instances, _groups| {
                    instances.push(instance.clone());
                    Ok(())
                })
                .unwrap();
            let container = crate::containers::DockerContainer::from_session_id(&instance.id).name;

            let result = perform_restart(RestartRequest {
                session_id: instance.id.clone(),
                instance,
                size: None,
                skip_on_launch: false,
                bound_hooks: true,
                discard_sandbox_container: true,
                conversation_carry: None,
            });

            let error = result
                .outcome
                .expect_err("the fake runtime fails every launch");
            assert_eq!(error.contains("busy"), peer_reserved, "{error}");
            let removals = std::fs::read_to_string(&calls)
                .unwrap_or_default()
                .lines()
                .filter(|line| line.starts_with("rm ") && line.ends_with(&container))
                .count();
            assert_eq!(
                removals, expected_removals,
                "peer_reserved={peer_reserved}: {error}"
            );
        }
    }
}
