//! Where supervisor events go: the durable event store and the live broadcast.

use std::future::Future;
use std::sync::Arc;

use tokio::sync::broadcast;

use crate::acp::acp_client::FIVE_HOUR_WINDOW_SECS;
use crate::acp::approvals::Nonce;
use crate::acp::event_sender::{AcpEventData, EventMetadata, EventScope};
use crate::acp::event_store::{
    AttachmentBlob, EventStore, RecordDisposition, ResetProvenance, UnresolvedBackgroundAgentLaunch,
};
use crate::acp::state::{BackgroundTaskLaunch, Event, RateLimitInfo, SubagentRecord};

/// Destination for published events; test sinks override only what they observe.
pub trait BroadcastSink: Send + Sync + 'static {
    fn publish(&self, session_id: &str, seq: u64, data: &AcpEventData);
    /// A sink without an actual durable store cannot certify persistence.
    fn publish_persisted(&self, session_id: &str, seq: u64, data: &AcpEventData) -> bool {
        self.publish(session_id, seq, data);
        false
    }
    /// Queue owned host DATA; allocation and persistence share the writer order.
    fn queue_persisted(
        &self,
        session_id: &str,
        next_seqs: Arc<super::SeqMap>,
        data: AcpEventData,
        expected_seq: Option<u64>,
    ) -> Persisted {
        let seq = super::with_session_sequence(&next_seqs, session_id, |_slot, current| {
            if expected_seq.is_some_and(|expected| expected != *current) {
                return None;
            }
            *current = current.saturating_add(1);
            Some(*current)
        });
        Persisted(Receipt::Ready(
            seq.map(|seq| (seq, self.publish_persisted(session_id, seq, &data))),
        ))
    }

    /// Store the prompt and its attachments in the same publication order.
    fn queue_prompt(
        &self,
        session_id: &str,
        next_seqs: Arc<super::SeqMap>,
        attachments: &[AttachmentBlob],
        data: AcpEventData,
    ) -> Persisted {
        let seq = super::with_publication_seq(&next_seqs, session_id, |seq| seq);
        let stored = attachments
            .iter()
            .all(|blob| self.record_attachment(session_id, seq, blob))
            && self.publish_persisted(session_id, seq, &data);
        Persisted(Receipt::Ready(Some((seq, stored))))
    }

    /// Transfer the original owned SDK item and completion to the writer.
    /// `None` transfers ownership; `Some` returns it for real inline completion.
    fn publish_from_worker(&self, publication: WorkerPublication) -> Option<WorkerPublication> {
        Some(publication)
    }

    /// Called only inside ordered completion, returning the actual durable seq and outcome.
    /// A duplicate retains its committed seq; no journal means no replay certification.
    fn publish_replay_from_worker(
        &self,
        session_id: &str,
        seq: u64,
        data: &AcpEventData,
        _generation: u64,
        _native_subject: Option<crate::acp::state::NativeEventSubject>,
        replay: Option<(u64, &mut crate::acp::event_store::ReplayCommit)>,
    ) -> (u64, bool) {
        if let Some((_, replay)) = replay {
            replay.reject();
            self.publish(session_id, seq, data);
            (seq, false)
        } else {
            (seq, self.publish_persisted(session_id, seq, data))
        }
    }

    fn establishment_store(&self) -> Option<Arc<EventStore>> {
        None
    }
    /// Erase durable session projections without resetting the publication counter.
    fn clear_session_events(
        &self,
        _session_id: &str,
        _next_seqs: Arc<super::SeqMap>,
    ) -> anyhow::Result<()> {
        anyhow::bail!("a durable event store is required to clear session events")
    }
    fn unsuccessful_main(
        &self,
        _session_id: &str,
    ) -> anyhow::Result<crate::acp::event_store::ControlKnowledge<bool>> {
        Ok(crate::acp::event_store::ControlKnowledge::Indeterminate)
    }
    /// Nonces of approvals requested on disk but never resolved.
    fn unresolved_approval_nonces(&self, _session_id: &str) -> Vec<Nonce> {
        Vec::new()
    }
    /// Nonces of elicitations requested on disk but never resolved.
    fn unresolved_elicitation_nonces(&self, _session_id: &str) -> Vec<Nonce> {
        Vec::new()
    }
    /// Agent ids of `BackgroundAgentLaunched` events on disk with no matching
    /// `BackgroundAgentCompleted`: sub-agents a dead worker's tailer will
    /// never report on again.
    fn unresolved_background_agent_ids(&self, _session_id: &str) -> Vec<String> {
        Vec::new()
    }
    /// Task ids of `BackgroundTaskStarted` events on disk with no matching
    /// `BackgroundTaskFinished`: tasks a dead worker will never report on.
    fn unresolved_background_task_ids(&self, _session_id: &str) -> Vec<String> {
        Vec::new()
    }
    fn unresolved_background_task_launches(
        &self,
        _session_id: &str,
    ) -> anyhow::Result<Vec<BackgroundTaskLaunch>> {
        anyhow::bail!("a durable event store is required to restore resource tracking")
    }
    /// Subagents whose latest snapshot on disk is still working.
    fn open_subagents(&self, _session_id: &str) -> Vec<SubagentRecord> {
        Vec::new()
    }
    /// The same rows, carrying `output_file` so `Supervisor::attach` can
    /// resume tailing a sub-agent that survived a daemon restart.
    fn unresolved_background_agent_launches(
        &self,
        _session_id: &str,
    ) -> anyhow::Result<Vec<UnresolvedBackgroundAgentLaunch>> {
        anyhow::bail!("a durable event store is required to restore resource tracking")
    }
    /// Persist a prompt attachment keyed to its `UserPromptSent` seq.
    fn record_attachment(&self, _session_id: &str, _seq: u64, _blob: &AttachmentBlob) -> bool {
        false
    }
    fn delete_attachments_for_seq(&self, _session_id: &str, _seq: u64) {}
}

/// The actual allocated seq and durable result; `None` is a refused conditional publication.
#[must_use = "wait for the store, or say why the outcome is not needed"]
pub struct Persisted(Receipt);

enum Receipt {
    Ready(Option<(u64, bool)>),
    Pending(std::sync::mpsc::Receiver<Option<(u64, bool)>>),
}

impl Persisted {
    pub fn wait(self) -> Option<(u64, bool)> {
        match self.0 {
            Receipt::Ready(stored) => stored,
            Receipt::Pending(wait) => blocking_io(|| wait.recv()).ok().flatten(),
        }
    }
}

/// One owned, sequential completion. The future owns the original SDK frame,
/// including every metadata/context Arc, journal sender and pending responder.
/// Borrowed processing and replay are created only while this item is executed.
pub struct WorkerPublication {
    runtime: tokio::runtime::Handle,
    completion: std::pin::Pin<Box<dyn Future<Output = ()> + Send + 'static>>,
    #[cfg(test)]
    submission: Option<TestSdkSubmission>,
}

impl WorkerPublication {
    pub(super) fn new(completion: impl Future<Output = ()> + Send + 'static) -> Self {
        Self {
            runtime: tokio::runtime::Handle::current(),
            completion: Box::pin(completion),
            #[cfg(test)]
            submission: None,
        }
    }

    #[cfg(test)]
    pub(super) fn with_test_submission(mut self, submission: Option<TestSdkSubmission>) -> Self {
        self.submission = submission;
        self
    }

    pub(super) async fn complete(self) {
        self.completion.await;
    }

    fn run(self) {
        self.runtime.block_on(self.completion);
    }
}

#[cfg(test)]
#[derive(PartialEq, Eq)]
pub(super) struct TestSdkSubmission {
    session_id: String,
    correlation: crate::acp::establishment::NativeOperationCorrelation,
    origin: crate::acp::state::NativeEventOrigin,
}

#[cfg(test)]
impl TestSdkSubmission {
    pub(super) fn from_incoming(
        session_id: &str,
        incoming: &crate::acp::event_sender::SdkEvent,
    ) -> Option<Self> {
        let crate::acp::event_sender::SdkEvent::Data(data) = incoming else {
            return None;
        };
        if data.metadata.scope != crate::acp::event_sender::EventScope::OperationCurrent
            || !matches!(data.event.as_ref(), crate::acp::state::Event::Stopped { reason } if reason == "prompt_complete")
        {
            return None;
        }
        Some(Self {
            session_id: session_id.to_owned(),
            correlation: data.metadata.operation.as_ref()?.correlation,
            origin: data.metadata.native_origin?,
        })
    }
}

#[cfg(test)]
struct TestSubmissionProbe {
    expected: TestSdkSubmission,
    submitted: tokio::sync::oneshot::Sender<()>,
}

/// Publications queued for the writer thread. A worker's drain blocks only
/// when this many are waiting, so a slow save does not stall its socket.
const WRITE_QUEUE_CAPACITY: usize = 4096;

/// The production sink: persist to the event store, fold the control cache,
/// then broadcast, in seq order on one writer thread.
pub struct ChannelSink {
    persister: Arc<Persister>,
    jobs: Option<std::sync::mpsc::SyncSender<Job>>,
    writer: Option<std::thread::JoinHandle<()>>,
    #[cfg(test)]
    flushes: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    submission_probe: std::sync::Mutex<Option<TestSubmissionProbe>>,
}

struct Persister {
    tx: broadcast::Sender<crate::server::AcpBroadcastFrame>,
    event_store: Arc<EventStore>,
}

struct Publication {
    session_id: String,
    next_seqs: Arc<super::SeqMap>,
    expected_seq: Option<u64>,
    data: AcpEventData,
    attachments: Vec<AttachmentBlob>,
    persisted: std::sync::mpsc::SyncSender<Option<(u64, bool)>>,
}

struct ClearJournal {
    session_id: String,
    next_seqs: Arc<super::SeqMap>,
    expected: Arc<std::sync::Mutex<super::SessionSequence>>,
    done: std::sync::mpsc::SyncSender<anyhow::Result<()>>,
}

enum Job {
    Publish(Box<Publication>),
    Worker(WorkerPublication),
    Clear(ClearJournal),
    Flush(std::sync::mpsc::SyncSender<()>),
    #[cfg(test)]
    Test(Box<dyn FnOnce() + Send>),
}

impl ChannelSink {
    pub fn new(
        tx: broadcast::Sender<crate::server::AcpBroadcastFrame>,
        event_store: Arc<EventStore>,
    ) -> Self {
        let persister = Arc::new(Persister { tx, event_store });
        let (jobs, queue) = std::sync::mpsc::sync_channel(WRITE_QUEUE_CAPACITY);
        let worker = Arc::clone(&persister);
        let writer = std::thread::Builder::new()
            .name("acp-event-writer".into())
            .spawn(move || {
                for job in queue {
                    // The job moves into the guarded call, so keep what names its gap.
                    let publication = match &job {
                        Job::Publish(p) => Some(p.session_id.clone()),
                        _ => None,
                    };
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker.run(job)))
                        .is_err()
                    {
                        tracing::error!(target: "acp.event_store", session_id = publication, "event writer job panicked; publication may be incomplete, continuing queued jobs");
                    }
                }
            })
            .map_err(|e| {
                tracing::warn!(target: "acp.event_store", "event writer thread failed to start; writing inline: {e}")
            })
            .ok();
        Self {
            persister,
            jobs: writer.is_some().then_some(jobs),
            writer,
            #[cfg(test)]
            flushes: Default::default(),
            #[cfg(test)]
            submission_probe: Default::default(),
        }
    }

    #[cfg(test)]
    fn notify_submission(&self, submission: Option<TestSdkSubmission>) {
        let Some(submission) = submission else {
            return;
        };
        let notify = {
            let mut probe = self.submission_probe.lock().unwrap();
            if probe
                .as_ref()
                .is_some_and(|probe| probe.expected == submission)
            {
                probe.take().map(|probe| probe.submitted)
            } else {
                None
            }
        };
        if let Some(notify) = notify {
            let _ = notify.send(());
        }
    }

    /// Queue `job` behind earlier publications; run it inline if the writer is gone.
    fn submit(&self, job: Job) {
        use std::sync::mpsc::TrySendError;
        if self
            .writer
            .as_ref()
            .is_some_and(|writer| writer.thread().id() == std::thread::current().id())
        {
            return self.persister.run(job);
        }
        let Some(jobs) = &self.jobs else {
            return self.persister.run(job);
        };
        let job = match jobs.try_send(job) {
            Ok(()) => return,
            Err(TrySendError::Disconnected(job)) => job,
            Err(TrySendError::Full(job)) => {
                tracing::warn!(target: "acp.event_store", "event write queue full; waiting for the writer");
                match blocking_io(|| jobs.send(job).err().map(|error| error.0)) {
                    None => return,
                    Some(job) => job,
                }
            }
        };
        self.persister.run(job);
    }

    /// Wait until every earlier publication is stored and broadcast.
    pub(super) fn flush(&self) {
        #[cfg(test)]
        self.flushes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let (done, wait) = std::sync::mpsc::sync_channel(1);
        self.submit(Job::Flush(done));
        let _ = blocking_io(|| wait.recv());
    }

    fn store(&self) -> &EventStore {
        &self.persister.event_store
    }

    /// Reads that must see this sink's earlier publications.
    fn flushed_store(&self) -> &EventStore {
        self.flush();
        self.store()
    }
}

impl super::Supervisor<ChannelSink> {
    /// Probe turn state after this sink's queued publications reach the store.
    pub fn has_in_flight_turn(
        &self,
        session_id: &str,
    ) -> anyhow::Result<crate::acp::event_store::ControlKnowledge<bool>> {
        self.sink.flushed_store().has_in_flight_turn(session_id)
    }

    /// Wait until every queued worker event is in the store, before a probe that
    /// decides whether to stop or replace a worker.
    pub(crate) fn flush_events(&self) {
        self.sink.flush();
    }

    /// Queue a gate or fault for deterministic writer tests.
    #[cfg(test)]
    pub(crate) fn test_run_on_writer(&self, run: impl FnOnce() + Send + 'static) {
        self.sink.submit(Job::Test(Box::new(run)));
    }

    #[cfg(test)]
    pub(crate) fn test_observe_stopped_submission(
        &self,
        session_id: &str,
        correlation: crate::acp::establishment::NativeOperationCorrelation,
        origin: crate::acp::state::NativeEventOrigin,
    ) -> tokio::sync::oneshot::Receiver<()> {
        let (submitted, received) = tokio::sync::oneshot::channel();
        let mut probe = self.sink.submission_probe.lock().unwrap();
        assert!(probe.is_none(), "one submission probe per sink");
        *probe = Some(TestSubmissionProbe {
            expected: TestSdkSubmission {
                session_id: session_id.to_owned(),
                correlation,
                origin,
            },
            submitted,
        });
        received
    }

    /// Flushes requested so far, so a test can tell a reader is waiting on the writer.
    #[cfg(test)]
    pub(crate) fn test_flushes(&self) -> usize {
        self.sink.flushes.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Drop for ChannelSink {
    /// Let the writer store what is queued before the sink goes away.
    fn drop(&mut self) {
        drop(self.jobs.take());
        if let Some(writer) = self.writer.take() {
            if writer.thread().id() != std::thread::current().id() {
                let _ = writer.join();
            }
        }
    }
}

impl BroadcastSink for ChannelSink {
    fn publish(&self, session_id: &str, seq: u64, data: &AcpEventData) {
        let _ = self.publish_persisted(session_id, seq, data);
    }

    /// Queue original owned completion without a per-frame task or durable wait.
    fn publish_from_worker(&self, publication: WorkerPublication) -> Option<WorkerPublication> {
        use std::sync::mpsc::TrySendError;
        #[cfg(test)]
        let (publication, submission) = {
            let mut publication = publication;
            let submission = publication.submission.take();
            (publication, submission)
        };
        if self
            .writer
            .as_ref()
            .is_some_and(|writer| writer.thread().id() == std::thread::current().id())
        {
            return Some(publication);
        }
        let Some(jobs) = &self.jobs else {
            return Some(publication);
        };
        let job = match jobs.try_send(Job::Worker(publication)) {
            Ok(()) => {
                #[cfg(test)]
                self.notify_submission(submission);
                return None;
            }
            Err(TrySendError::Disconnected(job)) => job,
            Err(TrySendError::Full(job)) => {
                match blocking_io(|| jobs.send(job).err().map(|error| error.0)) {
                    None => {
                        #[cfg(test)]
                        self.notify_submission(submission);
                        return None;
                    }
                    Some(job) => job,
                }
            }
        };
        let Job::Worker(publication) = job else {
            unreachable!("worker submission changed kind");
        };
        Some(publication)
    }

    fn publish_replay_from_worker(
        &self,
        session_id: &str,
        seq: u64,
        data: &AcpEventData,
        generation: u64,
        native_subject: Option<crate::acp::state::NativeEventSubject>,
        replay: Option<(u64, &mut crate::acp::event_store::ReplayCommit)>,
    ) -> (u64, bool) {
        self.persister.publish_tagged(
            session_id,
            seq,
            data,
            Some(generation),
            native_subject,
            replay,
        )
    }

    fn establishment_store(&self) -> Option<Arc<EventStore>> {
        Some(Arc::clone(&self.persister.event_store))
    }

    fn clear_session_events(
        &self,
        session_id: &str,
        next_seqs: Arc<super::SeqMap>,
    ) -> anyhow::Result<()> {
        let expected = super::sequence_slot(&next_seqs, session_id);
        let (done, wait) = std::sync::mpsc::sync_channel(1);
        self.submit(Job::Clear(ClearJournal {
            session_id: session_id.to_owned(),
            next_seqs,
            expected,
            done,
        }));
        blocking_io(|| wait.recv())
            .map_err(|_| anyhow::anyhow!("event writer ended before clearing the journal"))?
    }

    fn unsuccessful_main(
        &self,
        session_id: &str,
    ) -> anyhow::Result<crate::acp::event_store::ControlKnowledge<bool>> {
        self.flushed_store()
            .has_unsuccessful_main_outcome(session_id)
    }

    fn publish_persisted(&self, session_id: &str, seq: u64, data: &AcpEventData) -> bool {
        self.persister
            .publish_tagged(session_id, seq, data, None, None, None)
            .1
    }

    fn queue_persisted(
        &self,
        session_id: &str,
        next_seqs: Arc<super::SeqMap>,
        data: AcpEventData,
        expected_seq: Option<u64>,
    ) -> Persisted {
        let (done, wait) = std::sync::mpsc::sync_channel(1);
        self.submit(Job::Publish(Box::new(Publication {
            session_id: session_id.to_owned(),
            next_seqs,
            expected_seq,
            data,
            attachments: Vec::new(),
            persisted: done,
        })));
        Persisted(Receipt::Pending(wait))
    }

    fn queue_prompt(
        &self,
        session_id: &str,
        next_seqs: Arc<super::SeqMap>,
        attachments: &[AttachmentBlob],
        data: AcpEventData,
    ) -> Persisted {
        let (done, wait) = std::sync::mpsc::sync_channel(1);
        self.submit(Job::Publish(Box::new(Publication {
            session_id: session_id.to_owned(),
            next_seqs,
            expected_seq: None,
            data,
            attachments: attachments.to_vec(),
            persisted: done,
        })));
        Persisted(Receipt::Pending(wait))
    }

    fn unresolved_approval_nonces(&self, session_id: &str) -> Vec<Nonce> {
        self.flushed_store().unresolved_approval_nonces(session_id)
    }

    fn unresolved_elicitation_nonces(&self, session_id: &str) -> Vec<Nonce> {
        self.flushed_store()
            .unresolved_elicitation_nonces(session_id)
    }

    fn unresolved_background_agent_ids(&self, session_id: &str) -> Vec<String> {
        self.flushed_store()
            .unresolved_background_agent_ids(session_id)
    }

    fn unresolved_background_task_ids(&self, session_id: &str) -> Vec<String> {
        self.flushed_store()
            .unresolved_background_task_ids(session_id)
    }

    fn unresolved_background_task_launches(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Vec<BackgroundTaskLaunch>> {
        self.flushed_store()
            .unresolved_background_task_launches(session_id)
    }

    fn open_subagents(&self, session_id: &str) -> Vec<SubagentRecord> {
        self.flushed_store().open_subagents(session_id)
    }

    fn unresolved_background_agent_launches(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Vec<UnresolvedBackgroundAgentLaunch>> {
        self.flushed_store()
            .unresolved_background_agent_launches(session_id)
    }

    fn record_attachment(&self, session_id: &str, seq: u64, blob: &AttachmentBlob) -> bool {
        blocking_io(|| self.store().record_attachment(session_id, seq, blob))
    }

    fn delete_attachments_for_seq(&self, session_id: &str, seq: u64) {
        blocking_io(|| self.store().delete_attachments_for_seq(session_id, seq))
    }
}

/// Run blocking work via `block_in_place` on a multi-thread runtime, inline elsewhere.
pub(super) fn blocking_io<T>(f: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()) {
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(f),
        _ => f(),
    }
}

/// A reset a fresh rejection inherits from the previous one.
struct InheritedReset {
    /// The window the rejection carries: its own, or the source's for a window-less one.
    window: String,
    resets_at: crate::time::Timestamp,
    observed_at_ms: i64,
    /// The window the reset was actually reported for. A reset reported for no window stays
    /// generic however many rejections it is copied through.
    source_window: String,
}

/// Window and reset time a fresh rejection of quota window `kind` inherits from the previous
/// one, while the reset is still ahead of `now`. A report naming no window (`rate_limit`) may
/// take any quota window's reset and keeps that window; a report of another limit, such as
/// `billing_error`, never takes a quota window's. An unnamed reset a five-hour window could not
/// have reached from when it was observed (`previous.1.observed_at_ms`) is not that window's.
fn inheritable_rate_limit_reset(
    previous: Option<(RateLimitInfo, ResetProvenance)>,
    kind: &str,
    now: crate::time::Timestamp,
) -> Option<InheritedReset> {
    let (previous, provenance) = previous?;
    let source_window = provenance.source_window.unwrap_or(previous.kind);
    let compatible = source_window == kind
        || (source_window != "billing_error"
            && kind != "billing_error"
            && (source_window == "rate_limit" || kind == "rate_limit"));
    if !compatible {
        return None;
    }
    let resets_at = previous.resets_at.filter(|resets_at| *resets_at > now)?;
    let unreachable = kind == "five_hour"
        && source_window == "rate_limit"
        && resets_at.as_millis() - provenance.observed_at_ms > FIVE_HOUR_WINDOW_SECS * 1000;
    if unreachable {
        return None;
    }
    let window = if kind == "rate_limit" {
        source_window.clone()
    } else {
        kind.to_string()
    };
    Some(InheritedReset {
        window,
        resets_at,
        observed_at_ms: provenance.observed_at_ms,
        source_window,
    })
}

impl Persister {
    fn bind_pending_publication(&self, session_id: &str, seq: u64, data: &AcpEventData) {
        let Some(publication) = &data.pending_request else {
            return;
        };
        if !Arc::ptr_eq(publication.event(), &data.event) || publication.metadata() != data.metadata
        {
            return;
        }
        publication.mark_published(seq);
        if publication.is_original() {
            self.event_store
                .control_cache()
                .bind_original_responders(session_id, publication.responders());
        }
    }

    fn run(&self, job: Job) {
        match job {
            Job::Publish(publication) => {
                let Publication {
                    session_id,
                    next_seqs,
                    expected_seq,
                    data,
                    attachments,
                    persisted,
                } = *publication;
                let candidate =
                    super::with_session_sequence(&next_seqs, &session_id, |slot, current| {
                        if expected_seq.is_some_and(|expected| expected != *current) {
                            return None;
                        }
                        *current = current.saturating_add(1);
                        Some((Arc::clone(slot), *current))
                    });
                let outcome = candidate.map(|(slot, seq)| {
                    let outcome = if attachments
                        .iter()
                        .all(|blob| self.event_store.record_attachment(&session_id, seq, blob))
                    {
                        self.publish_tagged(&session_id, seq, &data, None, None, None)
                    } else {
                        (seq, false)
                    };
                    if outcome.1 && outcome.0 < seq {
                        let mut current = super::lock_recover(&slot);
                        if !current.retired && current.seq == seq {
                            current.seq = seq - 1;
                        }
                    }
                    outcome
                });
                let _ = persisted.send(outcome);
            }
            Job::Worker(publication) => publication.run(),
            Job::Clear(clear) => {
                let ClearJournal {
                    session_id,
                    next_seqs,
                    expected,
                    done,
                } = clear;
                let result = (|| -> anyhow::Result<()> {
                    let current = super::lock_recover(&next_seqs).get(&session_id).cloned();
                    anyhow::ensure!(
                        current
                            .as_ref()
                            .is_some_and(|slot| Arc::ptr_eq(slot, &expected))
                            && !super::lock_recover(&expected).retired,
                        "journal publication namespace changed before clear"
                    );
                    // The writer orders clear with every append. No sequence/global lock
                    // survives the durable delete or a writer barrier.
                    self.event_store.delete_session_checked(&session_id)?;
                    let mut sequence = super::lock_recover(&expected);
                    let mut sequences = super::lock_recover(&next_seqs);
                    anyhow::ensure!(
                        sequences
                            .get(&session_id)
                            .is_some_and(|slot| Arc::ptr_eq(slot, &expected)),
                        "journal publication namespace changed during clear"
                    );
                    sequence.retired = true;
                    sequences.remove(&session_id);
                    Ok(())
                })();
                let _ = done.send(result);
            }
            Job::Flush(done) => {
                let _ = done.send(());
            }
            #[cfg(test)]
            Job::Test(run) => run(),
        }
    }

    fn publish_tagged(
        &self,
        session_id: &str,
        seq: u64,
        data: &AcpEventData,
        worker_generation: Option<u64>,
        native_subject: Option<crate::acp::state::NativeEventSubject>,
        replay: Option<(u64, &mut crate::acp::event_store::ReplayCommit)>,
    ) -> (u64, bool) {
        let inherited = match data.event.as_ref() {
            Event::RateLimit { info }
                if info.resets_at.is_none()
                    && replay.is_none()
                    && matches!(
                        data.metadata.scope,
                        EventScope::SessionCurrent | EventScope::OperationCurrent
                    ) =>
            {
                let previous = match self
                    .event_store
                    .latest_rate_limit_with_provenance(session_id)
                {
                    Ok(previous) => previous,
                    Err(error) => {
                        tracing::warn!(target: "acp.event_store", session = %session_id,
                            %error, "quota calendar inheritance unavailable");
                        None
                    }
                };
                inheritable_rate_limit_reset(previous, &info.kind, crate::time::Timestamp::now())
                    .map(|inherited| {
                        let event = Arc::new(Event::RateLimit {
                            info: RateLimitInfo {
                                resets_at: Some(inherited.resets_at),
                                kind: inherited.window,
                                ..info.clone()
                            },
                        });
                        (event, inherited.observed_at_ms, inherited.source_window)
                    })
            }
            _ => None,
        };
        let event = inherited.as_ref().map_or(&data.event, |(event, ..)| event);
        let metadata = &data.metadata;
        let native_subject = native_subject.or_else(|| event.native_subject());
        let (event, persisted) = match blocking_io(|| match replay {
            Some((ordinal, replay)) => self
                .event_store
                .record_replay(session_id, seq, event, metadata, ordinal, replay),
            None => match inherited.as_ref() {
                Some((_, observed_at_ms, source_window)) => {
                    self.event_store.record_inherited_rate_limit(
                        session_id,
                        seq,
                        event,
                        metadata,
                        *observed_at_ms,
                        source_window,
                    )
                }
                None => self.event_store.record(session_id, seq, event, metadata),
            },
        }) {
            Ok(RecordDisposition::Existing { seq: stored_seq }) => {
                self.bind_pending_publication(session_id, stored_seq, data);
                return (stored_seq, true);
            }
            Ok(RecordDisposition::Inserted) => {
                self.bind_pending_publication(session_id, seq, data);
                (Arc::clone(event), true)
            }
            Err(e) => {
                tracing::warn!(
                    target: "acp.event_store",
                    session = %session_id,
                    seq,
                    "event store write failed: {e}"
                );
                let failure = Event::SessionNotice {
                    key: Some(format!("event-store-{seq}")),
                    severity: "error".into(),
                    title: "Event storage failed".into(),
                    description: Some(
                        "The event could not be saved; the native session was not replaced.".into(),
                    ),
                };
                (Arc::new(failure), false)
            }
        };

        // Fold before broadcasting, in the same seq order the log is written in.
        let sidebar_status = if persisted {
            self.event_store
                .control_cache()
                .apply_if_cached(session_id, seq, &event, metadata)
        } else {
            self.event_store.control_cache().record_failed_observation(
                session_id,
                seq,
                matches!(
                    metadata.scope,
                    EventScope::SessionCurrent | EventScope::OperationCurrent
                )
                .then_some(native_subject)
                .flatten(),
            );
            None
        };
        let failure_metadata = EventMetadata::history();
        let _ = self
            .tx
            .send(crate::server::AcpBroadcastFrame::from_qualified(
                session_id.to_owned(),
                seq,
                event,
                if persisted {
                    metadata
                } else {
                    &failure_metadata
                },
                worker_generation,
                if persisted { native_subject } else { None },
                sidebar_status,
            ));
        (seq, persisted)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::channel_sink;
    use super::*;

    /// A failed writer job must not discard the publications queued behind it.
    #[test]
    fn writer_panic_preserves_later_queued_publications() {
        use std::time::Duration;

        let (sink, store, mut rx, _tmp) = channel_sink();
        let (entered, ready) = std::sync::mpsc::sync_channel(1);
        let (release, wait) = std::sync::mpsc::sync_channel(1);
        sink.submit(Job::Test(Box::new(move || {
            entered.send(()).unwrap();
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
            panic!("injected writer failure");
        })));
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for seq in 1..=2 {
            let persister = Arc::clone(&sink.persister);
            let publication = {
                let _entered = runtime.enter();
                WorkerPublication::new(async move {
                    assert!(
                        persister
                            .publish_tagged(
                                "writer-panic",
                                seq,
                                &AcpEventData::from_history(Event::ThinkingStarted),
                                Some(7),
                                None,
                                None
                            )
                            .1
                    );
                })
            };
            assert!(sink.publish_from_worker(publication).is_none());
        }
        // This barrier is also queued before the panic, so a later inline
        // fallback cannot hide the loss of buffered publications.
        let (done, flushed) = std::sync::mpsc::sync_channel(1);
        sink.submit(Job::Flush(done));
        release.send(()).unwrap();
        flushed
            .recv_timeout(Duration::from_secs(5))
            .expect("writer must continue after a panic");
        let seqs: Vec<_> = store
            .replay_from("writer-panic", 0)
            .expect("read retained events")
            .into_iter()
            .map(|stored| stored.seq)
            .collect();
        assert_eq!(seqs, [1, 2]);
        for seq in 1..=2 {
            let frame = rx.try_recv().unwrap();
            assert_eq!(frame.seq, seq);
            assert_eq!(frame.worker_generation, Some(7));
        }
        assert!(sink.publish_persisted(
            "writer-panic",
            3,
            &AcpEventData::from_history(Event::ThinkingStarted)
        ));
        assert_eq!(store.highest_seq("writer-panic").unwrap(), 3);
    }

    /// Stalls a save on the writer connection until the returned sender fires.
    fn stall_writer(sink: &ChannelSink) -> std::sync::mpsc::SyncSender<()> {
        use std::time::Duration;

        let (entered, ready) = std::sync::mpsc::sync_channel(1);
        let (release, wait) = std::sync::mpsc::sync_channel(1);
        let store = Arc::clone(&sink.persister.event_store);
        sink.submit(Job::Test(Box::new(move || {
            let _writer = store.hold_writer();
            entered.send(()).unwrap();
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
        })));
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        release
    }

    /// A synchronous receipt waiter never owns the seq lock while the writer is blocked.
    #[test]
    fn a_stalled_sync_publish_leaves_the_seq_lock_to_the_drain() {
        use std::time::Duration;
        for with_attachment in [false, true] {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let (sink, store, _rx, _tmp) = channel_sink();
            let sup = Arc::new(super::super::Supervisor::new(Arc::clone(&sink)));
            let release = stall_writer(&sink);
            let (submitted, queued) = std::sync::mpsc::sync_channel(1);
            let publisher = {
                let sink = Arc::clone(&sink);
                let seqs = Arc::clone(&sup.next_seqs);
                std::thread::spawn(move || {
                    let receipt = if with_attachment {
                        let blob = AttachmentBlob {
                            id: "a-1".into(),
                            kind: crate::daemon::PromptAttachmentKind::Image,
                            mime_type: "image/png".into(),
                            name: None,
                            data: vec![1, 2, 3],
                        };
                        sink.queue_prompt(
                            "stalled",
                            seqs,
                            &[blob],
                            AcpEventData::from_host(crate::acp::state::test_support::prompt(
                                "look",
                            )),
                        )
                    } else {
                        sink.queue_persisted(
                            "stalled",
                            seqs,
                            AcpEventData::from_history(Event::ThinkingStarted),
                            None,
                        )
                    };
                    submitted.send(()).unwrap();
                    receipt.wait()
                })
            };
            queued
                .recv_timeout(Duration::from_secs(5))
                .expect("the real host publication is enqueued before its receipt wait");
            assert_eq!(
                super::super::try_activity_seq(&sup.next_seqs, "stalled")
                    .expect("a durable waiter must leave seq custody free"),
                0
            );
            let persister = Arc::clone(&sink.persister);
            let seqs = Arc::clone(&sup.next_seqs);
            let publication = {
                let _entered = runtime.enter();
                WorkerPublication::new(async move {
                    let seq = super::super::with_publication_seq(&seqs, "stalled", |seq| seq);
                    assert!(
                        persister
                            .publish_tagged(
                                "stalled",
                                seq,
                                &AcpEventData::from_history(Event::ThinkingEnded),
                                Some(1),
                                None,
                                None
                            )
                            .1
                    );
                })
            };
            assert!(
                sink.publish_from_worker(publication).is_none(),
                "the owned drain publication queues while the writer is stalled"
            );
            assert_eq!(
                super::super::try_activity_seq(&sup.next_seqs, "stalled")
                    .expect("queued completion owns no seq guard"),
                0
            );
            assert!(
                !publisher.is_finished(),
                "the actual durable receipt remains blocked"
            );
            release.send(()).unwrap();
            assert_eq!(publisher.join().unwrap(), Some((1, true)));
            sink.flush();
            let stored: Vec<_> = store
                .replay_from("stalled", 0)
                .unwrap()
                .into_iter()
                .map(|stored| stored.seq)
                .collect();
            assert_eq!(
                stored,
                [1, 2],
                "host and worker share actual allocation and durable FIFO order"
            );
        }
    }

    #[test]
    fn dropping_the_sink_stores_queued_publications() {
        use std::time::{Duration, Instant};

        let (sink, store, _rx, _tmp) = channel_sink();
        let release = stall_writer(&sink);
        let receipt = sink.queue_persisted(
            "shutdown",
            Default::default(),
            AcpEventData::from_history(Event::ThinkingStarted),
            None,
        );
        let alive = Arc::downgrade(&sink);
        let (dropped, done) = std::sync::mpsc::sync_channel(1);
        let dropper = std::thread::spawn(move || {
            drop(sink);
            dropped.send(()).unwrap();
        });
        // The last strong reference is gone, so the destructor is running.
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive.strong_count() > 0 {
            assert!(Instant::now() < deadline, "the sink was never dropped");
            std::thread::yield_now();
        }
        // Drop may only return once the writer has run what was queued.
        if done.recv_timeout(Duration::from_millis(200)).is_ok() {
            assert_eq!(
                store.highest_seq("shutdown").unwrap(),
                1,
                "drop returned before the queued save"
            );
        }
        release.send(()).unwrap();
        dropper.join().unwrap();
        assert_eq!(store.highest_seq("shutdown").unwrap(), 1);
        assert_eq!(receipt.wait(), Some((1, true)));
    }

    #[test]
    fn inheritable_reset_takes_only_a_future_previous_reset() {
        let now = crate::time::Timestamp::from_unix_secs(1_800_000_000).expect("now");
        let future = now + crate::time::Duration::hours(2);
        let info = |resets_at| RateLimitInfo {
            status: "usage limit reached".into(),
            resets_at,
            kind: "rate_limit".into(),
        };
        let generic = |at| Some(("rate_limit".to_string(), at));
        let from = |previous: Option<RateLimitInfo>,
                    observed_at_ms: i64,
                    source_window: Option<&str>,
                    kind: &str,
                    now: crate::time::Timestamp| {
            super::inheritable_rate_limit_reset(
                previous.map(|p| {
                    (
                        p,
                        ResetProvenance {
                            observed_at_ms,
                            source_window: source_window.map(str::to_string),
                        },
                    )
                }),
                kind,
                now,
            )
        };
        // Observed just now, unless a case says otherwise.
        let inheritable_rate_limit_reset =
            |previous: Option<RateLimitInfo>, kind: &str, now: crate::time::Timestamp| {
                from(previous, now.as_millis(), None, kind, now).map(|i| (i.window, i.resets_at))
            };
        assert_eq!(
            inheritable_rate_limit_reset(Some(info(Some(future))), "rate_limit", now),
            generic(future)
        );
        assert_eq!(
            inheritable_rate_limit_reset(
                Some(info(Some(now - crate::time::Duration::minutes(1)))),
                "rate_limit",
                now
            ),
            None
        );
        assert_eq!(
            inheritable_rate_limit_reset(Some(info(None)), "rate_limit", now),
            None
        );
        assert_eq!(inheritable_rate_limit_reset(None, "rate_limit", now), None);
        // An unnamed capture's reset is compatible with a later report naming its window; a
        // billing error is compatible with neither.
        let unnamed = RateLimitInfo {
            kind: "rate_limit".into(),
            ..info(Some(future))
        };
        assert_eq!(
            inheritable_rate_limit_reset(Some(unnamed.clone()), "seven_day", now),
            Some(("seven_day".to_string(), future))
        );
        // ...unless a five-hour window cannot reach it: that reset is some longer window's.
        let multi_day = now + crate::time::Duration::days(3);
        let long_unnamed = RateLimitInfo {
            kind: "rate_limit".into(),
            ..info(Some(multi_day))
        };
        assert_eq!(
            inheritable_rate_limit_reset(Some(long_unnamed.clone()), "five_hour", now),
            None
        );
        assert_eq!(
            inheritable_rate_limit_reset(Some(unnamed.clone()), "five_hour", now),
            Some(("five_hour".to_string(), future))
        );
        // The reach is measured from when the reset was observed, so a later look, now
        // within five hours of it, does not make it a five-hour window's.
        let observed_long_ago = (now - crate::time::Duration::days(3)).as_millis();
        let later = multi_day - crate::time::Duration::hours(1);
        assert!(from(
            Some(long_unnamed.clone()),
            observed_long_ago,
            None,
            "five_hour",
            later
        )
        .is_none());
        assert_eq!(
            from(
                Some(long_unnamed.clone()),
                observed_long_ago,
                None,
                "seven_day",
                later
            )
            .map(|i| (i.window, i.resets_at)),
            Some(("seven_day".to_string(), multi_day))
        );
        // The source window is the one the reset was reported for, however often it was copied:
        // a named rejection that took an unnamed reset is still carrying a generic one, and a
        // later rejection copying from it must not take it for that window's own.
        let copied = RateLimitInfo {
            kind: "seven_day".into(),
            ..info(Some(multi_day))
        };
        let source = |previous, source_window: Option<&str>, kind| {
            from(
                Some(previous),
                observed_long_ago,
                source_window,
                kind,
                later,
            )
            .map(|i| (i.window, i.source_window))
        };
        assert_eq!(
            source(long_unnamed.clone(), None, "seven_day"),
            Some(("seven_day".to_string(), "rate_limit".to_string()))
        );
        assert_eq!(
            source(copied.clone(), Some("rate_limit"), "rate_limit"),
            Some(("rate_limit".to_string(), "rate_limit".to_string())),
            "a window-less rejection keeps the source window, not the label it was copied under"
        );
        assert_eq!(
            source(copied.clone(), Some("rate_limit"), "five_hour"),
            None,
            "the five-hour reach is judged on the generic source"
        );
        assert_eq!(
            source(copied, None, "rate_limit"),
            Some(("seven_day".to_string(), "seven_day".to_string())),
            "a reset reported for the weekly window is the weekly window's"
        );
        // The bound is inclusive: five hours exactly is reachable, one second more is not.
        for (secs, expected) in [(5 * 3600, true), (5 * 3600 + 1, false)] {
            let edge = now + crate::time::Duration::seconds(secs);
            let edge_unnamed = RateLimitInfo {
                kind: "rate_limit".into(),
                ..info(Some(edge))
            };
            assert_eq!(
                inheritable_rate_limit_reset(Some(edge_unnamed), "five_hour", now).is_some(),
                expected,
                "reset {secs}s out"
            );
        }
        // An unnamed report keeps the window the reset belonged to.
        let long_weekly = RateLimitInfo {
            kind: "seven_day".into(),
            ..info(Some(multi_day))
        };
        assert_eq!(
            inheritable_rate_limit_reset(Some(long_weekly), "rate_limit", now),
            Some(("seven_day".to_string(), multi_day))
        );
        assert_eq!(
            inheritable_rate_limit_reset(Some(unnamed), "billing_error", now),
            None
        );
        let billing = RateLimitInfo {
            kind: "billing_error".into(),
            ..info(Some(future))
        };
        assert_eq!(
            inheritable_rate_limit_reset(Some(billing), "rate_limit", now),
            None
        );
        // Only a quota window's own report, or one naming no window, takes its reset.
        let weekly = RateLimitInfo {
            kind: "seven_day".into(),
            ..info(Some(future))
        };
        for (kind, want) in [
            ("seven_day", Some(("seven_day".to_string(), future))),
            ("rate_limit", Some(("seven_day".to_string(), future))),
            ("five_hour", None),
            ("billing_error", None),
        ] {
            assert_eq!(
                inheritable_rate_limit_reset(Some(weekly.clone()), kind, now),
                want,
                "{kind}"
            );
        }
    }

    #[tokio::test]
    async fn channel_sink_inherits_a_missing_rate_limit_reset() {
        use crate::acp::establishment::NativeOperationCorrelation;
        use crate::acp::event_sender::{AcpEventData, EventMetadata, EventScope};
        use crate::acp::protocol::{
            AcpOperationMetadata, AcpOriginalRequestResult, AcpPromptObservation, AcpPromptWindowId,
        };
        use crate::acp::state::NativeEventOrigin;
        let (sink, event_store, _rx, _tmp) = channel_sink();
        let resets_at = crate::time::Timestamp::now() + crate::time::Duration::hours(3);
        let info = |resets_at| RateLimitInfo {
            status: "usage limit reached".into(),
            resets_at,
            kind: "rate_limit".into(),
        };
        // SQL/projection DATA only: this fixture never creates a Native request,
        // receipt, witness, ACK or filesystem grant. No Drain consumes these rows.
        let origin = NativeEventOrigin {
            context: serde_json::from_value(serde_json::to_value([41u8; 32]).unwrap()).unwrap(),
            generation: 9,
            attempt_id: serde_json::from_value(serde_json::json!(uuid::Uuid::new_v4())).unwrap(),
            producer: uuid::Uuid::new_v4(),
            owner: Some(super::super::test_support::attested_test_identity(
                505309, 9,
            )),
        };
        let correlation = NativeOperationCorrelation::new(None);
        let admission = EventMetadata {
            scope: EventScope::OperationCurrent,
            native_origin: Some(origin),
            operation: Some(AcpOperationMetadata {
                correlation,
                outcome: None,
            }),
            prompt_observation: Some(AcpPromptObservation::Written {
                window_id: AcpPromptWindowId::new(),
            }),
            ..EventMetadata::history()
        };
        let publish = |seq, event, metadata| {
            assert!(sink.publish_persisted(
                "s-rl",
                seq,
                &AcpEventData {
                    event: Arc::new(event),
                    metadata,
                    current_scope: None,
                    constructor_scope: None,
                    journal_publication: None,
                    pending_request: None,
                }
            ));
        };
        publish(1, Event::ThinkingStarted, admission.clone());
        let quota = EventMetadata {
            operation: Some(AcpOperationMetadata {
                correlation,
                outcome: Some(AcpOriginalRequestResult::Failed),
            }),
            ..admission.clone()
        };
        publish(
            2,
            Event::RateLimit {
                info: info(Some(resets_at)),
            },
            quota.clone(),
        );
        publish(
            3,
            Event::Stopped {
                reason: "rate_limited".into(),
            },
            quota,
        );
        let next_correlation = NativeOperationCorrelation::new(None);
        let next_admission = EventMetadata {
            operation: Some(AcpOperationMetadata {
                correlation: next_correlation,
                outcome: None,
            }),
            prompt_observation: Some(AcpPromptObservation::Written {
                window_id: AcpPromptWindowId::new(),
            }),
            ..admission
        };
        publish(4, Event::ThinkingStarted, next_admission.clone());
        publish(
            5,
            Event::RateLimit { info: info(None) },
            EventMetadata {
                operation: Some(AcpOperationMetadata {
                    correlation: next_correlation,
                    outcome: Some(AcpOriginalRequestResult::Failed),
                }),
                ..next_admission
            },
        );
        let stored = event_store
            .replay_from("s-rl", 4)
            .expect("durable quota suffix");
        let last = stored.last().expect("second quota persisted");
        assert_eq!(last.seq, 5);
        let Event::RateLimit { info: stored_info } = last.event.as_ref() else {
            panic!("expected stored RateLimit, got {:?}", last.event);
        };
        assert_eq!(stored_info.resets_at, Some(resets_at));
    }
}
