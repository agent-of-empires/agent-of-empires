//! Where supervisor events go: the durable event store and the live broadcast.

use std::sync::Arc;

use tokio::sync::broadcast;

use crate::acp::approvals::Nonce;
use crate::acp::event_sender::{AcpEventData, EventMetadata, EventScope};
use crate::acp::event_store::{
    AttachmentBlob, EventStore, RecordDisposition, UnresolvedBackgroundAgentLaunch,
};
use crate::acp::state::{BackgroundTaskLaunch, Event, RateLimitInfo};

/// Destination for published events; test sinks override only what they observe.
pub trait BroadcastSink: Send + Sync + 'static {
    fn publish(&self, session_id: &str, seq: u64, data: &AcpEventData);
    /// Like `publish`, but reports whether the event reached the durable store.
    fn publish_persisted(&self, session_id: &str, seq: u64, data: &AcpEventData) -> bool {
        self.publish(session_id, seq, data);
        true
    }
    /// Publish an event pumped from a worker, tagged with that worker's generation.
    fn publish_from_worker(
        &self,
        session_id: &str,
        seq: u64,
        data: &AcpEventData,
        _generation: u64,
        _native_subject: Option<crate::acp::state::NativeEventSubject>,
    ) -> bool {
        self.publish_persisted(session_id, seq, data)
    }
    /// A sink without the actual journal cannot certify original replay.
    fn publish_replay_from_worker(
        &self,
        session_id: &str,
        seq: u64,
        data: &AcpEventData,
        generation: u64,
        native_subject: Option<crate::acp::state::NativeEventSubject>,
        replay: (u64, &mut crate::acp::event_store::ReplayCommit),
    ) -> bool {
        replay.1.reject();
        self.publish_from_worker(session_id, seq, data, generation, native_subject);
        false
    }

    fn establishment_store(&self) -> Option<Arc<EventStore>> {
        None
    }
    /// Erase durable session projections without resetting the publication counter.
    fn clear_session_events(&self, _session_id: &str) -> anyhow::Result<()> {
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
        true
    }
    fn delete_attachments_for_seq(&self, _session_id: &str, _seq: u64) {}
}

/// The production sink: persist to the event store, fold the control cache,
/// then broadcast.
pub struct ChannelSink {
    pub tx: broadcast::Sender<crate::server::AcpBroadcastFrame>,
    pub event_store: Arc<EventStore>,
}

impl BroadcastSink for ChannelSink {
    fn publish(&self, session_id: &str, seq: u64, data: &AcpEventData) {
        let _ = self.publish_persisted(session_id, seq, data);
    }

    fn publish_from_worker(
        &self,
        session_id: &str,
        seq: u64,
        data: &AcpEventData,
        generation: u64,
        native_subject: Option<crate::acp::state::NativeEventSubject>,
    ) -> bool {
        self.publish_tagged(
            session_id,
            seq,
            data,
            Some(generation),
            native_subject,
            None,
        )
    }

    fn publish_replay_from_worker(
        &self,
        session_id: &str,
        seq: u64,
        data: &AcpEventData,
        generation: u64,
        native_subject: Option<crate::acp::state::NativeEventSubject>,
        replay: (u64, &mut crate::acp::event_store::ReplayCommit),
    ) -> bool {
        self.publish_tagged(
            session_id,
            seq,
            data,
            Some(generation),
            native_subject,
            Some(replay),
        )
    }

    fn establishment_store(&self) -> Option<Arc<EventStore>> {
        Some(Arc::clone(&self.event_store))
    }

    fn clear_session_events(&self, session_id: &str) -> anyhow::Result<()> {
        self.event_store.delete_session_checked(session_id)
    }

    fn unsuccessful_main(
        &self,
        session_id: &str,
    ) -> anyhow::Result<crate::acp::event_store::ControlKnowledge<bool>> {
        self.event_store.has_unsuccessful_main_outcome(session_id)
    }

    fn publish_persisted(&self, session_id: &str, seq: u64, data: &AcpEventData) -> bool {
        self.publish_tagged(session_id, seq, data, None, None, None)
    }

    fn unresolved_approval_nonces(&self, session_id: &str) -> Vec<Nonce> {
        self.event_store.unresolved_approval_nonces(session_id)
    }

    fn unresolved_elicitation_nonces(&self, session_id: &str) -> Vec<Nonce> {
        self.event_store.unresolved_elicitation_nonces(session_id)
    }

    fn unresolved_background_agent_ids(&self, session_id: &str) -> Vec<String> {
        self.event_store.unresolved_background_agent_ids(session_id)
    }

    fn unresolved_background_task_ids(&self, session_id: &str) -> Vec<String> {
        self.event_store.unresolved_background_task_ids(session_id)
    }

    fn unresolved_background_task_launches(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Vec<BackgroundTaskLaunch>> {
        self.event_store
            .unresolved_background_task_launches(session_id)
    }

    fn unresolved_background_agent_launches(
        &self,
        session_id: &str,
    ) -> anyhow::Result<Vec<UnresolvedBackgroundAgentLaunch>> {
        self.event_store
            .unresolved_background_agent_launches(session_id)
    }

    fn record_attachment(&self, session_id: &str, seq: u64, blob: &AttachmentBlob) -> bool {
        blocking_io(|| self.event_store.record_attachment(session_id, seq, blob))
    }

    fn delete_attachments_for_seq(&self, session_id: &str, seq: u64) {
        blocking_io(|| self.event_store.delete_attachments_for_seq(session_id, seq))
    }
}

/// Run store I/O via `block_in_place` on a multi-thread runtime, inline elsewhere.
fn blocking_io<T>(f: impl FnOnce() -> T) -> T {
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

impl ChannelSink {
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

    fn publish_tagged(
        &self,
        session_id: &str,
        seq: u64,
        data: &AcpEventData,
        worker_generation: Option<u64>,
        native_subject: Option<crate::acp::state::NativeEventSubject>,
        replay: Option<(u64, &mut crate::acp::event_store::ReplayCommit)>,
    ) -> bool {
        let inherited = match data.event.as_ref() {
            Event::RateLimit { info }
                if info.resets_at.is_none()
                    && matches!(
                        data.metadata.scope,
                        EventScope::SessionCurrent | EventScope::OperationCurrent
                    ) =>
            {
                inheritable_rate_limit_reset(
                    match self.event_store.latest_rate_limit_event(session_id) {
                        Ok(previous) => previous.map(|(info, _)| info),
                        Err(error) => {
                            tracing::warn!(target: "acp.event_store", session = %session_id,
                                %error, "quota calendar inheritance unavailable");
                            None
                        }
                    },
                    chrono::Utc::now(),
                )
                .map(|resets_at| {
                    Arc::new(Event::RateLimit {
                        info: RateLimitInfo {
                            resets_at: Some(resets_at),
                            ..info.clone()
                        },
                    })
                })
            }
            _ => None,
        };
        let event = inherited.as_ref().unwrap_or(&data.event);
        let metadata = &data.metadata;
        let native_subject = native_subject.or_else(|| event.native_subject());
        let (event, persisted) = match blocking_io(|| match replay {
            Some((ordinal, replay)) => self
                .event_store
                .record_replay(session_id, seq, event, metadata, ordinal, replay),
            None => self.event_store.record(session_id, seq, event, metadata),
        }) {
            Ok(RecordDisposition::Existing { .. }) => {
                self.bind_pending_publication(session_id, seq, data);
                return true;
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
        persisted
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::channel_sink;
    use super::*;

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
        use crate::acp::establishment::NativeOperationCorrelation;
        use crate::acp::event_sender::{AcpEventData, EventMetadata, EventScope};
        use crate::acp::protocol::{
            AcpOperationMetadata, AcpOriginalRequestResult, AcpPromptObservation, AcpPromptWindowId,
        };
        use crate::acp::state::NativeEventOrigin;
        let (sink, event_store, _rx, _tmp) = channel_sink();
        let resets_at = chrono::Utc::now() + chrono::Duration::hours(3);
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
