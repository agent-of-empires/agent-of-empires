//! Background stop handler for TUI responsiveness.
//!
//! `docker stop` blocks for the container's grace period (~10s), which froze
//! the UI event loop (issue #1496), so stops run on a worker thread and the
//! main loop drains results each frame.

use crate::session::runner_journal::OwnedStop;
use std::sync::mpsc::TryRecvError;
use std::sync::Arc;

use crate::session::stop::perform_stop;
pub use crate::session::stop::{StopRequest, StopResult};
use crate::tui::worker::{SessionScoped, TrackedWorker};

impl SessionScoped for StopResult {
    fn session_id(&self) -> &str {
        &self.session_id
    }
}

pub struct StopPoller {
    worker: TrackedWorker<StopRequest, StopResult>,
}

impl StopPoller {
    pub fn new() -> Self {
        Self {
            worker: TrackedWorker::spawn("aoe-stop-poller", |request| perform_stop(&request)),
        }
    }

    pub fn request_stop(&mut self, request: StopRequest) {
        self.worker.request(request.session_id.clone(), request);
    }

    pub fn try_recv_result(&mut self) -> Result<StopResult, TryRecvError> {
        self.worker.try_recv()
    }

    /// Sessions whose stop never landed, for recovering rows left optimistically
    /// `Stopped` once the worker is known dead.
    pub fn take_pending(&mut self) -> Vec<String> {
        self.worker.take_pending()
    }
}

impl Default for StopPoller {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) enum SettlementAction {
    Workdir {
        name: String,
        rename_branch: bool,
    },
    Rename {
        title: String,
        group: Option<String>,
        profile: Option<String>,
        rename_branch: bool,
    },
    Archive {
        reveal: bool,
    },
}

pub(crate) struct SettlementRequest {
    pub session_id: String,
    pub storage: crate::session::Storage,
    pub instance: crate::session::Instance,
    pub action: SettlementAction,
}

pub(crate) struct SettlementResult {
    pub request: SettlementRequest,
    pub generation: anyhow::Result<StopCustody>,
}

impl SessionScoped for SettlementResult {
    fn session_id(&self) -> &str {
        &self.request.session_id
    }
}

pub(crate) struct StopCustody {
    pub stop: Arc<OwnedStop>,
    completion: Option<std::sync::mpsc::Sender<Arc<OwnedStop>>>,
}

impl StopCustody {
    pub fn disarm(&mut self) {
        self.completion = None;
    }
}

impl Drop for StopCustody {
    fn drop(&mut self) {
        if let Some(completion) = self.completion.take() {
            if completion.send(self.stop.clone()).is_err() {
                tracing::error!(target: "session.store", "Stop retirement worker disappeared; reservation remains fenced");
            }
        }
    }
}

pub(crate) struct SettlementShutdown {
    worker: TrackedWorker<SettlementRequest, SettlementResult>,
    completion: std::sync::mpsc::Sender<Arc<OwnedStop>>,
    retirement_worker: std::thread::JoinHandle<anyhow::Result<()>>,
}

impl SettlementShutdown {
    pub fn finish(self) -> anyhow::Result<()> {
        let result = self
            .worker
            .finish()
            .map_err(|_| anyhow::anyhow!("settlement worker panicked"));
        drop(self.completion);
        let retired = self
            .retirement_worker
            .join()
            .map_err(|_| anyhow::anyhow!("Stop retirement worker panicked"))?;
        result.and(retired)
    }
}

pub(crate) struct SettlementPoller {
    worker: Option<TrackedWorker<SettlementRequest, SettlementResult>>,
    completion: Option<std::sync::mpsc::Sender<Arc<OwnedStop>>>,
    retirement_worker: Option<std::thread::JoinHandle<anyhow::Result<()>>>,
}

impl SettlementPoller {
    pub fn new() -> anyhow::Result<Self> {
        let (completion, retired) = std::sync::mpsc::channel::<Arc<OwnedStop>>();
        let retirement_worker = std::thread::Builder::new()
            .name("aoe-stop-retirement".into())
            .spawn(move || {
                let mut failure = None;
                while let Ok(retirement) = retired.recv() {
                    if let Err(error) =
                        crate::session::runner_journal::release_owned_stop(&retirement)
                    {
                        if failure.is_none() {
                            failure = Some(error);
                        }
                    }
                }
                match failure {
                    Some(error) => Err(error),
                    None => Ok(()),
                }
            })?;
        let worker_completion = completion.clone();
        Ok(Self {
            completion: Some(completion),
            retirement_worker: Some(retirement_worker),
            worker: Some(TrackedWorker::spawn(
                "aoe-settlement-poller",
                move |request: SettlementRequest| {
                    let generation = (|| -> anyhow::Result<StopCustody> {
                        let runtime = tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()?;
                        let stop = crate::session::runner_journal::reserve_owned_stop(
                            &request.storage,
                            &request.instance,
                            !matches!(request.action, SettlementAction::Archive { .. }),
                        )?;
                        let custody = StopCustody {
                            stop: stop.clone(),
                            completion: Some(worker_completion.clone()),
                        };
                        runtime.block_on(async {
                            if matches!(request.action, SettlementAction::Archive { .. }) {
                                crate::session::runner_journal::settle(stop.clone()).await
                            } else {
                                crate::session::runner_journal::settle_if_idle(stop.clone()).await
                            }
                        })?;
                        Ok(custody)
                    })();
                    SettlementResult {
                        request,
                        generation,
                    }
                },
            )),
        })
    }

    pub fn request(&mut self, request: SettlementRequest) {
        if let Some(worker) = self.worker.as_mut() {
            worker.request(request.session_id.clone(), request);
        }
    }

    pub fn try_recv(&mut self) -> Result<SettlementResult, TryRecvError> {
        self.worker
            .as_mut()
            .ok_or(TryRecvError::Disconnected)?
            .try_recv()
    }

    pub fn take_pending(&mut self) -> Vec<String> {
        self.worker
            .as_mut()
            .map(TrackedWorker::take_pending)
            .unwrap_or_default()
    }

    pub fn take_shutdown(&mut self) -> Option<SettlementShutdown> {
        self.worker.take().map(|worker| SettlementShutdown {
            worker,
            completion: self
                .completion
                .take()
                .expect("live settlement retirement sender"),
            retirement_worker: self
                .retirement_worker
                .take()
                .expect("live settlement retirement worker"),
        })
    }
}

pub(crate) struct SettledEdit {
    pub storage: crate::session::Storage,
    pub custody: StopCustody,
}

impl SettledEdit {
    pub fn consume_under_locks(
        mut self,
        current: &crate::session::Instance,
    ) -> anyhow::Result<u64> {
        self.storage.verify_profile_identity()?;
        let stop = &self.custody.stop;
        let generation = stop.generation();
        stop.original().validate_baseline_at(current, generation)?;
        anyhow::ensure!(
            current.lifecycle_reservation_is_owned(
                crate::session::LifecycleOperation::Stop,
                generation
            ),
            "session changed during runner settlement"
        );
        crate::session::runner_journal::release_settled_stop_under_locks(stop)?;
        self.custody.disarm();
        Ok(generation)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Instance;
    use std::time::Duration;

    struct TestPoller(Option<StopPoller>);

    impl Drop for TestPoller {
        fn drop(&mut self) {
            if let Some(poller) = self.0.take() {
                let _ = poller.worker.finish();
            }
        }
    }

    fn fixture(profile: &str) -> (crate::session::Storage, TestPoller, Instance) {
        let storage = crate::session::Storage::new_unwatched(profile).unwrap();
        let mut instance = Instance::new("Test Session", "/tmp/test-project");
        instance.source_profile = profile.to_string();
        storage
            .update(|instances, _groups| {
                instances.push(instance.clone());
                Ok(())
            })
            .unwrap();
        let instance = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|row| row.id == instance.id)
            .unwrap();
        (storage, TestPoller(Some(StopPoller::new())), instance)
    }

    fn await_result(poller: &mut StopPoller) -> StopResult {
        for _ in 0..50 {
            match poller.try_recv_result() {
                Ok(result) => return result,
                Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(20)),
                Err(error) => panic!("stop worker disconnected: {error}"),
            }
        }
        panic!("timed out waiting for stop result");
    }

    #[test]
    #[serial_test::serial]
    fn stop_tracks_its_request_and_persists_the_status() {
        if !crate::tui::isolated_test_process(
            "tui::stop_poller::tests::stop_tracks_its_request_and_persists_the_status",
            Duration::from_secs(5),
        ) {
            return;
        }
        let _home = crate::session::test_support::isolate_app_dir();
        let (storage, mut held, instance) = fixture("default");
        let poller = held.0.as_mut().unwrap();
        let session_id = instance.id.clone();

        poller.request_stop(StopRequest {
            session_id: session_id.clone(),
            instance,
        });
        assert_eq!(poller.take_pending(), vec![session_id.clone()]);
        assert!(poller.take_pending().is_empty(), "take_pending drains");

        let result = await_result(poller);
        assert_eq!(result.session_id, session_id);
        assert!(result.success, "{:?}", result.error);
        assert_eq!(
            storage.load().unwrap()[0].status,
            crate::session::Status::Stopped
        );
    }
}
