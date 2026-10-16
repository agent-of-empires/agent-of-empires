//! Background deletion handler for TUI responsiveness

use std::sync::mpsc::TryRecvError;

use crate::session::deletion::execute_deletion;
pub use crate::session::deletion::{DeletionRequest, DeletionResult};
use crate::tui::worker::Worker;

pub struct DeletionPoller {
    worker: Worker<DeletionRequest, DeletionResult>,
}

impl DeletionPoller {
    pub fn new() -> Self {
        Self {
            worker: Worker::spawn("aoe-deletion-poller", execute_deletion),
        }
    }

    #[cfg(test)]
    pub(crate) fn seeded_for_test(result: DeletionResult) -> Self {
        Self {
            worker: Worker::seeded_for_test("aoe-deletion-test", result),
        }
    }

    #[cfg(test)]
    pub(crate) fn crashing_for_test() -> Self {
        Self {
            worker: Worker::spawn("aoe-deletion-crash-test", |_| {
                panic!("simulated deletion worker failure")
            }),
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
