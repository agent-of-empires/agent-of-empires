//! Background stop handler for TUI responsiveness.
//!
//! Stopping a sandboxed session calls `docker stop`, which can block for up to
//! the container's stop grace period (~10s). Running that on the UI event loop
//! froze the TUI (issue #1496). This mirrors `DeletionPoller`: requests go to a
//! worker thread, results come back over a channel the main loop polls each
//! frame.

use std::collections::HashSet;
use std::sync::mpsc::TryRecvError;

use crate::session::stop::perform_stop;
pub use crate::session::stop::{StopRequest, StopResult};
use crate::tui::worker::Worker;

pub struct StopPoller {
    worker: Worker<StopRequest, StopResult>,
    /// Session ids with a stop in flight. Rows are optimistically marked
    /// `Stopped` at request time, so if the worker dies (Disconnected) the
    /// status alone cannot identify which stops were lost; this set can.
    pending: HashSet<String>,
}

impl StopPoller {
    pub fn new() -> Self {
        Self {
            worker: Worker::spawn("aoe-stop-poller", |request| perform_stop(&request)),
            pending: HashSet::new(),
        }
    }

    pub fn request_stop(&mut self, request: StopRequest) {
        self.pending.insert(request.session_id.clone());
        self.worker.request(request);
    }

    /// Non-blocking poll for a completed stop. Surfaces `Disconnected` (see
    /// `Worker::try_recv`) so the caller can recover the sessions still in
    /// [`Self::take_pending`] instead of leaving them looking stopped while
    /// their container may still be running.
    pub fn try_recv_result(&mut self) -> Result<StopResult, TryRecvError> {
        let result = self.worker.try_recv();
        if let Ok(ref stop) = result {
            self.pending.remove(&stop.session_id);
        }
        result
    }

    /// Drain the in-flight set. Called once the worker is known dead so the
    /// consumer can transition the affected rows out of their optimistic
    /// `Stopped` state.
    pub fn take_pending(&mut self) -> Vec<String> {
        self.pending.drain().collect()
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
        let mut poller = StopPoller::new();
        let instance = create_test_instance();
        let session_id = instance.id.clone();

        poller.request_stop(StopRequest {
            session_id: session_id.clone(),
            instance,
        });

        let mut result = None;
        for _ in 0..50 {
            if let Ok(r) = poller.try_recv_result() {
                result = Some(r);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let result = result.expect("Timed out waiting for stop result");

        assert_eq!(result.session_id, session_id);
        assert!(result.success);
        // The delivered result must clear the in-flight marker.
        assert!(poller.take_pending().is_empty());
    }

    #[test]
    fn test_stop_poller_try_recv_returns_empty_when_idle() {
        let mut poller = StopPoller::new();
        assert!(matches!(poller.try_recv_result(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn test_stop_poller_tracks_pending_requests() {
        let mut poller = StopPoller::new();
        let instance = create_test_instance();
        let session_id = instance.id.clone();

        poller.request_stop(StopRequest {
            session_id: session_id.clone(),
            instance,
        });

        assert_eq!(poller.take_pending(), vec![session_id]);
        assert!(poller.take_pending().is_empty(), "take_pending drains");
    }
}
