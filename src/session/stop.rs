//! Shared session stop logic.
//!
//! Stopping a session kills its tmux pane and, for sandboxed sessions, stops
//! (but does not remove) the Docker container so it can be restarted on
//! re-attach. `container.stop()` can block for up to the Docker stop grace
//! period (~10s), so the TUI runs this off the UI thread via `StopPoller`.

use crate::session::Instance;

pub struct StopRequest {
    pub session_id: String,
    pub instance: Instance,
}

#[derive(Debug)]
pub struct StopResult {
    pub session_id: String,
    pub success: bool,
    pub error: Option<String>,
}

pub fn perform_stop(request: &StopRequest) -> StopResult {
    match request.instance.stop() {
        Ok(()) => {
            crate::tmux::refresh_session_cache();
            StopResult {
                session_id: request.session_id.clone(),
                success: true,
                error: None,
            }
        }
        Err(e) => {
            tracing::error!(target: "session.stop", session_id = %request.session_id, error = %e, "perform_stop failed");
            StopResult {
                session_id: request.session_id.clone(),
                success: false,
                error: Some(e.to_string()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_instance() -> Instance {
        Instance::new("Test Session", "/tmp/test-project")
    }

    #[test]
    fn test_stop_result_success_for_session_without_tmux_or_sandbox() {
        let instance = create_test_instance();
        let request = StopRequest {
            session_id: instance.id.clone(),
            instance,
        };

        let result = perform_stop(&request);

        assert!(result.success);
        assert!(result.error.is_none());
        assert_eq!(result.session_id, request.session_id);
    }

    #[test]
    fn test_stop_result_preserves_session_id() {
        let instance = create_test_instance();
        let custom_id = "custom-session-id-123".to_string();
        let request = StopRequest {
            session_id: custom_id.clone(),
            instance,
        };

        let result = perform_stop(&request);
        assert_eq!(result.session_id, custom_id);
    }
}
