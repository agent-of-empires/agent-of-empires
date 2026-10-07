//! Background deletion handler for TUI responsiveness

use std::sync::mpsc::TryRecvError;

use crate::session::deletion::{execute_deletion, execute_drop};
pub use crate::session::deletion::{DeletionRequest, DeletionResult};
use crate::session::Instance;
use crate::tui::worker::Worker;

enum DeletionJob {
    Cleanup(DeletionRequest),
    KeepPaths(Instance),
}

pub struct DeletionPoller {
    worker: Worker<DeletionJob, DeletionResult>,
}

impl DeletionPoller {
    pub fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        if let Err(error) = &runtime {
            tracing::warn!(target: "tui.deletion", "runtime build failed; deletion cannot prove its runner dead: {error}");
        }
        Self {
            worker: Worker::spawn("aoe-deletion-poller", move |job: DeletionJob| match runtime
                .as_ref()
            {
                Ok(runtime) => runtime.block_on(async {
                    match job {
                        DeletionJob::Cleanup(request) => execute_deletion(request).await,
                        DeletionJob::KeepPaths(instance) => execute_drop(instance).await,
                    }
                }),
                Err(error) => {
                    let id = match job {
                        DeletionJob::Cleanup(request) => request.session_id,
                        DeletionJob::KeepPaths(instance) => instance.id,
                    };
                    DeletionResult::rejected(
                            id,
                            crate::session::deletion::DeletionDisposition::Failed,
                            format!("No runtime to settle this session's agent, so nothing was removed: {error}"),
                            None,
                        )
                }
            }),
        }
    }

    pub fn request_deletion(&self, request: DeletionRequest) {
        self.worker.request(DeletionJob::Cleanup(request));
    }

    pub(super) fn request_drop(&self, instance: Instance) {
        self.worker.request(DeletionJob::KeepPaths(instance));
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
