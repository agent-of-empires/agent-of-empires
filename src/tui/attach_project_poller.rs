//! Background attach-a-project handler for TUI responsiveness (#3103).
//!
//! An attach runs `git worktree add` (plus an optional fetch and submodule init),
//! persists, bounces the ACP worker and removes the sandbox container. On the UI
//! event loop that froze the TUI for the whole operation. This mirrors
//! `RestartPoller`: requests go to a worker thread, results come back over a
//! channel the main loop polls each frame.

use std::sync::mpsc::TryRecvError;

use crate::session::attach_project::perform_attach_project;
pub use crate::session::attach_project::{AttachProjectRequest, AttachProjectResult};
use crate::tui::worker::Worker;

pub struct AttachProjectPoller {
    worker: Worker<AttachProjectRequest, AttachProjectResult>,
}

impl AttachProjectPoller {
    pub fn new() -> Self {
        Self {
            worker: Worker::spawn("aoe-attach-project-poller", perform_attach_project),
        }
    }

    pub fn request_attach(&self, request: AttachProjectRequest) {
        self.worker.request(request);
    }

    /// Non-blocking poll for a finished attach. Surfaces `Disconnected` rather
    /// than collapsing it into `None`, so a panic in `perform_attach_project`
    /// clears the caller's in-flight marker instead of leaving the session
    /// pinned as attaching forever.
    pub fn try_recv_result(&self) -> Result<AttachProjectResult, TryRecvError> {
        self.worker.try_recv()
    }

    #[cfg(test)]
    pub(crate) fn with_result_for_test(result: AttachProjectResult) -> Self {
        Self {
            worker: Worker::seeded_for_test("aoe-attach-project-poller-test", result),
        }
    }
}

impl Default for AttachProjectPoller {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_poller_reports_empty_rather_than_blocking() {
        let poller = AttachProjectPoller::new();
        assert!(matches!(poller.try_recv_result(), Err(TryRecvError::Empty)));
    }

    /// The seeded constructor is what lets the home-view tests drive the
    /// completion path without a real worktree on disk.
    #[test]
    fn seeded_poller_hands_back_its_result_once() {
        let poller = AttachProjectPoller::with_result_for_test(AttachProjectResult {
            session_id: "s-1".to_string(),
            outcome: Ok("Attached 'frontend' on branch 'feature/abc'.".to_string()),
        });
        let got = poller
            .try_recv_result()
            .expect("the seeded result must be delivered");
        assert_eq!(got.session_id, "s-1");
        assert!(got.outcome.is_ok());
        assert!(matches!(poller.try_recv_result(), Err(TryRecvError::Empty)));
    }
}
