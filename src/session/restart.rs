//! Shared session restart logic.
//!
//! Restarting a session re-runs the start cascade. For sandboxed sessions that
//! shells out to Docker (image pull with no built-in timeout, container
//! create/start) and runs the `before_start` host hook, any of which can block
//! for seconds. Running it on the TUI event loop froze the whole UI, so the TUI
//! drives this off the UI thread via `RestartPoller`, mirroring `StopPoller`.

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
}

pub struct RestartResult {
    pub session_id: String,
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
    } = request;

    let title = instance.title.clone();
    let tool = instance.tool.clone();

    // Honor the same on_launch / before_start hook timeout the startup-recovery
    // worker installs (`run_recovery_for_instance`). Without it, a hanging
    // before_start hook (e.g. a `mint` script waiting on the network) runs with
    // no kill timer and wedges this serial worker thread forever, taking every
    // future restart down with it.
    let outcome = {
        let _scope = crate::session::recovery::HookTimeoutScope::new(
            crate::session::recovery::recovery_hook_timeout(),
        );
        instance.restart_with_size(size).map_err(|e| e.to_string())
    };

    // On a successful restart, send the wake-up keys on a detached thread so
    // the result (and the row's status update) propagate back immediately
    // rather than waiting out the up-to-3s pane-readiness probe.
    if outcome.is_ok() && !wake_message.is_empty() {
        spawn_wake_worker(session_id.clone(), title, tool, wake_message);
    }

    RestartResult {
        session_id,
        instance: Box::new(instance),
        outcome,
    }
}

/// Wait for the restarted pane to become live and past its boot shell, then
/// send the wake-up message. Best-effort: a failure to spawn or send is logged,
/// never fatal.
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

    #[test]
    #[serial_test::serial]
    fn perform_restart_preserves_session_id_and_returns_instance() {
        let instance = test_instance();
        let id = instance.id.clone();
        let title = instance.title.clone();
        let result = perform_restart(RestartRequest {
            session_id: id.clone(),
            instance,
            size: None,
            wake_message: String::new(),
        });
        // The cascade may create a real tmux session; tear it down so the test
        // cleans up after itself.
        if let Ok(session) = crate::tmux::Session::new(&id, &title) {
            let _ = session.kill();
        }
        assert_eq!(result.session_id, id);
        assert_eq!(result.instance.id, id);
    }
}
