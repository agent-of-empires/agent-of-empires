//! Idle auto-stop (#1689) and repair of turns that ended with no terminal event (#3190).

use std::sync::Arc;
use std::time::Duration;

use super::{is_resumable, query_store, resolve_per_profile, AppState};
use crate::acp::event_store::ControlKnowledge::Known;

/// The idle threshold is hours; probe it coarsely.
pub(super) const IDLE_REAP_INTERVAL: Duration = Duration::from_secs(60);

pub(super) const TERMINAL_REPAIR_INTERVAL: Duration = Duration::from_secs(30);

/// How long a cost-bearing `UsageUpdated` must stand as the latest event before
/// the turn counts as finished. Far more patient than `acp_client`'s 3s watchdog
/// because this writes a terminal into the canonical log.
const TERMINAL_REPAIR_GRACE_SECS: u32 = 60;

#[cfg(test)]
type IdleDecisionGate = (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);
#[cfg(test)]
static IDLE_DECISION_GATE: std::sync::Mutex<Option<IdleDecisionGate>> = std::sync::Mutex::new(None);
#[cfg(test)]
static IDLE_PROBE_GATE: std::sync::Mutex<Option<IdleDecisionGate>> = std::sync::Mutex::new(None);

/// Publishes the `Stopped` an agent-initiated turn never got, inferred from the
/// adapter's end-of-turn marker being latest, never from silence alone. A turn
/// that ended on the usage limit is parked instead, which stops its worker.
pub(super) async fn repair_missing_terminal(state: &Arc<AppState>) {
    // `Waiting` is excluded: an approval is legitimately silent.
    let candidates: Vec<String> = {
        let instances = state.instances.read().await;
        instances
            .iter()
            .filter(|i| {
                i.is_structured()
                    && i.terminal_handoff.is_none()
                    && i.status == crate::session::Status::Running
            })
            .map(|i| i.id.clone())
            .collect()
    };
    if candidates.is_empty() {
        return;
    }
    let store = Arc::clone(&state.acp_event_store);
    let ids = candidates.clone();
    let latest_at = match tokio::task::spawn_blocking(move || {
        store.last_event_at_for_sessions(&ids)
    })
    .await
    {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(target: "acp.supervisor", error = %e, "terminal-repair activity query failed");
            return;
        }
    };
    let now_ms = crate::time::Timestamp::now().as_millis();
    let grace_ms = i64::from(TERMINAL_REPAIR_GRACE_SECS) * 1000;
    for id in candidates {
        if latest_at
            .get(&id)
            .is_none_or(|last| now_ms.saturating_sub(*last) < grace_ms)
        {
            continue;
        }
        // Before the probes, so a prompt admitted during them fails the stop.
        let observed = state
            .acp_supervisor
            .running_lease(&id)
            .map(|lease| (lease.admissions(), lease));
        // A prompt still being published is not in the log yet.
        if observed
            .as_ref()
            .is_some_and(|(_, lease)| lease.has_unsettled_admission())
        {
            continue;
        }
        let probe = query_store(&state.acp_event_store, &id, "terminal-repair", |s, id| {
            (
                s.terminal_repair_probe(id),
                s.has_in_flight_turn(id),
                s.has_open_tool_call_in_epoch(id),
                !s.unresolved_approval_nonces(id).is_empty()
                    || !s.unresolved_elicitation_nonces(id).is_empty(),
            )
        })
        .await;
        let Some((Some(latest), in_flight, open_tool, awaiting_user)) = probe else {
            continue;
        };
        // Same condition as `acp_client`'s `LifecycleSignal::TerminalUsage`.
        let terminal_usage = matches!(
            &latest.substantive,
            crate::acp::Event::UsageUpdated { usage } if usage.cost.is_some()
        );
        if !terminal_usage
            || !matches!(in_flight, Ok(Known(false)))
            || open_tool
            || awaiting_user
            || now_ms.saturating_sub(latest.substantive_at_ms) < grace_ms
        {
            continue;
        }
        let sole_text = query_store(
            &state.acp_event_store,
            &id,
            "terminal-repair text",
            |s, id| s.agent_turn_sole_text(id),
        )
        .await
        .flatten();
        if let Some(info) = sole_text
            .as_deref()
            .and_then(crate::acp::state::RateLimitInfo::from_agent_text)
        {
            park_limited_agent_turn(state, &id, info, latest.latest_seq, observed).await;
            continue;
        }
        // Conditional on `latest_seq` (not the substantive seq, which ambient
        // events outrun) so anything published since the probe wins.
        if state.acp_supervisor.publish_stopped_if_seq(
            &id,
            "inferred_prompt_complete",
            latest.latest_seq,
        ) {
            tracing::info!(
                target: "acp.supervisor",
                session = %id,
                after_seq = latest.latest_seq,
                quiet_ms = now_ms.saturating_sub(latest.substantive_at_ms),
                "terminal-repair: agent-initiated turn ended with no Stopped; published inferred_prompt_complete"
            );
        }
    }
}

/// Ends a turn whose only output was the agent's usage-limit text the way the
/// connection does (#4373): `RateLimit`, a `rate_limited` stop, and no worker,
/// so the rate-limit reaper owns the session (#5990).
async fn park_limited_agent_turn(
    state: &Arc<AppState>,
    id: &str,
    info: crate::acp::state::RateLimitInfo,
    latest_seq: u64,
    observed: Option<(u64, crate::acp::runner_lifecycle::Lease)>,
) {
    let supervisor = &state.acp_supervisor;
    let status = info.status.clone();
    // Only a stored park lets the reaper resume what the stop interrupts.
    if !supervisor.publish_if_seq(id, &crate::acp::Event::RateLimit { info }, latest_seq) {
        return;
    }
    use crate::acp::supervisor::SupervisorError;
    // The stop publishes the `rate_limited` terminal itself.
    let stop = match &observed {
        Some((admissions, lease)) => {
            supervisor
                .shutdown_rate_limited(id, (lease, *admissions))
                .await
        }
        None => Err(SupervisorError::UnknownSession(id.to_string())),
    };
    match stop {
        Ok(()) => {}
        Err(SupervisorError::UnknownSession(_)) => {
            supervisor.publish_stopped_if_seq(id, "rate_limited", latest_seq + 1);
        }
        // A prompt admitted since owns the turn's terminal.
        Err(SupervisorError::Superseded(_)) => return,
        Err(e) => {
            tracing::warn!(target: "acp.supervisor", session = %id, "terminal-repair: usage-limit stop failed: {e}");
            return;
        }
    }
    tracing::info!(
        target: "acp.supervisor",
        session = %id,
        after_seq = latest_seq,
        %status,
        "terminal-repair: agent-initiated turn ended on the usage limit; published RateLimit + rate_limited"
    );
}

/// A worker is auto-stopped only when enabled, not mid-turn, and its last
/// event is at least `threshold_secs` old. No events means never.
fn should_auto_stop(
    now_ms: i64,
    last_event_ms: Option<i64>,
    threshold_secs: u32,
    in_flight: bool,
) -> bool {
    threshold_secs > 0
        && !in_flight
        && last_event_ms
            .is_some_and(|ms| now_ms.saturating_sub(ms) >= i64::from(threshold_secs) * 1000)
}

/// Persist a bound dormancy stamp before deciding the exact idle retirement.
pub(super) async fn reap_idle_workers(state: &Arc<AppState>) {
    // Queued work is not idle: reaping would fight wake-on-drain.
    let candidates: Vec<(String, String)> = {
        let instances = state.instances.read().await;
        instances
            .iter()
            .filter(|i| is_resumable(i) && i.queued_prompts.is_empty())
            .map(|i| (i.id.clone(), i.source_profile.clone()))
            .collect()
    };
    if candidates.is_empty() {
        return;
    }
    let idle_by_profile = resolve_per_profile(candidates.iter().map(|(_, p)| p.clone()), |c| {
        c.acp.auto_stop_idle_secs
    })
    .await;
    for (id, profile) in candidates {
        let idle_secs = idle_by_profile.get(&profile).copied().unwrap_or(0);
        if idle_secs == 0 {
            continue;
        }
        let Some(submission) = state.session_service.try_prompt_submission(&id).await else {
            continue;
        };
        #[cfg(test)]
        let probe_gate = { IDLE_PROBE_GATE.lock().unwrap().take() };
        #[cfg(test)]
        if let Some((entered, release)) = probe_gate {
            let _ = entered.send(());
            let _ = release.await;
        }
        if !matches!(
            super::query_flushed(
                &state.acp_supervisor,
                &state.acp_event_store,
                &id,
                "idle-reap quota hold",
                |store, id| store.rate_limit_park(id),
            )
            .await,
            Some(Ok(Known(None)))
        ) {
            continue;
        }
        let Some((lease, activity_seq)) = state.acp_supervisor.idle_observation(&id) else {
            continue;
        };
        let admissions = lease.admissions();
        let Some(scope) = lease.native_scope().ok().flatten() else {
            continue;
        };
        // Re-check after queued events reach the store; a failed probe is busy.
        let probe = super::query_flushed(
            &state.acp_supervisor,
            &state.acp_event_store,
            &id,
            "idle-reap eligibility",
            |store, id| {
                (
                    store
                        .last_event_at_for_sessions(&[id.to_owned()])
                        .remove(id),
                    store.holds_live_work(id),
                    store.has_open_tool_call_in_epoch(id),
                    !store.unresolved_approval_nonces(id).is_empty()
                        || !store.unresolved_elicitation_nonces(id).is_empty(),
                )
            },
        )
        .await;
        let Some((latest, holds_live_work, open_tool, awaiting_user)) = probe else {
            continue;
        };
        let last_ms = latest.map(|last| last.max(lease.idle_baseline_ms()));
        if open_tool
            || awaiting_user
            || !should_auto_stop(
                crate::time::Timestamp::now().as_millis(),
                last_ms,
                idle_secs,
                !matches!(holds_live_work, Ok(Known(false))),
            )
        {
            continue;
        }
        let receipt = match state.session_service.mark_idle_dormant(&id, scope).await {
            Ok(Some(receipt)) => receipt,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(target: "acp.supervisor", session = %id, %error,
                    "idle dormancy persistence declined; worker retained");
                continue;
            }
        };
        #[cfg(test)]
        let gate = { IDLE_DECISION_GATE.lock().unwrap().take() };
        #[cfg(test)]
        if let Some((entered, release)) = gate {
            let _ = entered.send(());
            let _ = release.await;
        }
        match state
            .acp_supervisor
            .shutdown_idle(&id, (&lease, admissions), activity_seq, receipt.stamp)
            .await
        {
            Ok(retirement) => {
                drop(receipt);
                drop(submission);
                state
                    .acp_supervisor
                    .settle_idle_retirement(retirement)
                    .await;
                tracing::info!(target: "acp.supervisor", session = %id, idle_secs,
                    "accepted idle structured worker retirement");
            }
            Err(error) => {
                if let Err(rollback) = state
                    .session_service
                    .rollback_idle_dormancy(&id, receipt)
                    .await
                {
                    tracing::warn!(target: "acp.supervisor", session = %id, %rollback,
                        "idle dormancy rollback remains unconfirmed");
                }
                tracing::debug!(target: "acp.supervisor", session = %id, %error,
                    "idle retirement declined; newer activity or ownership retained");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::state::{SessionUsage, UsageCost};
    use crate::acp::Event;
    use crate::time::Timestamp;

    #[test]
    fn should_auto_stop_policy() {
        const HOUR_MS: i64 = 3_600_000;
        let cases = [
            ("disabled threshold", HOUR_MS * 24, Some(0), 0, false, false),
            ("in flight", HOUR_MS * 24, Some(0), 3600, true, false),
            (
                "idle past threshold",
                HOUR_MS * 2,
                Some(0),
                3600,
                false,
                true,
            ),
            (
                "within threshold",
                HOUR_MS,
                Some(HOUR_MS / 2),
                3600,
                false,
                false,
            ),
            ("no events", HOUR_MS * 24, None, 3600, false, false),
            ("exactly at threshold", HOUR_MS, Some(0), 3600, false, true),
        ];
        for (name, now, last, threshold, in_flight, expected) in cases {
            assert_eq!(
                should_auto_stop(now, last, threshold, in_flight),
                expected,
                "{name}"
            );
        }
    }

    async fn wait_for_worker_sequence(state: &AppState, id: &str, expected: u64) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if state
                    .acp_supervisor
                    .idle_observation(id)
                    .is_some_and(|(_, seq)| seq >= expected)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("actual worker publication reaches the activity fence");
    }

    /// A prompt resuming a long-quiet session has no newer event until its
    /// worker is ready, so neither the start nor a fresh epoch reads as idle,
    /// and a stop claims only the epoch the reaper observed (#4990).
    #[tokio::test]
    #[serial_test::serial]
    async fn reaper_stops_only_an_idle_running_epoch() {
        use crate::acp::event_store::ControlKnowledge::{Indeterminate, Known};
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| {
            Box::pin(async move {
                for case in ["fresh", "idle", "unknown", "prompt_pending"] {
                    let live = managed_idle_session(scope, case).await;
                    let id = live.id().to_owned();
                    if case == "fresh" {
                        live.client
                            .lease
                            .test_backdate_idle_baseline(crate::time::Timestamp::now().as_millis());
                    } else if case == "prompt_pending" {
                        live.state.acp_supervisor.test_note_admission(&id);
                    } else if case == "unknown" {
                        let retained = live.state.acp_event_store.highest_seq(&id).unwrap();
                        live.state
                            .acp_event_store
                            .test_conn()
                            .execute(
                                "DELETE FROM acp_events WHERE session_id=?1 AND seq<?2",
                                rusqlite::params![id, i64::try_from(retained).unwrap()],
                            )
                            .unwrap();
                        live.state.acp_event_store.control_cache().forget(&id);
                        assert!(matches!(
                            live.state.acp_event_store.rate_limit_park(&id).unwrap(),
                            Indeterminate
                        ));
                    } else {
                        assert!(matches!(
                            live.state.acp_event_store.rate_limit_park(&id).unwrap(),
                            Known(None)
                        ));
                    }
                    reap_idle_workers(&live.state).await;
                    let dormant = live
                        .state
                        .instances
                        .read()
                        .await
                        .iter()
                        .find(|row| row.id == id)
                        .unwrap()
                        .is_idle_dormant();
                    assert_eq!(dormant, case == "idle");
                    if case != "idle" {
                        assert_eq!(
                            live.state.acp_supervisor.current_lease(&id),
                            Some(live.client.lease.clone())
                        );
                    }
                    assert_eq!(
                        live.native
                            .requests()
                            .iter()
                            .filter(|method| *method == "session/new")
                            .count(),
                        1
                    );
                    assert!(!live
                        .native
                        .requests()
                        .iter()
                        .any(|method| method == "session/load"));
                }
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn admitted_unpublished_prompt_vetoes_observed_idle_retirement() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| {
            Box::pin(async move {
                let live = managed_idle_session(scope, "idle-unpublished-admission").await;
                let id = live.id().to_owned();
                let lease = live.client.lease.clone();
                let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                let (release_tx, release_rx) = tokio::sync::oneshot::channel();
                let release = scope.hold_release(release_tx);
                *IDLE_DECISION_GATE.lock().unwrap() = Some((entered_tx, release_rx));
                let state = live.state.clone();
                let reaper = scope.spawn_helper(async move {
                    reap_idle_workers(&state).await;
                });
                tokio::time::timeout(Duration::from_secs(5), entered_rx)
                    .await
                    .unwrap()
                    .unwrap();
                let before = live.state.acp_event_store.highest_seq(&id).unwrap();
                live.state.acp_supervisor.test_note_admission(&id);
                assert_eq!(live.state.acp_event_store.highest_seq(&id).unwrap(), before);
                assert!(release.release());
                reaper.await.unwrap();
                assert_eq!(live.state.acp_supervisor.current_lease(&id), Some(lease));
                let row = live
                    .state
                    .instances
                    .read()
                    .await
                    .iter()
                    .find(|row| row.id == id)
                    .unwrap()
                    .clone();
                let durable = crate::session::Storage::open_reference("test")
                    .unwrap()
                    .load()
                    .unwrap()
                    .into_iter()
                    .find(|row| row.id == id)
                    .unwrap();
                assert!(row.idle_dormant_since.is_none());
                assert!(durable.idle_dormant_since.is_none());
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn same_lease_worker_activity_cancels_idle_decision_without_losing_queued_intent() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| Box::pin(async move {
            let live = managed_idle_session(scope, "idle-worker-activity").await;
            let id = live.id().to_owned();
            let lease = live.client.lease.clone();
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
            let release = scope.hold_release(release_tx);
            *IDLE_DECISION_GATE.lock().unwrap() = Some((entered_tx, release_rx));
            let state = live.state.clone();
            let reaper = scope.spawn_helper(async move { reap_idle_workers(&state).await; });
            tokio::time::timeout(Duration::from_secs(5), entered_rx).await.unwrap().unwrap();
            assert!(live.state.session_service.try_prompt_submission(&id).await.is_none());
            let old_stamp = live.state.instances.read().await.iter().find(|row| row.id == id).unwrap().idle_dormant_since.unwrap();
            let storage = crate::session::Storage::open_reference("test").unwrap();
            assert_eq!(storage.load().unwrap().iter().find(|row| row.id == id).unwrap().idle_dormant_since, Some(old_stamp));
            let before = live.state.acp_event_store.highest_seq(&id).unwrap();
            live.native.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{
                "sessionId":live.sid,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"late native output"}}
            }})).await;
            wait_for_worker_sequence(&live.state, &id, before+1).await;
            let state = live.state.clone();
            let queue_id = id.clone();
            let (queued_tx, queued_rx) = tokio::sync::oneshot::channel();
            let queued_task = scope.spawn_helper(async move {
                let queued = state.session_service.enqueue_prompt(&queue_id, "queued-after-idle-probe".into(),
                    "keep this intent".into(), Vec::new(), None, crate::server::session_service::QueueSender::default()).await.unwrap();
                queued_tx.send(queued).unwrap();
            });
            assert!(release.release());
            reaper.await.unwrap();
            queued_task.await.unwrap();
            let queued = queued_rx.await.unwrap();
            assert_eq!(live.state.acp_supervisor.current_lease(&id), Some(lease));
            let memory = live.state.instances.read().await.iter().find(|row| row.id == id).unwrap().clone();
            let durable = storage.load().unwrap().into_iter().find(|row| row.id == id).unwrap();
            for row in [&memory, &durable] {
                assert!(row.idle_dormant_since.is_none());
                assert_eq!(row.queued_prompts.as_slice(), std::slice::from_ref(&queued));
            }
            assert!(live.state.session_service.try_prompt_submission(&id).await.is_some());
        })).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_running_child_holds_the_session_until_it_idles() {
        use crate::acp::event_store::ControlKnowledge::Known;
        use crate::acp::state::SubagentState;
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| {
            Box::pin(async move {
                let mut live = managed_idle_session(scope, "idle-child").await;
                let id = live.id().to_owned();
                for (wire_state, observed, holds) in [
                    ("running", SubagentState::Running, true),
                    ("idle", SubagentState::Idle, false),
                ] {
                    live.native
                        .send(serde_json::json!({
                            "jsonrpc": "2.0", "method": "session/update",
                            "params": {"sessionId": live.sid, "update": {
                                "sessionUpdate": "subagent_update", "sessionId": "child",
                                "state": {"state": wire_state}, "background": true
                            }}
                        }))
                        .await;
                    tokio::time::timeout(std::time::Duration::from_secs(10), async {
                        loop {
                            let event = live.client.next_event().await.expect("child update");
                            if matches!(event, crate::acp::Event::SubagentUpdated { subagent, .. }
                                if subagent.state == observed)
                            {
                                break;
                            }
                        }
                    })
                    .await
                    .expect("announced child activity reaches the durable view");
                    live.make_quota_due();
                    assert_eq!(
                        live.state.acp_event_store.holds_live_work(&id).unwrap(),
                        Known(holds)
                    );
                    reap_idle_workers(&live.state).await;
                    let rows = live.state.instances.read().await;
                    assert_eq!(
                        rows.iter()
                            .find(|row| row.id == id)
                            .unwrap()
                            .is_idle_dormant(),
                        !holds,
                    );
                }
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn fresh_substantive_completion_before_sequence_capture_vetoes_idle_retirement() {
        use crate::acp::event_store::ControlKnowledge::Known;
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| {
            Box::pin(async move {
                let mut live = managed_idle_session(scope, "idle-fresh-completion").await;
                let id = live.id().to_owned();
                let lease = live.client.lease.clone();
                let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                let (release_tx, release_rx) = tokio::sync::oneshot::channel();
                let release = scope.hold_release(release_tx);
                *IDLE_PROBE_GATE.lock().unwrap() = Some((entered_tx, release_rx));
                let state = live.state.clone();
                let reaper = scope.spawn_helper(async move {
                    reap_idle_workers(&state).await;
                });
                tokio::time::timeout(Duration::from_secs(5), entered_rx)
                    .await
                    .unwrap()
                    .unwrap();
                // The reaper owns the submission lock here. Agent-initiated
                // output can still arrive without submitting another prompt.
                live.native.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{
                    "sessionId":live.sid,"update":{"sessionUpdate":"agent_message_chunk",
                        "content":{"type":"text","text":"new substantive output"}}
                }})).await;
                live.native.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{
                    "sessionId":live.sid,"update":{"sessionUpdate":"usage_update","used":10,"size":100,
                        "cost":{"amount":0.1,"currency":"USD"}}
                }})).await;
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        if matches!(live.client.next_event().await.unwrap(), Event::UsageUpdated { .. }) {
                            break;
                        }
                    }
                }).await.unwrap();
                assert_eq!(
                    live.state.acp_event_store.has_in_flight_turn(&id).unwrap(),
                    Known(false)
                );
                assert!(release.release());
                reaper.await.unwrap();
                assert_eq!(live.state.acp_supervisor.current_lease(&id), Some(lease));
                assert!(live
                    .state
                    .instances
                    .read()
                    .await
                    .iter()
                    .find(|row| row.id == id)
                    .unwrap()
                    .idle_dormant_since
                    .is_none());
                assert!(crate::session::Storage::open_reference("test")
                    .unwrap()
                    .load()
                    .unwrap()
                    .iter()
                    .find(|row| row.id == id)
                    .unwrap()
                    .idle_dormant_since
                    .is_none());
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn idle_mark_and_rollback_decline_replaced_native_ownership_or_newer_state() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        use futures_util::FutureExt as _;
        with_managed(|managed| {
            Box::pin(async move {
                for after_mark in [false, true] {
                    for change in [
                        "creation",
                        "generation",
                        "wake",
                        "queue",
                        "profile",
                        "absent",
                    ] {
                        if change == "wake" && !after_mark {
                            continue;
                        }
                        let live = managed_idle_session(managed, change).await;
                        let id = live.id().to_owned();
                        let state = live.state.clone();
                        let lease = live.client.lease.clone();
                        let native_scope = lease.native_scope().unwrap().unwrap();
                        let receipt = if after_mark {
                            Some(
                                tokio::time::timeout(std::time::Duration::from_secs(10), async {
                                    loop {
                                        if let Some(receipt) = state
                                            .session_service
                                            .mark_idle_dormant(&id, native_scope.clone())
                                            .await
                                            .unwrap()
                                        {
                                            break receipt;
                                        }
                                        tokio::task::yield_now().await;
                                    }
                                })
                                .await
                                .unwrap(),
                            )
                        } else {
                            None
                        };
                        let storage = crate::session::Storage::open_reference("test").unwrap();
                        let original_rows = storage.load_instances_complete().unwrap();
                        let mut replacement = original_rows
                            .iter()
                            .find(|row| row.id == id)
                            .unwrap()
                            .clone();
                        match change {
                            "creation" => {
                                replacement.created_at += crate::time::Duration::new(0, 1)
                            }
                            "generation" => replacement.lifecycle_generation += 1,
                            "wake" => replacement.idle_dormant_since = None,
                            "queue" => {
                                replacement.idle_dormant_since = Some(
                                    crate::time::Timestamp::now()
                                        + crate::time::Duration::seconds(1),
                                );
                                replacement
                                    .queued_prompts
                                    .push(super::super::test_fixtures::queued_prompt());
                            }
                            "profile" | "absent" => {}
                            _ => unreachable!(),
                        }
                        let project = managed.tempdir();
                        let profile = crate::session::get_profile_dir("test").unwrap();
                        let retained = project.path().join("retained-original-profile");
                        if change == "profile" {
                            std::fs::rename(&profile, &retained).unwrap();
                            crate::server::test_support::seed_instances_on_disk_for_test(
                                "test",
                                vec![replacement.clone()],
                            );
                        } else {
                            storage
                                .update_complete(|rows, _| {
                                    rows.retain(|row| row.id != id);
                                    if change != "absent" {
                                        rows.push(replacement.clone());
                                    }
                                    Ok(())
                                })
                                .unwrap();
                        }
                        let checked = std::panic::AssertUnwindSafe(async {
                            let current = crate::session::Storage::open_reference("test").unwrap();
                            let before = current.load_instances_complete().unwrap();
                            if let Some(receipt) = receipt {
                                assert!(
                                    !matches!(
                                        state
                                            .session_service
                                            .rollback_idle_dormancy(&id, receipt)
                                            .await,
                                        Ok(true)
                                    ),
                                    "{change}"
                                );
                            } else {
                                assert!(
                                    !matches!(
                                        state
                                            .session_service
                                            .mark_idle_dormant(&id, native_scope)
                                            .await,
                                        Ok(Some(_))
                                    ),
                                    "{change}"
                                );
                            }
                            assert_eq!(
                                serde_json::to_value(current.load_instances_complete().unwrap())
                                    .unwrap(),
                                serde_json::to_value(before).unwrap(),
                                "{change}, after_mark={after_mark}"
                            );
                            assert_eq!(state.acp_supervisor.current_lease(&id), Some(lease));
                        })
                        .catch_unwind()
                        .await;
                        if change == "profile" {
                            std::fs::remove_dir_all(&profile).unwrap();
                            std::fs::rename(&retained, &profile).unwrap();
                        }
                        storage
                            .update_complete(|rows, _| {
                                *rows = original_rows.clone();
                                Ok(())
                            })
                            .unwrap();
                        if let Err(panic) = checked {
                            std::panic::resume_unwind(panic);
                        }
                    }
                }
            })
        })
        .await;
    }

    /// Case 0 replays the real occurrence: an agent-initiated turn after the
    /// prompt's own `Stopped`, ending on the cost-bearing marker. A turn whose
    /// only output is the usage-limit text parks instead (#5990). The rest are vetoes.
    #[tokio::test]
    #[serial_test::serial]
    async fn terminal_repair_publishes_only_for_known_finished_work() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| Box::pin(async move {
            for case in ["known", "unknown", "fresh", "waiting", "sql"] {
                let mut live = managed_idle_session(scope, case).await;
                let id = live.id().to_owned();
                live.native.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{
                    "sessionId":live.sid,"update":{"sessionUpdate":"usage_update","used":10,"size":100,
                        "cost":{"amount":0.1,"currency":"USD"}}
                }})).await;
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        if matches!(live.client.next_event().await.unwrap(), Event::UsageUpdated { .. }) { break; }
                    }
                }).await.unwrap();
                if case != "fresh" { live.make_quota_due(); }
                live.state.instances.write().await.iter_mut().find(|row| row.id == id).unwrap().status =
                    if case == "waiting" { crate::session::Status::Waiting } else { crate::session::Status::Running };
                let before = live.state.acp_event_store.highest_seq(&id).unwrap();
                if case == "unknown" {
                    live.state.acp_event_store.test_conn().execute("DELETE FROM acp_events WHERE session_id=?1 AND seq<?2",
                        rusqlite::params![id, i64::try_from(before).unwrap()]).unwrap();
                }
                if case == "sql" {
                    live.state.acp_event_store.test_conn().execute_batch("ALTER TABLE acp_rate_limit_budgets RENAME TO hidden_repair_budgets").unwrap();
                }
                live.state.acp_event_store.control_cache().forget(&id);
                repair_missing_terminal(&live.state).await;
                if case == "sql" {
                    live.state.acp_event_store.test_conn().execute_batch("ALTER TABLE hidden_repair_budgets RENAME TO acp_rate_limit_budgets").unwrap();
                }
                let events = live.state.acp_event_store.replay_from(&id, before).unwrap();
                let repaired = events.iter().filter(|stored| matches!(stored.event.as_ref(), Event::Stopped { reason }
                    if reason == "inferred_prompt_complete")).count();
                assert_eq!(repaired, usize::from(case == "known"), "{case}");
                live.client.stop_session().await;
            }
        })).await;
    }
    async fn managed_idle_session(
        scope: &mut crate::acp::supervisor::test_support::managed::ManagedScope,
        label: &str,
    ) -> crate::server::test_support::ManagedControlSession {
        crate::session::config::update_config(|config| config.acp.auto_stop_idle_secs = 60)
            .unwrap();
        crate::server::test_support::refresh_canonical_metadata_for_test(&scope.state).await;
        let mut live =
            crate::server::test_support::managed_control_session(scope, label, "claude", None)
                .await;
        let request = live.start_prompt("establish a real completed turn").await;
        live.finish_prompt(&request, None).await;
        live.make_quota_due();
        live.client.lease.test_backdate_idle_baseline(
            (crate::time::Timestamp::now() - crate::time::Duration::hours(1)).as_millis(),
        );
        live
    }
    /// The production shape: a live worker is stopped with a `rate_limited`
    /// terminal, leaving a workerless park for the reaper (#5990). A prompt
    /// admitted but not yet stored keeps the worker, however long it takes.
    #[tokio::test]
    #[serial_test::serial]
    async fn terminal_repair_stops_the_worker_of_a_limited_agent_turn() {
        for admitted_unstored in [false, true] {
            let id = "acp-limited-live";
            let (_home, state, _project) = super::super::test_fixtures::test_state(id);
            state.instances.write().await[0].status = crate::session::Status::Running;
            let sup = &state.acp_supervisor;
            sup.test_insert_worker(id).await;
            let lease = sup.running_lease(id).unwrap();
            // Backdated after the admission, so only its unsettled publication protects the worker.
            let _admission = admitted_unstored.then(|| sup.test_note_admission(id));
            lease.test_backdate_idle_baseline(Timestamp::now().as_millis() - 3_600_000);
            let at_ms = Timestamp::now().as_millis() - 120_000;
            let events = [
                Event::Stopped {
                    reason: "prompt_complete".to_string(),
                },
                Event::AgentMessageChunk {
                    text: "You've hit your 5-hour limit · resets 10pm (America/New_York)"
                        .to_string(),
                    message_id: None,
                },
                Event::UsageUpdated {
                    usage: SessionUsage {
                        used: 1,
                        size: 10,
                        cost: Some(UsageCost {
                            amount: 0.1,
                            currency: "USD".to_string(),
                        }),
                    },
                },
            ];
            for (idx, event) in events.iter().enumerate() {
                state
                    .acp_event_store
                    .record_at(id, idx as u64 + 1, event, at_ms)
                    .unwrap();
            }
            let last_seq = events.len() as u64;
            sup.hydrate_seqs([(id.to_string(), last_seq)]);

            repair_missing_terminal(&state).await;

            let store = &state.acp_event_store;
            let tail = store.replay_from(id, last_seq).unwrap().len();
            if admitted_unstored {
                assert!(sup.is_running(id).await);
                assert_eq!(tail, 0);
                continue;
            }
            assert!(!sup.is_running(id).await);
            // A test worker's stop publishes nothing, so the reason it asked for
            // is read from the lease and the closing event recorded here.
            assert_eq!(lease.test_stop_reason().as_deref(), Some("rate_limited"));
            assert_eq!(tail, 1, "RateLimit");
            store
                .record_at(
                    id,
                    last_seq + 2,
                    &Event::Stopped {
                        reason: "rate_limited".to_string(),
                    },
                    Timestamp::now().as_millis(),
                )
                .unwrap();
            assert!(matches!(store.rate_limit_park(id).unwrap(), Known(Some(_))));
            assert!(matches!(
                store.rate_limit_interrupted_agent_turn(id).unwrap(),
                Known(true)
            ));
        }
    }
}
