//! Generic request/response worker thread for the TUI.
//!
//! Long-running operations (docker stop/start, worktree removal, tmux
//! batch queries) must not run on the UI event loop. Each poller wraps a
//! `Worker`: requests go to a dedicated named thread, results come back
//! over a channel the main loop drains each frame.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;

type Handler<Req, Res> = Box<dyn FnMut(Req) -> Res + Send>;
type LoopState<Req, Res> = (mpsc::Receiver<Req>, mpsc::Sender<Res>, Handler<Req, Res>);

pub struct Worker<Req, Res> {
    name: String,
    request_tx: mpsc::Sender<Req>,
    result_rx: mpsc::Receiver<Res>,
    _handle: thread::JoinHandle<()>,
}

impl<Req: Send + 'static, Res: Send + 'static> Worker<Req, Res> {
    /// Spawn a named worker thread running a recv -> handle -> send loop.
    /// The loop exits when the `Worker` is dropped (the request channel
    /// disconnects) or when the result channel is gone.
    pub fn spawn(thread_name: &str, handler: impl FnMut(Req) -> Res + Send + 'static) -> Self {
        let (request_tx, request_rx) = mpsc::channel::<Req>();
        let (result_tx, result_rx) = mpsc::channel::<Res>();

        // The loop state is parked in a shared cell so that a failed named
        // spawn can hand the same channels to a bare `thread::spawn` instead
        // of panicking (`thread::Builder::spawn` consumes its closure even
        // on failure, so a plain retry cannot reuse it).
        let state: Arc<Mutex<Option<LoopState<Req, Res>>>> =
            Arc::new(Mutex::new(Some((request_rx, result_tx, Box::new(handler)))));

        let run = {
            let state = Arc::clone(&state);
            move || Self::run_loop(&state)
        };
        let handle = match thread::Builder::new()
            .name(thread_name.to_string())
            .spawn(run)
        {
            Ok(handle) => handle,
            Err(e) => {
                tracing::warn!(
                    target: "tui.worker",
                    worker = thread_name,
                    error = %e,
                    "named worker spawn failed; falling back to unnamed thread",
                );
                thread::spawn(move || Self::run_loop(&state))
            }
        };

        Self {
            name: thread_name.to_string(),
            request_tx,
            result_rx,
            _handle: handle,
        }
    }

    fn run_loop(state: &Mutex<Option<LoopState<Req, Res>>>) {
        let taken = state.lock().ok().and_then(|mut slot| slot.take());
        let Some((request_rx, result_tx, mut handler)) = taken else {
            return;
        };
        while let Ok(request) = request_rx.recv() {
            if result_tx.send(handler(request)).is_err() {
                break;
            }
        }
    }

    /// Enqueue a request (non-blocking). A send failure means the worker
    /// thread is gone (channel closed at teardown, or a panic in the
    /// handler). Log it rather than dropping silently so a stuck-looking
    /// in-flight row is traceable.
    pub fn request(&self, req: Req) {
        if let Err(e) = self.request_tx.send(req) {
            tracing::warn!(
                target: "tui.worker",
                worker = %self.name,
                error = %e,
                "request dropped; worker thread unavailable",
            );
        }
    }

    /// Non-blocking poll for a completed result. Surfaces `Disconnected`
    /// (returned forever once the worker thread is gone, e.g. after a panic
    /// in the handler) rather than collapsing it into `None`, so the caller
    /// can clear stuck in-flight state instead of leaving rows pinned on a
    /// transient status forever.
    pub fn try_recv(&self) -> Result<Res, mpsc::TryRecvError> {
        self.result_rx.try_recv()
    }

    /// Test-only worker with one pre-seeded result and no handler; requests
    /// are drained and ignored. Lets consumer tests exercise the
    /// result-application path without running real side effects.
    #[cfg(test)]
    pub(crate) fn seeded_for_test(thread_name: &str, result: Res) -> Self {
        let (request_tx, request_rx) = mpsc::channel::<Req>();
        let (result_tx, result_rx) = mpsc::channel::<Res>();
        result_tx.send(result).expect("seed worker result");

        let handle = thread::Builder::new()
            .name(thread_name.to_string())
            .spawn(move || {
                while request_rx.recv().is_ok() {}
                drop(result_tx);
            })
            .expect("failed to spawn test worker thread");

        Self {
            name: thread_name.to_string(),
            request_tx,
            result_rx,
            _handle: handle,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn recv_with_retries<Req: Send + 'static, Res: Send + 'static>(
        worker: &Worker<Req, Res>,
    ) -> Result<Res, mpsc::TryRecvError> {
        for _ in 0..100 {
            match worker.try_recv() {
                Err(mpsc::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                other => return other,
            }
        }
        Err(mpsc::TryRecvError::Empty)
    }

    #[test]
    fn worker_round_trips_requests_through_handler() {
        let worker: Worker<u32, u32> = Worker::spawn("aoe-test-worker", |n| n * 2);
        worker.request(21);
        let result = recv_with_retries(&worker).expect("timed out waiting for worker result");
        assert_eq!(result, 42);
    }

    #[test]
    fn worker_try_recv_returns_empty_when_idle() {
        let worker: Worker<u32, u32> = Worker::spawn("aoe-test-worker-idle", |n| n);
        assert!(matches!(worker.try_recv(), Err(mpsc::TryRecvError::Empty)));
    }

    #[test]
    fn worker_try_recv_surfaces_disconnected_after_handler_panic() {
        // A panicking handler kills the worker thread and drops result_tx;
        // consumers rely on seeing Disconnected (not Empty/None) to run
        // their stuck-row recovery.
        let worker: Worker<u32, u32> = Worker::spawn("aoe-test-worker-panic", |_| panic!("boom"));
        worker.request(1);
        let outcome = recv_with_retries(&worker);
        assert!(matches!(outcome, Err(mpsc::TryRecvError::Disconnected)));
    }

    #[test]
    fn worker_processes_requests_in_order() {
        let worker: Worker<u32, u32> = Worker::spawn("aoe-test-worker-order", |n| n + 1);
        worker.request(1);
        worker.request(2);
        assert_eq!(recv_with_retries(&worker).expect("first result"), 2);
        assert_eq!(recv_with_retries(&worker).expect("second result"), 3);
    }
}
