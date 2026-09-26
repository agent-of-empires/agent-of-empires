//! Shared session restart logic.

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
    /// Kill a hook that outlives the recovery hook timeout, so a hung hook cannot wedge the worker.
    pub bound_hooks: bool,
    /// Remove the sandbox container before relaunching, so the next start creates a fresh one.
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
    /// Post-cascade instance snapshot.
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

    let title = instance.title.clone();
    let tool = instance.tool.clone();
    let before = instance.clone();

    // With `bound_hooks`, honor the on_launch / before_start hook timeout the startup-recovery
    // worker installs (`run_recovery_for_instance`), so a hanging hook (e.g. a `mint` script
    // waiting on the network) cannot wedge this serial worker.
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
        spawn_wake_worker(session_id.clone(), title, tool, wake_message);
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

/// Wait for the restarted pane to become live and past its boot shell, then send the wake-up
/// message.
fn spawn_wake_worker(session_id: String, title: String, tool: String, wake_message: String) {
    let spawn_result = std::thread::Builder::new()
        .name(format!("aoe-restart-wake/{}", session_id))
        .stack_size(128 * 1024)
        .spawn(move || {
            let Ok(tmux_session) = crate::tmux::Session::new(&session_id, &title) else {
                return;
            };
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(3000);
            loop {
                if !tmux_session.exists() {
                    return;
                }
                let pane_alive = !tmux_session.is_pane_dead();
                let hook_active = crate::hooks::read_hook_status(&session_id).is_some();
                if pane_alive && (hook_active || !tmux_session.is_pane_running_shell()) {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }

            if !tmux_session.exists() {
                return;
            }
            let delay = crate::agents::send_keys_enter_delay(&tool);
            if let Err(e) = tmux_session.send_keys_with_delay(&wake_message, delay) {
                tracing::warn!(target: "session.restart", "failed to send wake-up message after restart: {}", e);
            }
        });
    if let Err(err) = spawn_result {
        tracing::warn!(target: "session.restart", ?err, "failed to spawn restart wake-up worker");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_instance() -> Instance {
        Instance::new("Test Session", "/tmp/test-project")
    }

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
                enabled: true,
                container_id: None,
                image: "ubuntu:latest".to_string(),
                container_name: "test-container".to_string(),
                extra_env: None,
                custom_instruction: None,
                before_start_env: Vec::new(),
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
    fn restart_wake_is_suppressed_for_resume_failed() {
        let outcome = Ok(StartOutcome::ResumeFailed {
            sid: "11111111-2222-3333-4444-555555555555".to_string(),
        });

        assert!(!launched_agent(&outcome));
    }
}
