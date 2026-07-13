//! Background deletion handler for TUI responsiveness

use std::sync::mpsc::TryRecvError;

use crate::session::deletion::perform_deletion;
pub use crate::session::deletion::{DeletionRequest, DeletionResult};
use crate::tui::worker::Worker;

pub struct DeletionPoller {
    worker: Worker<DeletionRequest, DeletionResult>,
}

impl DeletionPoller {
    pub fn new() -> Self {
        Self {
            worker: Worker::spawn("aoe-deletion-poller", |request| perform_deletion(&request)),
        }
    }

    pub fn request_deletion(&self, request: DeletionRequest) {
        self.worker.request(request);
    }

    /// Non-blocking poll for a completed deletion. Surfaces `Disconnected`
    /// (see `Worker::try_recv`) so the caller can recover rows stuck on
    /// `Status::Deleting` when the worker dies.
    pub fn try_recv_result(&self) -> Result<DeletionResult, TryRecvError> {
        self.worker.try_recv()
    }
}

impl Default for DeletionPoller {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Instance;
    use std::time::Duration;

    fn create_test_instance() -> Instance {
        Instance::new("Test Session", "/tmp/test-project")
    }

    #[test]
    fn test_deletion_poller_channel_communication() {
        let poller = DeletionPoller::new();
        let instance = create_test_instance();
        let session_id = instance.id.clone();

        poller.request_deletion(DeletionRequest {
            session_id: session_id.clone(),
            instance,
            delete_worktree: false,
            delete_branch: false,
            delete_sandbox: false,
            force_delete: false,
            detach_hooks: true,
            keep_scratch: false,
        });

        let mut result = None;
        for _ in 0..50 {
            if let Ok(r) = poller.try_recv_result() {
                result = Some(r);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let result = result.expect("Timed out waiting for deletion result");

        assert_eq!(result.session_id, session_id);
        assert!(result.success);
    }

    #[test]
    fn test_deletion_poller_try_recv_returns_empty_when_idle() {
        let poller = DeletionPoller::new();
        assert!(matches!(poller.try_recv_result(), Err(TryRecvError::Empty)));
    }
}
