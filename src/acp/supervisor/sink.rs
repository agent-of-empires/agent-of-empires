//! Where supervisor events go: the durable event store and the live broadcast.

use std::sync::Arc;

use tokio::sync::broadcast;

use crate::acp::approvals::Nonce;
use crate::acp::event_store::{AttachmentBlob, EventStore, UnresolvedBackgroundAgentLaunch};
use crate::acp::state::{BackgroundTaskLaunch, Event, RateLimitInfo};

/// Destination for published events; test sinks override only what they observe.
pub trait BroadcastSink: Send + Sync + 'static {
    fn publish(&self, session_id: &str, seq: u64, event: &Event);
    /// Like `publish`, but reports whether the event reached the durable store.
    fn publish_persisted(&self, session_id: &str, seq: u64, event: &Event) -> bool {
        self.publish(session_id, seq, event);
        true
    }
    /// Queue an event for the durable store without waiting for it. Callers
    /// holding a session's seq lock wait on the receipt after releasing it.
    fn queue_persisted(&self, session_id: &str, seq: u64, event: &Event) -> Persisted {
        Persisted(Receipt::Ready(
            self.publish_persisted(session_id, seq, event),
        ))
    }
    /// Like `queue_persisted`, storing a prompt's attachments under `seq` first.
    /// The event is not published if an attachment fails to store.
    fn queue_prompt(
        &self,
        session_id: &str,
        seq: u64,
        attachments: &[AttachmentBlob],
        event: &Event,
    ) -> Persisted {
        if attachments
            .iter()
            .all(|blob| self.record_attachment(session_id, seq, blob))
        {
            self.queue_persisted(session_id, seq, event)
        } else {
            Persisted(Receipt::Ready(false))
        }
    }
    /// Publish an event pumped from a worker, tagged with that worker's generation.
    fn publish_from_worker(&self, session_id: &str, seq: u64, event: &Event, _generation: u64) {
        self.publish(session_id, seq, event);
    }
    /// Drop all stored events for a session (import retry).
    fn clear_session_events(&self, _session_id: &str) {}
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
    fn unresolved_background_task_launches(&self, _session_id: &str) -> Vec<BackgroundTaskLaunch> {
        Vec::new()
    }
    /// The same rows, carrying `output_file` so `Supervisor::attach` can
    /// resume tailing a sub-agent that survived a daemon restart.
    fn unresolved_background_agent_launches(
        &self,
        _session_id: &str,
    ) -> Vec<UnresolvedBackgroundAgentLaunch> {
        Vec::new()
    }
    /// Persist a prompt attachment keyed to its `UserPromptSent` seq.
    fn record_attachment(&self, _session_id: &str, _seq: u64, _blob: &AttachmentBlob) -> bool {
        true
    }
    fn delete_attachments_for_seq(&self, _session_id: &str, _seq: u64) {}
}

/// Whether a queued publication reached the durable store.
#[must_use = "wait for the store, or say why the outcome is not needed"]
pub struct Persisted(Receipt);

enum Receipt {
    Ready(bool),
    Pending(std::sync::mpsc::Receiver<bool>),
}

impl Persisted {
    pub fn wait(self) -> bool {
        match self.0 {
            Receipt::Ready(stored) => stored,
            Receipt::Pending(wait) => blocking_io(|| wait.recv()).unwrap_or(false),
        }
    }
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
}

struct Persister {
    tx: broadcast::Sender<crate::server::AcpBroadcastFrame>,
    event_store: Arc<EventStore>,
    /// Live control-state projection read by prompt dispatch.
    control_cache: Arc<crate::acp::control_cache::ControlStateCache>,
}

struct Publication {
    session_id: String,
    seq: u64,
    event: Event,
    attachments: Vec<AttachmentBlob>,
    worker_generation: Option<u64>,
    persisted: Option<std::sync::mpsc::SyncSender<bool>>,
}

enum Job {
    Publish(Box<Publication>),
    Flush(std::sync::mpsc::SyncSender<()>),
    #[cfg(test)]
    Test(Box<dyn FnOnce() + Send>),
}

impl ChannelSink {
    pub fn new(
        tx: broadcast::Sender<crate::server::AcpBroadcastFrame>,
        event_store: Arc<EventStore>,
        control_cache: Arc<crate::acp::control_cache::ControlStateCache>,
    ) -> Self {
        let persister = Arc::new(Persister {
            tx,
            event_store,
            control_cache,
        });
        let (jobs, queue) = std::sync::mpsc::sync_channel(WRITE_QUEUE_CAPACITY);
        let worker = Arc::clone(&persister);
        let writer = std::thread::Builder::new()
            .name("acp-event-writer".into())
            .spawn(move || {
                for job in queue {
                    // The job moves into the guarded call, so keep what names its gap.
                    let publication = match &job {
                        Job::Publish(p) => Some((p.session_id.clone(), p.seq)),
                        _ => None,
                    };
                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker.run(job)))
                        .is_err()
                    {
                        let (session_id, seq) = publication.unzip();
                        tracing::error!(target: "acp.event_store", session_id, seq, "event writer job panicked; publication may be incomplete, continuing queued jobs");
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
        }
    }

    /// Queue `job` behind earlier publications; run it inline if the writer is gone.
    fn submit(&self, job: Job) {
        use std::sync::mpsc::TrySendError;
        let Some(jobs) = &self.jobs else {
            return self.persister.run(job);
        };
        let job = match jobs.try_send(job) {
            Ok(()) => return,
            Err(TrySendError::Disconnected(job)) => job,
            Err(TrySendError::Full(job)) => {
                tracing::warn!(target: "acp.event_store", "event write queue full; waiting for the writer");
                match blocking_io(|| jobs.send(job)) {
                    Ok(()) => return,
                    Err(std::sync::mpsc::SendError(job)) => job,
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
    pub fn has_in_flight_turn(&self, session_id: &str) -> bool {
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
            let _ = writer.join();
        }
    }
}

impl BroadcastSink for ChannelSink {
    fn publish(&self, session_id: &str, seq: u64, event: &Event) {
        let _ = self.publish_persisted(session_id, seq, event);
    }

    /// Queued, not awaited: the drain reading the worker's socket must not
    /// wait on SQLite.
    fn publish_from_worker(&self, session_id: &str, seq: u64, event: &Event, generation: u64) {
        self.submit(Job::Publish(Box::new(Publication {
            session_id: session_id.to_string(),
            seq,
            event: event.clone(),
            attachments: Vec::new(),
            worker_generation: Some(generation),
            persisted: None,
        })));
    }

    fn clear_session_events(&self, session_id: &str) {
        self.flushed_store().delete_session(session_id);
        self.persister.control_cache.forget(session_id);
    }

    fn publish_persisted(&self, session_id: &str, seq: u64, event: &Event) -> bool {
        self.queue_persisted(session_id, seq, event).wait()
    }

    fn queue_persisted(&self, session_id: &str, seq: u64, event: &Event) -> Persisted {
        self.queue_prompt(session_id, seq, &[], event)
    }

    fn queue_prompt(
        &self,
        session_id: &str,
        seq: u64,
        attachments: &[AttachmentBlob],
        event: &Event,
    ) -> Persisted {
        let (done, wait) = std::sync::mpsc::sync_channel(1);
        self.submit(Job::Publish(Box::new(Publication {
            session_id: session_id.to_string(),
            seq,
            event: event.clone(),
            attachments: attachments.to_vec(),
            worker_generation: None,
            persisted: Some(done),
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

    fn unresolved_background_task_launches(&self, session_id: &str) -> Vec<BackgroundTaskLaunch> {
        self.flushed_store()
            .unresolved_background_task_launches(session_id)
    }

    fn unresolved_background_agent_launches(
        &self,
        session_id: &str,
    ) -> Vec<UnresolvedBackgroundAgentLaunch> {
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

/// Reset time a fresh rejection inherits from the previous one, while still ahead of `now`.
fn inheritable_rate_limit_reset(
    previous: Option<RateLimitInfo>,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<chrono::DateTime<chrono::Utc>> {
    previous?.resets_at.filter(|resets_at| *resets_at > now)
}

impl Persister {
    fn run(&self, job: Job) {
        match job {
            Job::Publish(publication) => {
                let Publication {
                    session_id,
                    seq,
                    event,
                    attachments,
                    worker_generation,
                    persisted,
                } = *publication;
                let stored = attachments
                    .iter()
                    .all(|blob| self.event_store.record_attachment(&session_id, seq, blob))
                    && self.publish_tagged(&session_id, seq, &event, worker_generation);
                if let Some(persisted) = persisted {
                    let _ = persisted.send(stored);
                }
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
        event: &Event,
        worker_generation: Option<u64>,
    ) -> bool {
        let inherited = match event {
            Event::RateLimit { info } if info.resets_at.is_none() => inheritable_rate_limit_reset(
                self.event_store
                    .latest_rate_limit_event(session_id)
                    .map(|(previous, _)| previous),
                chrono::Utc::now(),
            )
            .map(|resets_at| Event::RateLimit {
                info: RateLimitInfo {
                    resets_at: Some(resets_at),
                    ..info.clone()
                },
            }),
            _ => None,
        };
        let event = inherited.as_ref().unwrap_or(event);
        let failure;
        let (event, persisted) = match blocking_io(|| {
            self.event_store.record(session_id, seq, event)
        }) {
            Ok(()) => (event, true),
            Err(e) => {
                tracing::warn!(
                    target: "acp.event_store",
                    session = %session_id,
                    seq,
                    "event store write failed; substituting AgentStartupError so the gap is visible: {e}"
                );
                failure = Event::AgentStartupError {
                    message: format!("event store write failed at seq {seq}: {e}"),
                };
                (&failure, false)
            }
        };

        // Fold before broadcasting, in the same seq order the log is written in.
        let sidebar_status = if persisted {
            self.control_cache.apply_if_cached(session_id, seq, event)
        } else {
            self.control_cache.forget(session_id);
            None
        };
        let _ = self.tx.send(crate::server::AcpBroadcastFrame {
            session_id: session_id.to_string(),
            seq,
            event: Arc::new(event.clone()),
            worker_generation,
            sidebar_status,
        });
        persisted
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
        for seq in 1..=2 {
            sink.publish_from_worker("writer-panic", seq, &Event::ThinkingStarted, 7);
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
            .into_iter()
            .map(|(seq, _)| seq)
            .collect();
        assert_eq!(seqs, [1, 2]);
        for seq in 1..=2 {
            let frame = rx.try_recv().unwrap();
            assert_eq!(frame.seq, seq);
            assert_eq!(frame.worker_generation, Some(7));
        }
        assert!(sink.publish_persisted("writer-panic", 3, &Event::ThinkingStarted));
        assert_eq!(store.highest_seq("writer-panic"), 3);
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

    /// A sync publisher must wait for a stalled save outside the session's seq
    /// lock, or the worker drain blocks on that lock and stops reading (#5251).
    #[test]
    fn a_stalled_sync_publish_leaves_the_seq_lock_to_the_drain() {
        use std::time::{Duration, Instant};

        type Publisher = fn(&super::super::Supervisor<ChannelSink>) -> bool;
        let cases: [(&str, Publisher); 2] = [
            ("event", |sup| {
                sup.publish_next("stalled", &Event::ThinkingStarted);
                true
            }),
            ("prompt with an attachment", |sup| {
                let blob = AttachmentBlob {
                    id: "a-1".into(),
                    kind: crate::daemon::PromptAttachmentKind::Image,
                    mime_type: "image/png".into(),
                    name: None,
                    data: vec![1, 2, 3],
                };
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap();
                let published = runtime.block_on(sup.publish_user_prompt_with_attachments(
                    "stalled",
                    "look".into(),
                    &[blob],
                    None,
                    false,
                    None,
                ));
                published.seq == Some(1)
            }),
        ];
        for (name, publish) in cases {
            let (sink, store, _rx, _tmp) = channel_sink();
            let sup = Arc::new(super::super::Supervisor::new(Arc::clone(&sink)));
            let release = stall_writer(&sink);
            let publisher = {
                let sup = Arc::clone(&sup);
                std::thread::spawn(move || publish(&sup))
            };
            let counter = super::super::session_seq(&sup.next_seqs, "stalled");
            let deadline = Instant::now() + Duration::from_secs(5);
            while !counter.try_lock().is_ok_and(|seq| *seq == 1) {
                assert!(
                    Instant::now() < deadline,
                    "{name}: the seq lock stayed held while the publisher waited for the writer"
                );
                std::thread::yield_now();
            }
            // What the drain does for each worker frame.
            let drained = super::super::publish_with_next_seq(&sup.next_seqs, "stalled", |seq| {
                sink.publish_from_worker("stalled", seq, &Event::ThinkingEnded, 1);
                seq
            });
            assert_eq!(drained, 2, "{name}");
            assert!(
                !publisher.is_finished(),
                "{name}: the save is still stalled"
            );
            release.send(()).unwrap();
            assert!(publisher.join().unwrap(), "{name}: publish was not stored");
            sink.flush();
            let stored: Vec<_> = store
                .replay_from("stalled", 0)
                .into_iter()
                .map(|(seq, _)| seq)
                .collect();
            assert_eq!(stored, [1, 2], "{name}");
        }
    }

    #[test]
    fn dropping_the_sink_stores_queued_publications() {
        use std::time::{Duration, Instant};

        let (sink, store, _rx, _tmp) = channel_sink();
        let release = stall_writer(&sink);
        sink.publish_from_worker("shutdown", 1, &Event::ThinkingStarted, 1);
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
                store.highest_seq("shutdown"),
                1,
                "drop returned before the queued save"
            );
        }
        release.send(()).unwrap();
        dropper.join().unwrap();
        assert_eq!(store.highest_seq("shutdown"), 1);
    }

    #[test]
    fn inheritable_reset_takes_only_a_future_previous_reset() {
        let now = chrono::DateTime::from_timestamp(1_800_000_000, 0).expect("now");
        let future = now + chrono::Duration::hours(2);
        let info = |resets_at| RateLimitInfo {
            status: "usage limit reached".into(),
            resets_at,
            kind: "rate_limit".into(),
        };
        assert_eq!(
            inheritable_rate_limit_reset(Some(info(Some(future))), now),
            Some(future)
        );
        assert_eq!(
            inheritable_rate_limit_reset(Some(info(Some(now - chrono::Duration::minutes(1)))), now),
            None
        );
        assert_eq!(inheritable_rate_limit_reset(Some(info(None)), now), None);
        assert_eq!(inheritable_rate_limit_reset(None, now), None);
    }

    #[tokio::test]
    async fn channel_sink_inherits_a_missing_rate_limit_reset() {
        let (sink, event_store, _rx, _tmp) = channel_sink();
        let resets_at = chrono::Utc::now() + chrono::Duration::hours(3);
        let info = |resets_at| RateLimitInfo {
            status: "usage limit reached".into(),
            resets_at,
            kind: "rate_limit".into(),
        };
        sink.publish(
            "s-rl",
            1,
            &Event::RateLimit {
                info: info(Some(resets_at)),
            },
        );
        sink.publish("s-rl", 2, &Event::RateLimit { info: info(None) });

        let stored = event_store.replay_from("s-rl", 1);
        let Some((_, Event::RateLimit { info: stored_info })) = stored.last() else {
            panic!("expected a stored RateLimit at seq 2, got {stored:?}");
        };
        assert_eq!(stored_info.resets_at, Some(resets_at));
    }
}
