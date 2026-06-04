//! Background stop handler for TUI responsiveness.
//!
//! Stopping a sandboxed session calls `docker stop`, which can block for up to
//! the container's stop grace period (~10s). Running that on the UI event loop
//! froze the TUI (issue #1496). This mirrors `DeletionPoller`: requests go to a
//! worker thread, results come back over a channel the main loop polls each
//! frame.

use std::sync::mpsc;
use std::thread;

use crate::session::stop::perform_stop;
pub use crate::session::stop::{StopRequest, StopResult};

pub struct StopPoller {
    request_tx: mpsc::Sender<StopRequest>,
    result_rx: mpsc::Receiver<StopResult>,
    _handle: thread::JoinHandle<()>,
}

impl StopPoller {
    pub fn new() -> Self {
        let (request_tx, request_rx) = mpsc::channel::<StopRequest>();
        let (result_tx, result_rx) = mpsc::channel::<StopResult>();

        let handle = thread::spawn(move || {
            Self::stop_loop(request_rx, result_tx);
        });

        Self {
            request_tx,
            result_rx,
            _handle: handle,
        }
    }

    fn stop_loop(request_rx: mpsc::Receiver<StopRequest>, result_tx: mpsc::Sender<StopResult>) {
        while let Ok(request) = request_rx.recv() {
            let result = perform_stop(&request);
            if result_tx.send(result).is_err() {
                break;
            }
        }
    }

    pub fn request_stop(&self, request: StopRequest) {
        if let Err(e) = self.request_tx.send(request) {
            // `perform_stop` is panic-safe, so a send failure means the worker
            // thread is gone (channel closed at teardown). Log it rather than
            // dropping silently so a stuck-looking "Stopping" row is traceable.
            tracing::warn!(target: "tui.stop_poller", error = %e, "stop request dropped; worker thread unavailable");
        }
    }

    pub fn try_recv_result(&self) -> Option<StopResult> {
        self.result_rx.try_recv().ok()
    }
}

impl Default for StopPoller {
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
    fn test_stop_poller_channel_communication() {
        let poller = StopPoller::new();
        let instance = create_test_instance();
        let session_id = instance.id.clone();

        poller.request_stop(StopRequest {
            session_id: session_id.clone(),
            instance,
        });

        let mut result = None;
        for _ in 0..50 {
            result = poller.try_recv_result();
            if result.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(result.is_some(), "Timed out waiting for stop result");

        let result = result.unwrap();
        assert_eq!(result.session_id, session_id);
        assert!(result.success);
    }

    #[test]
    fn test_stop_poller_try_recv_returns_none_when_empty() {
        let poller = StopPoller::new();
        assert!(poller.try_recv_result().is_none());
    }
}
