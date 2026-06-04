//! Background deletion handler for TUI responsiveness

use std::sync::mpsc;
use std::thread;

use crate::session::deletion::perform_deletion;
pub use crate::session::deletion::{DeletionRequest, DeletionResult};

pub struct DeletionPoller {
    request_tx: mpsc::Sender<DeletionRequest>,
    result_rx: mpsc::Receiver<DeletionResult>,
    _handle: thread::JoinHandle<()>,
}

impl DeletionPoller {
    pub fn new() -> Self {
        let (request_tx, request_rx) = mpsc::channel::<DeletionRequest>();
        let (result_tx, result_rx) = mpsc::channel::<DeletionResult>();

        let handle = thread::spawn(move || {
            Self::deletion_loop(request_rx, result_tx);
        });

        Self {
            request_tx,
            result_rx,
            _handle: handle,
        }
    }

    fn deletion_loop(
        request_rx: mpsc::Receiver<DeletionRequest>,
        result_tx: mpsc::Sender<DeletionResult>,
    ) {
        while let Ok(request) = request_rx.recv() {
            let result = perform_deletion(&request);
            if result_tx.send(result).is_err() {
                break;
            }
        }
    }

    pub fn request_deletion(&self, request: DeletionRequest) {
        let _ = self.request_tx.send(request);
    }

    pub fn try_recv_result(&self) -> Option<DeletionResult> {
        self.result_rx.try_recv().ok()
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
            result = poller.try_recv_result();
            if result.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(result.is_some(), "Timed out waiting for deletion result");

        let result = result.unwrap();
        assert_eq!(result.session_id, session_id);
        assert!(result.success);
    }

    #[test]
    fn test_deletion_poller_try_recv_returns_none_when_empty() {
        let poller = DeletionPoller::new();
        assert!(poller.try_recv_result().is_none());
    }
}
