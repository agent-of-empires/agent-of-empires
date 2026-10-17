//! Background deletion handler for TUI responsiveness

use std::sync::mpsc::TryRecvError;

use crate::session::deletion::execute_deletion;
pub use crate::session::deletion::{DeletionRequest, DeletionResult};
use crate::tui::worker::Worker;

pub struct DeletionPoller {
    worker: Worker<(u64, DeletionRequest), (u64, DeletionResult)>,
}

impl DeletionPoller {
    pub fn new() -> Self {
        Self {
            worker: Worker::spawn("aoe-deletion-poller", |(attempt_id, request)| {
                (attempt_id, execute_deletion(request))
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn seeded_for_test(attempt_id: u64, result: DeletionResult) -> Self {
        Self {
            worker: Worker::seeded_for_test("aoe-deletion-test", (attempt_id, result)),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_handler_for_test(
        mut handler: impl FnMut(DeletionRequest) -> DeletionResult + Send + 'static,
    ) -> Self {
        Self {
            worker: Worker::spawn("aoe-deletion-test", move |(attempt_id, request)| {
                (attempt_id, handler(request))
            }),
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

    pub fn request_deletion(&self, attempt_id: u64, request: DeletionRequest) {
        self.worker.request((attempt_id, request));
    }

    /// Non-blocking poll for a completed deletion. Surfaces `Disconnected`
    /// (see `Worker::try_recv`) so the caller can recover rows stuck on
    /// `Status::Deleting` when the worker dies.
    pub fn try_recv_result(&self) -> Result<(u64, DeletionResult), TryRecvError> {
        self.worker.try_recv()
    }
}

impl Default for DeletionPoller {
    fn default() -> Self {
        Self::new()
    }
}
