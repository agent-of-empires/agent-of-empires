//! Consumes `_meta.aoe` on inbound `session/update` frames, in arrival order,
//! before anything maps them to events.

use crate::acp::aoe_meta::lease::{LeaseNamespace, LeaseTable, LeaseTableFull};
use crate::acp::aoe_meta::ordering::{Arrival, ArrivalOrder};
use crate::acp::aoe_meta::{self, Envelope};
use crate::acp::state::{Event, IntegrationLostReason};
use agent_client_protocol::v1::schema::SessionNotification;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// A frame waiting in, or released by, the ordering gate.
struct Frame {
    notification: SessionNotification,
    local_prompt_signals: bool,
    /// Whether its lease may be taken: live, not replayed history. Decided on
    /// arrival, as a held frame is released after the load ends.
    admit_leases: bool,
    /// The prompt epoch when the frame arrived, not when it is released.
    epoch: u64,
    envelope: Option<Envelope>,
}

pub(super) struct Delivered {
    pub(super) notification: SessionNotification,
    pub(super) local_prompt_signals: bool,
    pub(super) epoch: u64,
    /// The envelope, when its lifecycle primitives may be applied: live, not
    /// replayed history.
    pub(super) lifecycle: Option<Envelope>,
}

pub(super) enum Ingested {
    /// Frames to process now, in order.
    Deliver(Vec<Delivered>),
    /// A proxied agent broke the contract: stop interpreting its frames.
    Lost(IntegrationLostReason),
    /// The break came partway through a released batch: the frames before it
    /// arrived first, so they are processed before the loss.
    DeliverThenLost(Vec<Delivered>, IntegrationLostReason),
}

/// Strictness depends on the hello: an agent that proved it is proxied is held
/// to the contract, any other agent's malformed `_meta.aoe` is ignored.
#[derive(Default)]
pub(super) struct AoeIngress {
    pub(super) leases: Arc<LeaseTable>,
    proxied: AtomicBool,
    lost: AtomicBool,
    /// A `session/load` is replaying stored history, which holds no lease.
    loading_history: AtomicBool,
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

    /// Whether this connection's agent sent a valid hello.
    pub(super) fn is_proxied(&self) -> bool {
        self.proxied.load(Ordering::Relaxed)
    }

    pub(super) fn set_loading_history(&self, loading: bool) {
        self.loading_history.store(loading, Ordering::Relaxed);
    }

    /// A reset commit starts a new native session: its keys are fresh, frames
    /// held for the old one are dropped, its leases are released, and a lost
    /// integration is restored, matching the reducer clearing
    /// `integration_lost` on the new session. A lease acquired after the reset
    /// gate checked and before the commit must not hold the new session.
    pub(super) fn session_reset(&self) {
        self.lost.store(false, Ordering::Relaxed);
        self.leases.clear();
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
        let update = serde_json::to_value(&notification.update);
        // A tailer frame carries an anchor even when its envelope is invalid,
        // and takes no adapter frame number.
        let is_adapter_frame = !update.as_ref().is_ok_and(aoe_meta::is_anchored);
        if self.lost.load(Ordering::Relaxed) {
            // Nothing is delivered, but the proxy keeps numbering adapter
            // frames, so the count must too or anchors lag after recovery.
            if is_adapter_frame {
                self.count_adapter_frame();
            }
            return Ingested::Deliver(Vec::new());
        }
        let proxied = self.proxied.load(Ordering::Relaxed);
        let envelope = match update
            .as_ref()
            .map_err(|e| e.to_string())
            .and_then(|v| aoe_meta::from_update_value(v).map_err(|e| e.to_string()))
        {
            Ok(envelope) => envelope,
            Err(detail) if proxied => {
                if is_adapter_frame {
                    self.count_adapter_frame();
                }
                return self.lose(IntegrationLostReason::ContractViolation { detail });
            }
            // Invalid but anchored: a tailer frame, takes no number.
            Err(_) if !is_adapter_frame => {
                return Ingested::Deliver(vec![Delivered {
                    notification,
                    local_prompt_signals,
                    epoch,
                    lifecycle: None,
                }]);
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
            admit_leases: local_prompt_signals && !self.loading_history.load(Ordering::Relaxed),
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
                lifecycle: None,
            }]);
        }
        let released = order.arrive(arrival, frame);
        drop(order);
        let Ok(released) = released else {
            return self.lose(IntegrationLostReason::HoldOverflow);
        };
        self.deliver(released, now_ms)
    }

    fn deliver(&self, released: Vec<Frame>, now_ms: i64) -> Ingested {
        let proxied = self.proxied.load(Ordering::Relaxed);
        let mut deliver = Vec::with_capacity(released.len());
        for frame in released {
            if let Some(envelope) = frame.envelope.as_ref().filter(|_| frame.admit_leases) {
                if self.apply_leases(envelope, now_ms).is_err() && proxied {
                    return match self.lose(IntegrationLostReason::LeaseOverflow) {
                        Ingested::Lost(reason) if !deliver.is_empty() => {
                            Ingested::DeliverThenLost(deliver, reason)
                        }
                        lost => lost,
                    };
                }
            }
            if frame.envelope.as_ref().and_then(|e| e.ephemeral) == Some(true) {
                continue;
            }
            deliver.push(Delivered {
                notification: frame.notification,
                local_prompt_signals: frame.local_prompt_signals,
                epoch: frame.epoch,
                lifecycle: frame.envelope.filter(|_| frame.admit_leases),
            });
        }
        Ingested::Deliver(deliver)
    }

    /// Finishes the tasks and runs whose lease expired with no `ended`.
    pub(super) fn lapsed_events(&self, now_ms: i64) -> Vec<Event> {
        self.leases
            .drain_expired(now_ms)
            .into_iter()
            .filter_map(|(ns, id)| match ns {
                LeaseNamespace::BackgroundTask => Some(Event::BackgroundTaskFinished {
                    task_id: id,
                    ended_at: None,
                }),
                LeaseNamespace::WorkflowRun => Some(Event::WorkflowRunFinished { run_id: id }),
                LeaseNamespace::Explicit => None,
            })
            .collect()
    }

    /// The explicit lease, then the task leases a background task or workflow
    /// run derives.
    fn apply_leases(&self, envelope: &Envelope, now_ms: i64) -> Result<(), LeaseTableFull> {
        if let Some(lease) = &envelope.lease {
            self.leases.apply(lease, now_ms)?;
        }
        if let Some(task) = &envelope.background_task {
            let ns = LeaseNamespace::BackgroundTask;
            self.leases
                .apply_derived(ns, &task.id, task.state, task.ttl_ms, now_ms)?;
        }
        if let Some(run) = &envelope.workflow_run {
            let ns = LeaseNamespace::WorkflowRun;
            self.leases
                .apply_derived(ns, &run.id, run.state, run.ttl_ms, now_ms)?;
        }
        Ok(())
    }

    /// Numbers `n` frames the session identity discarded before they got
    /// here, and delivers the held frames that releases.
    pub(super) fn skip_frames(&self, n: u64, now_ms: i64) -> Ingested {
        let mut order = self.order.lock().expect("aoe ordering mutex poisoned");
        let released = order.skip_frames(n);
        drop(order);
        if self.lost.load(Ordering::Relaxed) {
            return Ingested::Deliver(Vec::new());
        }
        self.deliver(released, now_ms)
    }

    fn count_adapter_frame(&self) {
        self.order
            .lock()
            .expect("aoe ordering mutex poisoned")
            .count_frame();
    }

    fn lose(&self, reason: IntegrationLostReason) -> Ingested {
        self.lost.store(true, Ordering::Relaxed);
        // Later frames are swallowed, so no release can arrive.
        self.leases.clear();
        Ingested::Lost(reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::acp_client::update_events::lifecycle_events;
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
        let lease = json!({"v":2,"key":"l1","lease":{"id":"bg","kind":"busy","ttl_ms":1000}});
        assert_eq!(ids(i.ingest(frame("t1", lease), true, 0, 0)), ["t1"]);
        assert!(i.leases.working(999));
        assert!(!i.leases.working(1000));
    }

    #[test]
    fn a_background_task_holds_working_until_it_ends_or_expires() {
        let i = ingress(true);
        let task = |key: &str, state: &str, ttl: Option<u64>| {
            let mut t = json!({"id":"b1","state":state});
            if let Some(ttl) = ttl {
                t["ttl_ms"] = json!(ttl);
            }
            json!({"v":2,"key":key,"background_task":t})
        };
        ids(i.ingest(frame("a", task("k1", "started", Some(1000))), true, 0, 0));
        assert!(i.leases.working(999));
        assert!(!i.leases.working(1000), "expires with no further frames");

        ids(i.ingest(frame("b", task("k2", "started", Some(1000))), true, 0, 2000));
        ids(i.ingest(frame("c", task("k3", "started", Some(1000))), true, 0, 2900));
        assert!(i.leases.working(3800), "a repeated started renews");
        ids(i.ingest(frame("d", task("k4", "ended", None)), true, 0, 3000));
        assert!(!i.leases.working(3001), "ended releases at once");
    }

    #[test]
    fn an_expired_task_and_run_finish_and_clear_state() {
        use crate::acp::state::AcpState;
        let i = ingress(true);
        let aoe = json!({"v":2,"key":"k1","background_task":{"id":"b","state":"started","ttl_ms":1000},
            "workflow_run":{"id":"w","state":"started","ttl_ms":1000}});
        let Ingested::Deliver(frames) = i.ingest(frame("a", aoe), true, 0, 0) else {
            panic!("expected delivery");
        };
        let mut state = AcpState::default();
        let mut seq = 0;
        let mut apply = |event| {
            seq += 1;
            state.apply_event(seq, event).unwrap();
        };
        for event in frames
            .iter()
            .flat_map(|d| d.lifecycle.iter().flat_map(lifecycle_events))
        {
            apply(event);
        }
        assert!(i.lapsed_events(999).is_empty());
        let finished = i.lapsed_events(1000);
        assert_eq!(finished.len(), 2);
        for event in finished {
            apply(event);
        }
        assert!(i.lapsed_events(2000).is_empty(), "reported once");
        assert!(state.background_tasks.is_empty());
        assert!(!state.has_active_background_agent());
    }

    #[test]
    fn replayed_frames_leave_nothing_to_expire() {
        let i = ingress(true);
        let aoe =
            json!({"v":2,"key":"k1","background_task":{"id":"b","state":"started","ttl_ms":10}});
        ids(i.ingest(frame("a", aoe), false, 0, 0));
        assert!(i.lapsed_events(1000).is_empty());
    }

    #[test]
    fn a_derived_lease_does_not_release_an_explicit_one_of_the_same_id() {
        let i = ingress(true);
        let busy = json!({"v":2,"key":"l1","lease":{"id":"x","kind":"busy","ttl_ms":60000}});
        ids(i.ingest(frame("a", busy), true, 0, 0));
        let ended = json!({"v":2,"key":"k2","background_task":{"id":"x","state":"ended"}});
        ids(i.ingest(frame("b", ended), true, 0, 1));
        assert!(i.leases.working(2), "the explicit lease stays");
    }

    #[test]
    fn lifecycle_is_delivered_for_live_frames_only() {
        let i = ingress(true);
        let aoe =
            json!({"v":2,"key":"k","background_task":{"id":"b","state":"started","ttl_ms":1000}});
        let lifecycle = |ingested| match ingested {
            Ingested::Deliver(frames) => {
                frames.into_iter().map(|d| d.lifecycle).collect::<Vec<_>>()
            }
            _ => panic!("expected delivery"),
        };
        let live = lifecycle(i.ingest(frame("a", aoe.clone()), true, 0, 0));
        assert_eq!(live.len(), 1);
        assert!(live[0]
            .as_ref()
            .is_some_and(|e| e.background_task.is_some()));

        let aoe =
            json!({"v":2,"key":"k2","background_task":{"id":"b2","state":"started","ttl_ms":1000}});
        let replayed = lifecycle(i.ingest(frame("b", aoe.clone()), false, 0, 0));
        assert!(replayed[0].is_none(), "replay emits no lifecycle");
        i.set_loading_history(true);
        let aoe =
            json!({"v":2,"key":"k3","background_task":{"id":"b3","state":"started","ttl_ms":1000}});
        let loading = lifecycle(i.ingest(frame("c", aoe), true, 0, 0));
        assert!(loading[0].is_none(), "a load emits no lifecycle");
    }

    #[test]
    fn replayed_task_and_run_frames_take_no_lease() {
        let i = ingress(true);
        let task =
            json!({"v":2,"key":"k1","background_task":{"id":"b","state":"started","ttl_ms":1000}});
        let run =
            json!({"v":2,"key":"k2","workflow_run":{"id":"w","state":"started","ttl_ms":1000}});
        ids(i.ingest(frame("a", task), false, 0, 0));
        ids(i.ingest(frame("b", run), false, 0, 0));
        assert!(!i.leases.any_live(1));
    }

    #[test]
    fn derived_leases_share_the_table_bound() {
        use crate::acp::aoe_meta::lease::MAX_LEASES;
        let i = ingress(true);
        for n in 0..MAX_LEASES {
            let run = json!({"v":2,"key":format!("k{n}"),"workflow_run":{"id":format!("w{n}"),"state":"started","ttl_ms":60000}});
            ids(i.ingest(frame("r", run), true, 0, 0));
        }
        let over = json!({"v":2,"key":"over","background_task":{"id":"b","state":"started","ttl_ms":60000}});
        assert!(matches!(
            i.ingest(frame("o", over), true, 0, 0),
            Ingested::Lost(IntegrationLostReason::LeaseOverflow)
        ));
    }

    #[test]
    fn replayed_frames_do_not_hold_leases() {
        let i = ingress(true);
        let lease = json!({"v":2,"key":"l1","lease":{"id":"bg","kind":"busy","ttl_ms":1000}});
        ids(i.ingest(frame("t1", lease), false, 0, 0));
        assert!(!i.leases.working(1));
    }

    #[test]
    fn contract_breaks_end_a_proxied_integration_only() {
        let broken = || frame("t", json!({"v": 2}));
        // (name, proxied)
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
            let aoe = json!({"v":2,"key":format!("k{n}"),"after_adapter_sequence":u64::MAX});
            assert!(ids(i.ingest(frame("h", aoe), true, 0, 0)).is_empty());
        }
        let aoe = json!({"v":2,"key":"over","after_adapter_sequence":u64::MAX});
        assert!(matches!(
            i.ingest(frame("h", aoe), true, 0, 0),
            Ingested::Lost(IntegrationLostReason::HoldOverflow)
        ));
    }

    #[test]
    fn a_held_frame_keeps_the_epoch_it_arrived_in() {
        let i = ingress(true);
        let held = json!({"v":2,"key":"k","after_adapter_sequence":1});
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
            let aoe = json!({"v":2,"key":format!("k{n}"),"after_adapter_sequence":u64::MAX});
            assert!(ids(i.ingest(frame("h", aoe), true, 0, 0)).is_empty());
        }
        let aoe = json!({"v":2,"key":"over","after_adapter_sequence":u64::MAX});
        assert_eq!(ids(i.ingest(frame("over", aoe), true, 0, 0)), ["over"]);
    }

    #[test]
    fn a_session_reset_forgets_keys_and_held_frames() {
        let i = ingress(true);
        let held = json!({"v":2,"key":"old","after_adapter_sequence":9});
        assert!(ids(i.ingest(frame("old", held), true, 0, 0)).is_empty());
        let keyed = json!({"v":2,"key":"k"});
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
            i.ingest(frame("bad", json!({"v": 2})), true, 0, 0),
            Ingested::Lost(_)
        ));
        assert!(ids(i.ingest(frame("a", json!(null)), true, 0, 0)).is_empty());
        i.session_reset();
        assert_eq!(ids(i.ingest(frame("b", json!(null)), true, 0, 0)), ["b"]);
    }

    #[test]
    fn frames_lost_integration_swallows_still_advance_the_adapter_count() {
        let i = ingress(true);
        assert!(matches!(
            i.ingest(frame("bad", json!({"v": 2})), true, 0, 0),
            Ingested::Lost(_)
        ));
        // The bad frame was adapter frame 1; frames 2 and 3 arrive while lost.
        assert!(ids(i.ingest(frame("a", json!(null)), true, 0, 0)).is_empty());
        assert!(ids(i.ingest(frame("b", json!(null)), true, 0, 0)).is_empty());
        i.session_reset();
        let anchored = json!({"v":2,"key":"t","after_adapter_sequence":3});
        assert_eq!(ids(i.ingest(frame("t", anchored), true, 0, 0)), ["t"]);
    }

    #[test]
    fn an_invalid_anchored_frame_takes_no_adapter_frame_number() {
        let i = ingress(true);
        assert_eq!(ids(i.ingest(frame("f1", json!(null)), true, 0, 0)), ["f1"]);
        // Anchored but with an empty key: invalid, yet still a tailer frame.
        let bad = json!({"v":2,"key":"","after_adapter_sequence":9});
        assert!(matches!(
            i.ingest(frame("bad", bad.clone()), true, 0, 0),
            Ingested::Lost(_)
        ));
        // Swallowed while lost: also uncounted.
        assert!(ids(i.ingest(frame("bad2", bad), true, 0, 0)).is_empty());
        i.session_reset();
        let anchored = json!({"v":2,"key":"t","after_adapter_sequence":2});
        assert!(
            ids(i.ingest(frame("t", anchored), true, 0, 0)).is_empty(),
            "held for frame 2"
        );
        assert_eq!(
            ids(i.ingest(frame("f2", json!(null)), true, 0, 0)),
            ["f2", "t"]
        );
    }

    #[test]
    fn a_plain_agent_invalid_anchored_frame_takes_no_adapter_frame_number() {
        let i = ingress(false);
        assert_eq!(ids(i.ingest(frame("f1", json!(null)), true, 0, 0)), ["f1"]);
        let bad = json!({"v":2,"key":"","after_adapter_sequence":9});
        assert_eq!(ids(i.ingest(frame("bad", bad), true, 0, 0)), ["bad"]);
        let anchored = json!({"v":2,"key":"t","after_adapter_sequence":2});
        assert!(
            ids(i.ingest(frame("t", anchored), true, 0, 0)).is_empty(),
            "held for frame 2"
        );
        assert_eq!(
            ids(i.ingest(frame("f2", json!(null)), true, 0, 0)),
            ["f2", "t"]
        );
    }

    #[test]
    fn a_session_reset_releases_leases_taken_before_the_commit() {
        let i = ingress(true);
        let lease = json!({"v":2,"key":"l1","lease":{"id":"bg","kind":"busy","ttl_ms":60000}});
        assert_eq!(ids(i.ingest(frame("t1", lease), true, 0, 0)), ["t1"]);
        assert!(i.leases.working(1) && i.leases.any_live(1));
        i.session_reset();
        assert!(!i.leases.working(1) && !i.leases.any_live(1));
        // The new session can still take its own.
        let again = json!({"v":2,"key":"l2","lease":{"id":"bg2","kind":"task","ttl_ms":60000}});
        ids(i.ingest(frame("t2", again), true, 0, 2));
        assert!(i.leases.working(3));
    }

    #[test]
    fn history_replayed_by_a_load_takes_no_lease() {
        let i = ingress(true);
        let lease = |key: &str, anchor: Option<u64>| {
            let mut aoe = json!({"v":2,"key":key,"lease":{"id":key,"kind":"busy","ttl_ms":60000}});
            if let Some(n) = anchor {
                aoe["after_adapter_sequence"] = json!(n);
            }
            aoe
        };
        i.set_loading_history(true);
        ids(i.ingest(frame("h1", lease("old", None)), true, 0, 0));
        // Held past the end of the load, then released: still history.
        let held = lease("held", Some(3));
        assert!(ids(i.ingest(frame("h2", held), true, 0, 0)).is_empty());
        i.set_loading_history(false);
        for n in 0..2 {
            ids(i.ingest(frame("f", json!(null)), true, 0, n));
        }
        assert!(!i.leases.any_live(1), "no historical lease is taken");
        ids(i.ingest(frame("live", lease("new", None)), true, 0, 1));
        assert!(i.leases.working(2), "a live lease after the load applies");
    }

    #[test]
    fn frames_released_before_a_lease_overflow_are_still_delivered() {
        use crate::acp::aoe_meta::lease::MAX_LEASES;
        let i = ingress(true);
        let lease = |key: String, anchor: Option<u64>| {
            let mut aoe = json!({"v":2,"key":key,"lease":{"id":key,"kind":"task","ttl_ms":60000}});
            if let Some(n) = anchor {
                aoe["after_adapter_sequence"] = json!(n);
            }
            aoe
        };
        for n in 0..MAX_LEASES {
            ids(i.ingest(frame("l", lease(format!("l{n}"), None)), true, 0, 0));
        }
        let next = MAX_LEASES as u64 + 1;
        let plain_tailer = json!({"v":2,"key":"plain","after_adapter_sequence":next});
        assert!(ids(i.ingest(frame("tail", plain_tailer), true, 0, 0)).is_empty());
        let over = lease("over".into(), Some(next));
        assert!(ids(i.ingest(frame("over", over), true, 0, 0)).is_empty());
        let Ingested::DeliverThenLost(frames, IntegrationLostReason::LeaseOverflow) =
            i.ingest(frame("f", json!(null)), true, 0, 0)
        else {
            panic!("expected the batch before the overflow, then the loss");
        };
        let delivered: Vec<_> = frames
            .iter()
            .map(|d| match &d.notification.update {
                SessionUpdate::ToolCall(tc) => tc.tool_call_id.0.to_string(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(delivered, ["f", "tail"]);
    }

    #[test]
    fn a_lost_integration_drops_its_leases() {
        let i = ingress(true);
        let lease = json!({"v":2,"key":"l1","lease":{"id":"bg","kind":"busy","ttl_ms":3600000}});
        ids(i.ingest(frame("t1", lease), true, 0, 0));
        assert!(i.leases.any_live(1));
        assert!(matches!(
            i.ingest(frame("bad", json!({"v": 2})), true, 0, 1),
            Ingested::Lost(_)
        ));
        assert!(!i.leases.any_live(1) && !i.leases.working(1));
    }
}
