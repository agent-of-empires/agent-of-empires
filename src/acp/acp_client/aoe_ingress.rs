//! Consumes `_meta.aoe` on inbound `session/update` frames, in arrival order,
//! before anything maps them to events.

use crate::acp::aoe_meta::lease::LeaseTable;
use crate::acp::aoe_meta::ordering::{Arrival, ArrivalOrder};
use crate::acp::aoe_meta::{self, Envelope};
use crate::acp::state::IntegrationLostReason;
use agent_client_protocol::v1::schema::SessionNotification;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// A frame waiting in, or released by, the ordering gate.
struct Frame {
    notification: SessionNotification,
    local_prompt_signals: bool,
    /// The prompt epoch when the frame arrived, not when it is released.
    epoch: u64,
    envelope: Option<Envelope>,
}

pub(super) struct Delivered {
    pub(super) notification: SessionNotification,
    pub(super) local_prompt_signals: bool,
    pub(super) epoch: u64,
}

pub(super) enum Ingested {
    /// Frames to process now, in order.
    Deliver(Vec<Delivered>),
    /// A proxied agent broke the contract: stop interpreting its frames.
    Lost(IntegrationLostReason),
}

/// Strictness depends on the hello: an agent that proved it is proxied is held
/// to the contract, any other agent's malformed `_meta.aoe` is ignored.
#[derive(Default)]
pub(super) struct AoeIngress {
    pub(super) leases: Arc<LeaseTable>,
    proxied: AtomicBool,
    lost: AtomicBool,
    order: Mutex<ArrivalOrder<Frame>>,
}

impl AoeIngress {
    /// Adapter frames are numbered per connection, so each starts clean.
    pub(super) fn begin_connection(&self, proxied: bool) {
        self.proxied.store(proxied, Ordering::Relaxed);
        self.lost.store(false, Ordering::Relaxed);
        self.leases.clear();
        *self.order.lock().expect("aoe ordering mutex poisoned") = ArrivalOrder::default();
    }

    /// A reset commit starts a new native session: its keys are fresh, frames
    /// held for the old one are dropped, and a lost integration is restored,
    /// matching the reducer clearing `integration_lost` on the new session.
    pub(super) fn session_reset(&self) {
        self.lost.store(false, Ordering::Relaxed);
        self.order
            .lock()
            .expect("aoe ordering mutex poisoned")
            .session_reset();
    }

    pub(super) fn ingest(
        &self,
        notification: SessionNotification,
        local_prompt_signals: bool,
        epoch: u64,
        now_ms: i64,
    ) -> Ingested {
        if self.lost.load(Ordering::Relaxed) {
            return Ingested::Deliver(Vec::new());
        }
        let proxied = self.proxied.load(Ordering::Relaxed);
        let envelope = match serde_json::to_value(&notification.update)
            .map_err(|e| e.to_string())
            .and_then(|v| aoe_meta::from_update_value(&v).map_err(|e| e.to_string()))
        {
            Ok(envelope) => envelope,
            Err(detail) if proxied => {
                return self.lose(IntegrationLostReason::ContractViolation { detail });
            }
            Err(_) => None,
        };
        let arrival = Arrival {
            key: envelope.as_ref().map(|e| e.key.as_str()),
            after_adapter_sequence: envelope.as_ref().and_then(|e| e.after_adapter_sequence),
        };
        let frame = Frame {
            notification,
            local_prompt_signals,
            epoch,
            envelope: envelope.clone(),
        };
        let mut order = self.order.lock().expect("aoe ordering mutex poisoned");
        // An agent that is not proxied has nothing to anchor to: past the
        // bound its frame is delivered unordered rather than dropped.
        if arrival.after_adapter_sequence.is_some() && order.held_full() && !proxied {
            drop(order);
            return Ingested::Deliver(vec![Delivered {
                notification: frame.notification,
                local_prompt_signals,
                epoch,
            }]);
        }
        let released = order.arrive(arrival, frame);
        drop(order);
        let Ok(released) = released else {
            return self.lose(IntegrationLostReason::HoldOverflow);
        };
        let mut deliver = Vec::with_capacity(released.len());
        for frame in released {
            if let Some(lease) = frame
                .envelope
                .as_ref()
                .and_then(|e| e.lease.as_ref())
                .filter(|_| frame.local_prompt_signals)
            {
                if self.leases.apply(lease, now_ms).is_err() && proxied {
                    return self.lose(IntegrationLostReason::LeaseOverflow);
                }
            }
            if frame.envelope.as_ref().and_then(|e| e.ephemeral) == Some(true) {
                continue;
            }
            deliver.push(Delivered {
                notification: frame.notification,
                local_prompt_signals: frame.local_prompt_signals,
                epoch: frame.epoch,
            });
        }
        Ingested::Deliver(deliver)
    }

    fn lose(&self, reason: IntegrationLostReason) -> Ingested {
        self.lost.store(true, Ordering::Relaxed);
        Ingested::Lost(reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::v1::schema::{SessionUpdate, ToolCall};
    use serde_json::json;

    fn frame(id: &str, aoe: serde_json::Value) -> SessionNotification {
        let mut tc = ToolCall::new(id.to_string(), "T");
        if !aoe.is_null() {
            tc.meta = json!({ "aoe": aoe }).as_object().cloned();
        }
        SessionNotification::new("s", SessionUpdate::ToolCall(tc))
    }

    fn ids(ingested: Ingested) -> Vec<String> {
        let Ingested::Deliver(frames) = ingested else {
            panic!("expected delivery");
        };
        frames
            .into_iter()
            .map(|d| match d.notification.update {
                SessionUpdate::ToolCall(tc) => tc.tool_call_id.0.to_string(),
                _ => unreachable!(),
            })
            .collect()
    }

    fn ingress(proxied: bool) -> AoeIngress {
        let i = AoeIngress::default();
        i.begin_connection(proxied);
        i
    }

    #[test]
    fn an_expired_lease_clears_busy_with_no_further_frames() {
        let i = ingress(true);
        let lease = json!({"v":1,"key":"l1","lease":{"id":"bg","kind":"busy","ttl_ms":1000}});
        assert_eq!(ids(i.ingest(frame("t1", lease), true, 0, 0)), ["t1"]);
        assert!(i.leases.working(999));
        assert!(!i.leases.working(1000));
    }

    #[test]
    fn replayed_frames_do_not_hold_leases() {
        let i = ingress(true);
        let lease = json!({"v":1,"key":"l1","lease":{"id":"bg","kind":"busy","ttl_ms":1000}});
        ids(i.ingest(frame("t1", lease), false, 0, 0));
        assert!(!i.leases.working(1));
    }

    #[test]
    fn contract_breaks_end_a_proxied_integration_only() {
        let broken = || frame("t", json!({"v": 1}));
        // (name, proxied, expected)
        for (name, proxied) in [("proxied", true), ("plain agent", false)] {
            let i = ingress(proxied);
            let out = i.ingest(broken(), true, 0, 0);
            match (proxied, out) {
                (true, Ingested::Lost(IntegrationLostReason::ContractViolation { .. })) => {
                    // Once lost, nothing else is interpreted.
                    assert!(ids(i.ingest(frame("after", json!(null)), true, 0, 0)).is_empty());
                }
                (false, Ingested::Deliver(frames)) => assert_eq!(frames.len(), 1, "{name}"),
                _ => panic!("{name}: unexpected outcome"),
            }
        }
    }

    #[test]
    fn hold_overflow_ends_a_proxied_integration() {
        use crate::acp::aoe_meta::ordering::MAX_HELD;
        let i = ingress(true);
        for n in 0..MAX_HELD {
            let aoe = json!({"v":1,"key":format!("k{n}"),"after_adapter_sequence":u64::MAX});
            assert!(ids(i.ingest(frame("h", aoe), true, 0, 0)).is_empty());
        }
        let aoe = json!({"v":1,"key":"over","after_adapter_sequence":u64::MAX});
        assert!(matches!(
            i.ingest(frame("h", aoe), true, 0, 0),
            Ingested::Lost(IntegrationLostReason::HoldOverflow)
        ));
    }

    #[test]
    fn a_held_frame_keeps_the_epoch_it_arrived_in() {
        let i = ingress(true);
        let held = json!({"v":1,"key":"k","after_adapter_sequence":1});
        assert!(ids(i.ingest(frame("t", held), true, 7, 0)).is_empty());
        let Ingested::Deliver(out) = i.ingest(frame("a", json!(null)), true, 9, 0) else {
            panic!("expected delivery");
        };
        assert_eq!(out.iter().map(|d| d.epoch).collect::<Vec<_>>(), [9, 7]);
    }

    #[test]
    fn a_plain_agent_past_the_hold_bound_is_delivered_not_dropped() {
        use crate::acp::aoe_meta::ordering::MAX_HELD;
        let i = ingress(false);
        for n in 0..MAX_HELD {
            let aoe = json!({"v":1,"key":format!("k{n}"),"after_adapter_sequence":u64::MAX});
            assert!(ids(i.ingest(frame("h", aoe), true, 0, 0)).is_empty());
        }
        let aoe = json!({"v":1,"key":"over","after_adapter_sequence":u64::MAX});
        assert_eq!(ids(i.ingest(frame("over", aoe), true, 0, 0)), ["over"]);
    }

    #[test]
    fn a_session_reset_forgets_keys_and_held_frames() {
        let i = ingress(true);
        let held = json!({"v":1,"key":"old","after_adapter_sequence":9});
        assert!(ids(i.ingest(frame("old", held), true, 0, 0)).is_empty());
        let keyed = json!({"v":1,"key":"k"});
        assert_eq!(ids(i.ingest(frame("a", keyed.clone()), true, 0, 0)), ["a"]);
        i.session_reset();
        assert_eq!(
            ids(i.ingest(frame("b", keyed), true, 0, 0)),
            ["b"],
            "key reusable"
        );
    }

    #[test]
    fn a_session_reset_restores_a_lost_integration() {
        let i = ingress(true);
        assert!(matches!(
            i.ingest(frame("bad", json!({"v": 1})), true, 0, 0),
            Ingested::Lost(_)
        ));
        assert!(ids(i.ingest(frame("a", json!(null)), true, 0, 0)).is_empty());
        i.session_reset();
        assert_eq!(ids(i.ingest(frame("b", json!(null)), true, 0, 0)), ["b"]);
    }
}
