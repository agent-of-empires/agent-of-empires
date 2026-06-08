//! Background session revive for TUI responsiveness.
//!
//! Restoring an archived session respawns its agent through the restart cascade
//! (`run_recovery_for_instance` -> `restart_with_size_opts`), which probes the
//! agent for up to ~2s to detect a crashed resume. Running that on the UI event
//! loop froze the TUI while a restored session loaded. This mirrors
//! `StopPoller`: a request carries a clone of the instance to a worker thread,
//! the cascade runs there, and the updated instance comes back over a channel
//! the main loop drains each frame (`HomeView::apply_revive_results`).

use std::sync::mpsc;
use std::thread;

use crate::session::{Instance, StartOutcome};

pub struct ReviveRequest {
    pub instance: Instance,
}

pub struct ReviveResult {
    pub session_id: String,
    /// The post-cascade instance, boxed to keep the channel message small.
    pub instance: Box<Instance>,
    pub outcome: Result<StartOutcome, String>,
}

pub struct RevivePoller {
    request_tx: mpsc::Sender<ReviveRequest>,
    result_rx: mpsc::Receiver<ReviveResult>,
    _handle: thread::JoinHandle<()>,
}

impl RevivePoller {
    pub fn new() -> Self {
        let (request_tx, request_rx) = mpsc::channel::<ReviveRequest>();
        let (result_tx, result_rx) = mpsc::channel::<ReviveResult>();

        let handle = thread::spawn(move || {
            Self::revive_loop(request_rx, result_tx);
        });

        Self {
            request_tx,
            result_rx,
            _handle: handle,
        }
    }

    fn revive_loop(
        request_rx: mpsc::Receiver<ReviveRequest>,
        result_tx: mpsc::Sender<ReviveResult>,
    ) {
        while let Ok(request) = request_rx.recv() {
            let mut working = request.instance;
            let session_id = working.id.clone();
            let outcome = crate::session::recovery::run_recovery_for_instance(&mut working)
                .map_err(|e| e.to_string());
            let result = ReviveResult {
                session_id,
                instance: Box::new(working),
                outcome,
            };
            if result_tx.send(result).is_err() {
                break;
            }
        }
    }

    pub fn request(&self, instance: Instance) {
        if let Err(e) = self.request_tx.send(ReviveRequest { instance }) {
            // The cascade is panic-safe, so a send failure means the worker
            // thread is gone (channel closed at teardown). Log it rather than
            // dropping silently so a stuck-looking "Starting" row is traceable.
            tracing::warn!(
                target: "tui.revive_poller",
                error = %e,
                "revive request dropped; worker thread unavailable",
            );
        }
    }

    pub fn try_recv_result(&self) -> Option<ReviveResult> {
        self.result_rx.try_recv().ok()
    }
}

impl Default for RevivePoller {
    fn default() -> Self {
        Self::new()
    }
}
