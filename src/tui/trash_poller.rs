//! Background trash handler for TUI responsiveness.
//!
//! Trashing a sandboxed session stops its Docker container (`docker stop`,
//! which blocks for the container's grace period, ~10s) and then relocates its
//! worktree into the holding area. Running that on the UI event loop froze the
//! TUI (issue #2924 added the container stop inline; this moves it off-thread,
//! the same fix `StopPoller` applied for the stop path in #1496). Requests go
//! to a worker thread, results come back over a channel the main loop polls
//! each frame.

use std::collections::HashSet;
use std::sync::mpsc::TryRecvError;

use crate::session::trash::perform_trash;
pub use crate::session::trash::{TrashRequest, TrashResult};
use crate::tui::worker::Worker;

pub struct TrashPoller {
    worker: Worker<TrashRequest, TrashResult>,
    /// Session ids with a trash prepare in flight. The row is already durably
    /// trashed at request time, so its status alone cannot identify which
    /// relocations were still pending if the worker dies (Disconnected); this
    /// set can, so the drain can log them as deferred.
    pending: HashSet<String>,
}

impl TrashPoller {
    pub fn new() -> Self {
        Self {
            worker: Worker::spawn("aoe-trash-poller", |request| perform_trash(&request)),
            pending: HashSet::new(),
        }
    }

    pub fn request_trash(&mut self, request: TrashRequest) {
        self.pending.insert(request.session_id.clone());
        self.worker.request(request);
    }

    /// Non-blocking poll for a completed trash prepare. Surfaces `Disconnected`
    /// (see `Worker::try_recv`) so the caller can note the relocations still in
    /// [`Self::take_pending`] as deferred rather than silently lost.
    pub fn try_recv_result(&mut self) -> Result<TrashResult, TryRecvError> {
        let result = self.worker.try_recv();
        if let Ok(ref trash) = result {
            self.pending.remove(&trash.session_id);
        }
        result
    }

    /// Drain the in-flight set. Called once the worker is known dead so the
    /// consumer can log which relocations never landed (the rows stay trashed;
    /// a later reconcile pass moves their worktrees).
    pub fn take_pending(&mut self) -> Vec<String> {
        self.pending.drain().collect()
    }
}

impl Default for TrashPoller {
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
    fn test_trash_poller_channel_communication() {
        let mut poller = TrashPoller::new();
        let instance = create_test_instance();
        let session_id = instance.id.clone();

        poller.request_trash(TrashRequest {
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
        let result = result.expect("Timed out waiting for trash result");

        assert_eq!(result.session_id, session_id);
        // A plain (non-worktree, non-sandbox) session has nothing to relocate.
        assert!(result.relocation.is_none());
        assert!(result.relocate_warning.is_none());
        // The delivered result must clear the in-flight marker.
        assert!(poller.take_pending().is_empty());
    }

    #[test]
    fn test_trash_poller_try_recv_returns_empty_when_idle() {
        let mut poller = TrashPoller::new();
        assert!(matches!(poller.try_recv_result(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn test_trash_poller_tracks_pending_requests() {
        let mut poller = TrashPoller::new();
        let instance = create_test_instance();
        let session_id = instance.id.clone();

        poller.request_trash(TrashRequest {
            session_id: session_id.clone(),
            instance,
        });

        assert_eq!(poller.take_pending(), vec![session_id]);
        assert!(poller.take_pending().is_empty(), "take_pending drains");
    }
}
