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
    /// Keys to send once the pane is live again. Empty disables the wake-up
    /// (the documented opt-out via `session.restart_wake_message`).
    pub wake_message: String,
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
        wake_message,
        skip_on_launch,
        bound_hooks,
        discard_sandbox_container,
        conversation_carry,
    } = request;

    let before = instance.clone();
    let wake_source = (!wake_message.is_empty())
        .then(|| super::Storage::open_reference(&instance.effective_profile()).ok())
        .flatten();

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

    // On a successful restart, send the wake-up keys on a detached thread so the result (and the
    // row's status update) propagate back immediately rather than waiting out the up-to-3s
    // pane-readiness probe.
    let should_wake = launched_agent(&outcome);
    if should_wake && !wake_message.is_empty() {
        let profile_identity = wake_source
            .as_ref()
            .filter(|source| source.verify_bound_profile().is_ok())
            .and_then(|source| source.bound_profile_identity().ok());
        spawn_wake_worker(&instance, wake_message, profile_identity);
    }

    RestartResult {
        session_id,
        before: Box::new(before),
        instance: Box::new(instance),
        outcome,
    }
}

/// Whether the restart left the agent running in a live pane.
pub(crate) fn launched_agent(outcome: &Result<StartOutcome, String>) -> bool {
    matches!(
        outcome,
        Ok(StartOutcome::Fresh
            | StartOutcome::Resumed
            | StartOutcome::FreshAfterFailedResume { .. })
    )
}

#[cfg(test)]
thread_local! {
    static WAKE_DISPATCHES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn wake_dispatches_for_test() -> u64 {
    WAKE_DISPATCHES.with(std::cell::Cell::get)
}

/// Send a restart wake through the captured launch pane, never a later name owner.
/// Failure is logged without replaying the message or replacing a runtime.
pub(crate) fn spawn_wake_worker(
    instance: &Instance,
    wake_message: String,
    profile_identity: Option<(u64, u64)>,
) {
    #[cfg(test)]
    WAKE_DISPATCHES.with(|count| count.set(count.get() + 1));
    let (Some(name), Some(physical)) = (
        instance.agent_pane.tmux_session.as_ref(),
        instance.agent_pane.physical.clone(),
    ) else {
        tracing::warn!(target: "session.restart", session = %instance.id,
            "restart wake refused: physical launch receipt unavailable");
        return;
    };
    let captured = (|| -> anyhow::Result<_> {
        let expected = profile_identity.ok_or(crate::session::NativeInputError::Ownership)?;
        let fence = crate::session::NativeInputFence::capture_route(
            &instance.effective_profile(),
            &instance.id,
            instance.created_at,
            instance.lifecycle_generation,
            true,
            Some(expected),
        )?;
        Ok(crate::tmux::Session::from_observation(name, physical).with_native_input_fence(fence))
    })();
    let mut session = match captured {
        Ok(session) => session,
        Err(error) => {
            tracing::warn!(target: "session.restart", session = %instance.id, %error,
                "restart wake refused before enqueue; no retry performed");
            return;
        }
    };
    let session_id = instance.id.clone();
    let tool = instance.tool.clone();
    let expects_shell = instance.expects_shell();
    let spawn_result = std::thread::Builder::new()
        .name(format!("aoe-restart-wake/{session_id}"))
        .spawn(move || {
            session.wait_until_ready(
                std::time::Duration::from_secs(5),
                crate::agents::ready_marker(&tool),
            );
            let delivered = session.admit_agent_input(expects_shell).and_then(|()| {
                session.send_keys_with_delay(
                    &wake_message,
                    crate::agents::send_keys_enter_delay(&tool),
                )
            });
            if let Err(error) = delivered {
                tracing::warn!(target: "session.restart", %error,
                    "restart wake was not confirmed; no retry performed");
            }
        });
    if let Err(error) = spawn_result {
        tracing::warn!(target: "session.restart", ?error,
            "failed to spawn restart wake-up worker");
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
                    at: chrono::Utc::now(),
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
                wake_message: String::new(),
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

    #[test]
    fn restart_wake_requires_a_successful_launch() {
        let sid = "11111111-2222-3333-4444-555555555555".to_string();
        assert!(!launched_agent(&Ok(StartOutcome::ResumeFailed {
            sid: sid.clone(),
        })));
        assert!(!launched_agent(&Ok(StartOutcome::Indeterminate {
            cause: crate::session::LaunchIndeterminateCause::CommandTimedOut,
        })));
        assert!(launched_agent(&Ok(StartOutcome::FreshAfterFailedResume {
            sid,
        })));
        assert!(!launched_agent(&Err("launch refused".into())));
    }
}
