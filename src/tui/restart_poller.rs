//! Background restart handler for TUI responsiveness.
//!
//! Restarting a sandboxed session re-runs the start cascade (docker image pull,
//! container create/start, `before_start` host hook), which can block for
//! seconds; a stalled registry pull has no timeout of its own. Running that on
//! the UI event loop froze the TUI. This mirrors `StopPoller`:
//! requests go to a worker thread, results come back over a channel the main
//! loop polls each frame.

use std::sync::mpsc;
use std::thread;

use crate::session::restart::perform_restart;
pub use crate::session::restart::{RestartRequest, RestartResult};

pub struct RestartPoller {
    request_tx: mpsc::Sender<RestartRequest>,
    result_rx: mpsc::Receiver<RestartResult>,
    _handle: thread::JoinHandle<()>,
}

impl RestartPoller {
    pub fn new() -> Self {
        let (request_tx, request_rx) = mpsc::channel::<RestartRequest>();
        let (result_tx, result_rx) = mpsc::channel::<RestartResult>();

        let handle = thread::Builder::new()
            .name("aoe-restart-poller".to_string())
            .spawn(move || {
                Self::restart_loop(request_rx, result_tx);
            })
            .expect("failed to spawn restart poller thread");

        Self {
            request_tx,
            result_rx,
            _handle: handle,
        }
    }

    fn restart_loop(
        request_rx: mpsc::Receiver<RestartRequest>,
        result_tx: mpsc::Sender<RestartResult>,
    ) {
        while let Ok(request) = request_rx.recv() {
            let result = perform_restart(request);
            if result_tx.send(result).is_err() {
                break;
            }
        }
    }

    pub fn request_restart(&self, request: RestartRequest) {
        if let Err(e) = self.request_tx.send(request) {
            tracing::warn!(target: "tui.restart_poller", error = %e, "restart request dropped; worker thread unavailable");
        }
    }

    /// Non-blocking poll for a completed restart. Surfaces `Disconnected`
    /// (returned forever once the worker thread is gone, e.g. after a panic in
    /// `perform_restart`) rather than collapsing it into `None`, so the caller
    /// can clear stuck in-flight state instead of leaving rows pinned on
    /// `Status::Starting` forever.
    pub fn try_recv_result(&self) -> Result<RestartResult, mpsc::TryRecvError> {
        self.result_rx.try_recv()
    }

    #[cfg(test)]
    pub(crate) fn with_result_for_test(result: RestartResult) -> Self {
        let (request_tx, request_rx) = mpsc::channel::<RestartRequest>();
        let (result_tx, result_rx) = mpsc::channel::<RestartResult>();
        result_tx.send(result).expect("seed restart result");

        let handle = thread::Builder::new()
            .name("aoe-restart-poller-test".to_string())
            .spawn(move || {
                while request_rx.recv().is_ok() {}
                drop(result_tx);
            })
            .expect("failed to spawn test restart poller thread");

        Self {
            request_tx,
            result_rx,
            _handle: handle,
        }
    }
}

impl Default for RestartPoller {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Instance;
    use std::time::Duration;

    #[test]
    #[serial_test::serial]
    fn restart_poller_channel_communication() {
        let poller = RestartPoller::new();
        let instance = Instance::new("Test Session", "/tmp/test-project");
        let session_id = instance.id.clone();
        let title = instance.title.clone();

        poller.request_restart(RestartRequest {
            session_id: session_id.clone(),
            instance,
            size: None,
            wake_message: String::new(),
        });

        let mut result = None;
        for _ in 0..100 {
            if let Ok(r) = poller.try_recv_result() {
                result = Some(r);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // The cascade may create a real tmux session; tear it down regardless of
        // whether the assertions below pass.
        if let Ok(session) = crate::tmux::Session::new(&session_id, &title) {
            let _ = session.kill();
        }
        let result = result.expect("timed out waiting for restart result");
        assert_eq!(result.session_id, session_id);
        assert_eq!(result.instance.id, session_id);
    }

    #[test]
    fn restart_poller_try_recv_returns_empty_when_no_result() {
        let poller = RestartPoller::new();
        assert!(matches!(
            poller.try_recv_result(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }
}
