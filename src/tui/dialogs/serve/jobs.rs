//! Bounded execution owned by Home, independent of a dialog's lifetime.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::TryRecvError;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::{Job, JobResult};

const PENDING: u8 = 0;
const ADMITTED: u8 = 1;
const OBSERVING: u8 = 2;
const CANCEL_REQUESTED: u8 = 1 << 2;

pub(super) struct Control {
    admission: AtomicU8,
    pub(super) cancel: CancellationToken,
}

impl Control {
    fn new() -> Self {
        Self {
            admission: AtomicU8::new(PENDING),
            cancel: CancellationToken::new(),
        }
    }

    pub(super) fn admit(&self) -> bool {
        self.admission
            .compare_exchange(PENDING, ADMITTED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn cancel(&self) -> bool {
        let previous = self.admission.fetch_or(CANCEL_REQUESTED, Ordering::AcqRel);
        let phase = previous & !CANCEL_REQUESTED;
        if phase != ADMITTED {
            self.cancel.cancel();
        }
        phase == PENDING
    }

    pub(super) fn finish_mutation(&self) {
        // Consume deferred cancellation when mutation ownership ends.
        if self.admission.swap(OBSERVING, Ordering::AcqRel) & CANCEL_REQUESTED != 0 {
            self.cancel.cancel();
        }
    }
}

struct Request {
    attempt: u64,
    job: Job,
    control: Arc<Control>,
    result: oneshot::Sender<(u64, JobResult)>,
}

#[derive(Default)]
struct Mailbox {
    queued: Option<Request>,
    active: Option<Arc<Control>>,
    closed: bool,
}

#[derive(Default)]
struct Shared {
    mailbox: Mutex<Mailbox>,
    ready: Condvar,
}

impl Shared {
    fn close(&self) {
        let mut mailbox = self.mailbox.lock().unwrap_or_else(|e| e.into_inner());
        mailbox.closed = true;
        if let Some(request) = mailbox.queued.take() {
            request.control.cancel();
        }
        if let Some(control) = &mailbox.active {
            control.cancel();
        }
        self.ready.notify_all();
    }
}

struct ExitGuard(Arc<Shared>);

impl Drop for ExitGuard {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// Dropping a ticket never waits for execution. Admitted mutations keep their owner.
pub(super) struct Ticket {
    control: Arc<Control>,
    result: oneshot::Receiver<(u64, JobResult)>,
}

impl Ticket {
    pub(super) fn try_recv(&mut self) -> Result<(u64, JobResult), TryRecvError> {
        self.result.try_recv().map_err(|error| match error {
            oneshot::error::TryRecvError::Empty => TryRecvError::Empty,
            oneshot::error::TryRecvError::Closed => TryRecvError::Disconnected,
        })
    }

    pub(super) fn cancel_before_admission(&self) -> bool {
        self.control.cancel()
    }

    pub(super) async fn finish(mut self, expected: u64) -> anyhow::Result<JobResult> {
        let (attempt, result) = (&mut self.result).await?;
        anyhow::ensure!(attempt == expected, "Serve completion attempt changed");
        Ok(result)
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.control.cancel();
    }
}

#[derive(Clone)]
pub(super) struct Submitter(Arc<Shared>);

impl Submitter {
    #[cfg(test)]
    pub(super) fn closed_for_test() -> Self {
        let shared = Arc::new(Shared::default());
        shared.close();
        Self(shared)
    }

    pub(super) fn request(&self, attempt: u64, job: Job) -> Ticket {
        let (result, receiver) = oneshot::channel();
        let control = Arc::new(Control::new());
        let mut mailbox = self.0.mailbox.lock().unwrap_or_else(|e| e.into_inner());
        if !mailbox.closed {
            if let Some(previous) = mailbox.queued.replace(Request {
                attempt,
                job,
                control: Arc::clone(&control),
                result,
            }) {
                previous.control.cancel();
            }
            self.0.ready.notify_one();
        }
        Ticket {
            control,
            result: receiver,
        }
    }
}

struct ServeWorker {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
    runtime: Option<tokio::runtime::Handle>,
}

impl Drop for ServeWorker {
    fn drop(&mut self) {
        self.shared.close();
        if self
            .thread
            .take()
            .is_some_and(|thread| thread.join().is_err())
        {
            tracing::warn!(target: "tui.dialog", "Serve worker stopped after a panic");
        }
    }
}

/// One active operation and one replaceable, not-yet-admitted request.
#[derive(Default)]
pub(crate) struct ServeJobs {
    worker: Option<ServeWorker>,
}

impl ServeJobs {
    pub(super) fn submitter(&mut self) -> Submitter {
        self.start(super::run_job)
    }

    fn start(
        &mut self,
        mut handler: impl FnMut(Job, &Control, &tokio::runtime::Runtime) -> JobResult + Send + 'static,
    ) -> Submitter {
        if self
            .worker
            .as_ref()
            .is_some_and(|worker| worker.thread.as_ref().is_none_or(JoinHandle::is_finished))
        {
            self.shutdown();
        }
        if let Some(worker) = &self.worker {
            return Submitter(Arc::clone(&worker.shared));
        }
        let shared = Arc::new(Shared::default());
        let worker_shared = Arc::clone(&shared);
        let spawned = std::thread::Builder::new()
            .name("aoe-serve-dialog".to_owned())
            .spawn(move || {
                let _exit = ExitGuard(Arc::clone(&worker_shared));
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                loop {
                    let request = {
                        let mut mailbox = worker_shared
                            .mailbox
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        while mailbox.queued.is_none() && !mailbox.closed {
                            mailbox = worker_shared
                                .ready
                                .wait(mailbox)
                                .unwrap_or_else(|e| e.into_inner());
                        }
                        if mailbox.closed {
                            return;
                        }
                        let request = mailbox.queued.take().expect("queued request");
                        mailbox.active = Some(Arc::clone(&request.control));
                        request
                    };
                    if !request.control.cancel.is_cancelled() {
                        let result = handler(request.job, &request.control, &runtime);
                        let _ = request.result.send((request.attempt, result));
                    }
                    worker_shared
                        .mailbox
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .active = None;
                }
            });
        match spawned {
            Ok(thread) => {
                self.worker = Some(ServeWorker {
                    shared: Arc::clone(&shared),
                    thread: Some(thread),
                    runtime: tokio::runtime::Handle::try_current().ok(),
                })
            }
            Err(error) => {
                shared.close();
                tracing::warn!(target: "tui.dialog", %error, "Serve worker could not start");
            }
        }
        Submitter(shared)
    }

    #[cfg(test)]
    pub(super) fn with_handler(
        &mut self,
        handler: impl FnMut(Job, &Control, &tokio::runtime::Runtime) -> JobResult + Send + 'static,
    ) -> Submitter {
        self.start(handler)
    }

    pub(crate) fn shutdown(&mut self) {
        if let Some(mut worker) = self.worker.take() {
            worker.shared.close();
            if !worker.thread.as_ref().is_none_or(JoinHandle::is_finished) {
                if let Some(runtime) = worker.runtime.take() {
                    // The owner also joins if the runtime cancels this finalizer.
                    runtime.spawn_blocking(move || drop(worker));
                }
            }
        }
    }
}

impl Drop for ServeJobs {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_is_deferred_only_while_mutation_is_admitted() {
        for admitted in [false, true] {
            for observing in [false, true] {
                let control = Control::new();
                if admitted {
                    assert!(control.admit());
                    assert!(!control.admit(), "admission cannot be repeated");
                }
                if observing {
                    control.finish_mutation();
                    assert!(!control.cancel.is_cancelled());
                }
                assert_eq!(control.cancel(), !admitted && !observing);
                assert_eq!(control.cancel(), !admitted && !observing);
                assert_eq!(
                    control.cancel.is_cancelled(),
                    !admitted || observing,
                    "admitted={admitted}, observing={observing}"
                );
                assert!(!control.admit(), "cancellation cannot reopen admission");
                control.finish_mutation();
                assert!(control.cancel.is_cancelled());
            }
        }
    }
}
