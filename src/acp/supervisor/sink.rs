//! Where supervisor events go: the durable event store and the live broadcast.

use std::sync::Arc;

use tokio::sync::broadcast;

use crate::acp::approvals::Nonce;
use crate::acp::event_store::{AttachmentBlob, EventStore, UnresolvedBackgroundAgentLaunch};
use crate::acp::state::{Event, RateLimitInfo};

/// Destination for published events; test sinks override only what they observe.
pub trait BroadcastSink: Send + Sync + 'static {
    fn publish(&self, session_id: &str, seq: u64, event: &Event);
    /// Like `publish`, but reports whether the event reached the durable store.
    fn publish_persisted(&self, session_id: &str, seq: u64, event: &Event) -> bool {
        self.publish(session_id, seq, event);
        true
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

/// The production sink: persist to the event store, fold the control cache,
/// then broadcast.
pub struct ChannelSink {
    pub tx: broadcast::Sender<crate::server::AcpBroadcastFrame>,
    pub event_store: Arc<EventStore>,
    /// Live control-state projection read by prompt dispatch.
    pub control_cache: Arc<crate::acp::control_cache::ControlStateCache>,
}

impl BroadcastSink for ChannelSink {
    fn publish(&self, session_id: &str, seq: u64, event: &Event) {
        let _ = self.publish_persisted(session_id, seq, event);
    }

    fn publish_from_worker(&self, session_id: &str, seq: u64, event: &Event, generation: u64) {
        self.publish_tagged(session_id, seq, event, Some(generation));
    }

    fn clear_session_events(&self, session_id: &str) {
        self.event_store.delete_session(session_id);
        self.control_cache.forget(session_id);
    }

    fn publish_persisted(&self, session_id: &str, seq: u64, event: &Event) -> bool {
        self.publish_tagged(session_id, seq, event, None)
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

    fn unresolved_background_agent_launches(
        &self,
        session_id: &str,
    ) -> Vec<UnresolvedBackgroundAgentLaunch> {
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
        if persisted {
            self.control_cache.apply_if_cached(session_id, seq, event);
        } else {
            self.control_cache.forget(session_id);
        }
        let _ = self.tx.send(crate::server::AcpBroadcastFrame {
            session_id: session_id.to_string(),
            seq,
            event: Arc::new(event.clone()),
            worker_generation,
        });
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
