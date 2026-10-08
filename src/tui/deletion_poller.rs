//! Background deletion handlers for TUI responsiveness.

use std::collections::HashSet;
use std::sync::mpsc::TryRecvError;

use crate::session::deletion::{execute_deletion, execute_drop};
pub use crate::session::deletion::{DeletionRequest, DeletionResult};
use crate::session::Instance;
use crate::tui::worker::Worker;

enum DeletionCommand {
    Cleanup(DeletionRequest),
    KeepPaths(Instance),
}

// Request ids correlate UI completions; they never authorize or advance Purge.
struct DeletionJob {
    request_id: u64,
    command: DeletionCommand,
}

pub(super) struct DeletionDone {
    pub request_id: u64,
    pub result: DeletionResult,
}

pub struct DeletionPoller {
    worker: Worker<DeletionJob, DeletionDone>,
    forced: Vec<(u64, Worker<DeletionJob, DeletionDone>)>,
    normal_pending: HashSet<u64>,
    next_request: u64,
}

fn spawn_worker(name: &str) -> Worker<DeletionJob, DeletionDone> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build();
    Worker::spawn(name, move |job: DeletionJob| {
        let (instance, forced) = match &job.command {
            DeletionCommand::Cleanup(request) => (&request.instance, request.force_delete),
            DeletionCommand::KeepPaths(instance) => (instance, true),
        };
        let result = if forced {
            instance
                .original_storage()
                .and_then(|storage| storage.verify_profile_identity())
        } else {
            Ok(())
        };
        let result = match result {
            Err(error) => DeletionResult::rejected(
                instance.id.clone(),
                crate::session::deletion::DeletionDisposition::Failed,
                format!("Original force-removal profile is no longer valid: {error:#}"),
                None,
            ),
            Ok(()) => match runtime.as_ref() {
                Ok(runtime) => runtime.block_on(async {
                    match job.command {
                        DeletionCommand::Cleanup(request) => execute_deletion(request).await,
                        DeletionCommand::KeepPaths(instance) => execute_drop(instance).await,
                    }
                }),
                Err(error) => DeletionResult::rejected(
                    instance.id.clone(),
                    crate::session::deletion::DeletionDisposition::Failed,
                    format!("No runtime to settle this session's agent, so nothing was removed: {error}"),
                    None,
                ),
            },
        };
        DeletionDone {
            request_id: job.request_id,
            result,
        }
    })
}

impl DeletionPoller {
    pub fn new() -> Self {
        Self {
            worker: spawn_worker("aoe-deletion-poller"),
            forced: Vec::new(),
            normal_pending: HashSet::new(),
            next_request: 0,
        }
    }

    fn request(&mut self, command: DeletionCommand, forced: bool) -> u64 {
        self.next_request = self
            .next_request
            .checked_add(1)
            .expect("TUI deletion request overflow");
        let request_id = self.next_request;
        let job = DeletionJob {
            request_id,
            command,
        };
        if forced {
            let worker = spawn_worker("aoe-force-deletion");
            worker.request(job);
            self.forced.push((request_id, worker));
        } else {
            self.normal_pending.insert(request_id);
            self.worker.request(job);
        }
        request_id
    }

    pub fn request_deletion(&mut self, request: DeletionRequest) -> u64 {
        let forced = request.force_delete;
        self.request(DeletionCommand::Cleanup(request), forced)
    }

    pub(super) fn request_drop(&mut self, instance: Instance) -> u64 {
        self.request(DeletionCommand::KeepPaths(instance), true)
    }

    pub(super) fn try_recv_result(&mut self) -> Option<Result<DeletionDone, Vec<u64>>> {
        for index in 0..self.forced.len() {
            match self.forced[index].1.try_recv() {
                Ok(result) => {
                    self.forced.swap_remove(index);
                    return Some(Ok(result));
                }
                Err(TryRecvError::Disconnected) => {
                    let (request_id, _) = self.forced.swap_remove(index);
                    return Some(Err(vec![request_id]));
                }
                Err(TryRecvError::Empty) => {}
            }
        }
        match self.worker.try_recv() {
            Ok(result) => {
                self.normal_pending.remove(&result.request_id);
                Some(Ok(result))
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                if self.normal_pending.is_empty() {
                    None
                } else {
                    Some(Err(self.normal_pending.drain().collect()))
                }
            }
        }
    }
}

impl Default for DeletionPoller {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::deletion::DeletionDisposition;
    use crate::session::test_support::isolate_app_dir;
    use crate::session::Storage;
    use std::time::{Duration, Instant};

    fn next_result(poller: &mut DeletionPoller) -> DeletionDone {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(result) = poller.try_recv_result() {
                return result.expect("deletion worker remained alive");
            }
            assert!(Instant::now() < deadline, "deletion result did not arrive");
            std::thread::yield_now();
        }
    }

    #[test]
    #[serial_test::serial]
    fn force_refuses_a_replaced_profile_without_waiting_for_the_normal_fifo() {
        let app = isolate_app_dir();
        let storage = Storage::new_unwatched("default").unwrap();
        let row = Instance::new("physical-original", app.path().to_str().unwrap());
        storage
            .update(|rows, _| {
                rows.push(row);
                Ok(())
            })
            .unwrap();
        let before = storage.load().unwrap().remove(0);
        let original_bytes = std::fs::read(storage.sessions_path()).unwrap();
        let retired = app.path().join("retired-profile");
        std::fs::rename(storage.sessions_path().parent().unwrap(), &retired).unwrap();
        crate::session::create_profile("default").unwrap();
        let replacement = Storage::new_unwatched("default").unwrap();
        let mut peer = Instance::new("replacement-peer", app.path().to_str().unwrap());
        peer.id = before.id.clone();
        replacement
            .update(|rows, _| {
                rows.push(peer);
                Ok(())
            })
            .unwrap();
        let replacement_bytes = std::fs::read(replacement.sessions_path()).unwrap();
        let workspace = crate::session::acquire_session_workspace_claim_lock().unwrap();
        let request = |forced| DeletionRequest {
            session_id: before.id.clone(),
            instance: before.clone(),
            delete_worktree: false,
            delete_branch: false,
            delete_sandbox: false,
            force_delete: forced,
            detach_hooks: true,
            keep_scratch: true,
        };
        let mut poller = DeletionPoller::new();
        let normal_id = poller.request_deletion(request(false));
        let force_id = poller.request_deletion(request(true));
        let forced = next_result(&mut poller);
        assert_eq!(forced.request_id, force_id);
        assert_eq!(forced.result.disposition, DeletionDisposition::Failed);
        assert!(!forced.result.teardown_started);
        assert!(forced.result.retained_origin.is_none());
        assert_eq!(
            std::fs::read(replacement.sessions_path()).unwrap(),
            replacement_bytes
        );
        drop(workspace);
        let normal = next_result(&mut poller);
        assert_eq!(normal.request_id, normal_id);
        assert_eq!(normal.result.disposition, DeletionDisposition::Failed);
        assert!(!normal.result.teardown_started);
        assert_eq!(
            std::fs::read(retired.join("sessions.json")).unwrap(),
            original_bytes
        );
        assert_eq!(
            std::fs::read(replacement.sessions_path()).unwrap(),
            replacement_bytes
        );
    }
}
