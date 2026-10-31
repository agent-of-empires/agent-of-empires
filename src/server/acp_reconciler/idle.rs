//! Idle auto-stop (#1689) and repair of turns that ended with no terminal event (#3190).

use std::sync::Arc;
use std::time::Duration;

use super::{is_resumable, query_store, resolve_per_profile, AppState};

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
/// adapter's end-of-turn marker being latest, never from silence alone. It
/// only appends the event; it never stops or restarts a worker.
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
    let now_ms = chrono::Utc::now().timestamp_millis();
    let grace_ms = i64::from(TERMINAL_REPAIR_GRACE_SECS) * 1000;
    for id in candidates {
        if latest_at
            .get(&id)
            .is_none_or(|last| now_ms.saturating_sub(*last) < grace_ms)
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
            || in_flight
            || open_tool
            || awaiting_user
            || now_ms.saturating_sub(latest.substantive_at_ms) < grace_ms
        {
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
        let Some((lease, activity_seq)) = state.acp_supervisor.idle_observation(&id) else {
            continue;
        };
        let Some(scope) = lease.native_scope().ok().flatten() else {
            continue;
        };
        let probe = query_store(
            &state.acp_event_store,
            &id,
            "idle-reap eligibility",
            |store, id| {
                (
                    store
                        .last_event_at_for_sessions(&[id.to_owned()])
                        .remove(id),
                    store.has_in_flight_turn(id),
                    store.has_open_tool_call_in_epoch(id),
                    !store.unresolved_approval_nonces(id).is_empty()
                        || !store.unresolved_elicitation_nonces(id).is_empty(),
                )
            },
        )
        .await;
        let Some((latest, in_flight, open_tool, awaiting_user)) = probe else {
            continue;
        };
        let last_ms = latest.map(|last| last.max(lease.idle_baseline_ms()));
        if open_tool
            || awaiting_user
            || !should_auto_stop(
                chrono::Utc::now().timestamp_millis(),
                last_ms,
                idle_secs,
                in_flight,
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
            .shutdown_idle(&id, &lease, activity_seq, receipt.stamp)
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
    use super::super::test_fixtures::structured_instance;
    use super::*;
    use crate::acp::state::{SessionUsage, ToolCall, UsageCost};
    use crate::acp::Event;
    use chrono::Utc;

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

    async fn idle_fixture(
        id: &str,
    ) -> (
        crate::session::test_support::AppDirGuard,
        Arc<AppState>,
        tempfile::TempDir,
        crate::acp::runner_lifecycle::Lease,
    ) {
        let (home, state, project) = super::super::test_fixtures::test_state(id);
        std::fs::write(
            crate::session::get_app_dir().unwrap().join("config.toml"),
            "[acp]\nauto_stop_idle_secs = 60\n",
        )
        .unwrap();
        state.acp_supervisor.test_insert_worker(id).await;
        let lease = state.acp_supervisor.running_lease(id).unwrap();
        let row = state
            .instances
            .read()
            .await
            .iter()
            .find(|row| row.id == id)
            .unwrap()
            .clone();
        state.acp_supervisor.test_bind_native_scope(&lease, &row);
        let stale = Utc::now().timestamp_millis() - 3_600_000;
        lease.test_backdate_idle_baseline(stale);
        state
            .acp_event_store
            .record_at(
                id,
                1,
                &Event::Stopped {
                    reason: "prompt_complete".into(),
                },
                stale,
            )
            .unwrap();
        state
            .acp_supervisor
            .hydrate_seqs(state.acp_event_store.all_session_seqs());
        (home, state, project, lease)
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
        use crate::daemon::AcpWorkerState::{Resuming, Running};
        #[derive(Debug)]
        enum Worker {
            Starting,
            /// Admitted an hour ago, installed just now.
            SlowStart,
            Running {
                admitted_secs_ago: i64,
            },
        }
        // (worker, reaped)
        let cases = [
            (Worker::Starting, false),
            (Worker::SlowStart, false),
            (
                Worker::Running {
                    admitted_secs_ago: 0,
                },
                false,
            ),
            (
                Worker::Running {
                    admitted_secs_ago: 3600,
                },
                true,
            ),
        ];
        for (worker, reaped) in cases {
            let id = "resuming-session";
            let (_home, state, _project) = super::super::test_fixtures::test_state(id);
            let app_dir = crate::session::get_app_dir().unwrap();
            std::fs::write(
                app_dir.join("config.toml"),
                "[acp]\nauto_stop_idle_secs = 60\n",
            )
            .unwrap();
            let stale_ms = Utc::now().timestamp_millis() - 3_600_000;
            state
                .acp_event_store
                .record_at(
                    id,
                    1,
                    &Event::Stopped {
                        reason: "rate_limited".into(),
                    },
                    stale_ms,
                )
                .unwrap();
            let sup = &state.acp_supervisor;
            let (_hold, before) = match worker {
                Worker::Starting => (Some(sup.test_hold_starting(id)), Resuming),
                Worker::SlowStart => {
                    let lease = sup.test_hold_starting(id);
                    lease.test_backdate_idle_baseline(Utc::now().timestamp_millis() - 3_600_000);
                    sup.test_finish_starting(&lease);
                    (None, Running)
                }
                Worker::Running { admitted_secs_ago } => {
                    sup.test_insert_worker(id).await;
                    let lease = sup.running_lease(id).unwrap();
                    lease.test_backdate_idle_baseline(
                        Utc::now().timestamp_millis() - admitted_secs_ago * 1000,
                    );
                    (None, Running)
                }
            };
            if let Some(lease) = sup.running_lease(id) {
                let row = state
                    .instances
                    .read()
                    .await
                    .iter()
                    .find(|row| row.id == id)
                    .unwrap()
                    .clone();
                sup.test_bind_native_scope(&lease, &row);
            }

            reap_idle_workers(&state).await;

            let dormant = {
                let instances = state.instances.read().await;
                instances
                    .iter()
                    .find(|i| i.id == id)
                    .unwrap()
                    .is_idle_dormant()
            };
            assert_eq!(dormant, reaped, "{worker:?}");
            assert_eq!(sup.worker_state(id).await == before, !reaped, "{worker:?}");
        }

        // An epoch replaced after the reaper observed it is not stopped.
        let id = "respawned-session";
        let (_home, state, _project) = super::super::test_fixtures::test_state(id);
        let sup = &state.acp_supervisor;
        sup.test_insert_worker(id).await;
        let observed = sup.running_lease(id).unwrap();
        sup.test_respawn_worker(id).await;
        assert!(matches!(
            sup.shutdown_idle(id, &observed, 0, Utc::now()).await,
            Err(crate::acp::supervisor::SupervisorError::Superseded(_))
        ));
        assert_eq!(sup.worker_state(id).await, Running);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn same_lease_worker_activity_cancels_idle_decision_without_losing_queued_intent() {
        let id = "idle-worker-activity";
        let (_home, state, _project, lease) = idle_fixture(id).await;
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        *IDLE_DECISION_GATE.lock().unwrap() = Some((entered_tx, release_rx));
        let reap_state = Arc::clone(&state);
        let reaper = tokio::spawn(async move { reap_idle_workers(&reap_state).await });
        tokio::time::timeout(Duration::from_secs(5), entered_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(
            state
                .session_service
                .try_prompt_submission(id)
                .await
                .is_none(),
            "direct prompt admission cannot cross the idle decision"
        );
        let memory_stamp = state.instances.read().await[0].idle_dormant_since.unwrap();
        let storage = crate::session::Storage::open_reference("default").unwrap();
        assert_eq!(
            storage.load().unwrap()[0].idle_dormant_since,
            Some(memory_stamp)
        );
        let (events, inbound) = tokio::sync::mpsc::channel(4);
        let pump = state
            .acp_supervisor
            .test_start_worker_pump(id, lease.clone(), inbound);
        events
            .send(Event::AgentMessageChunk {
                text: "late native output".into(),
                message_id: None,
            })
            .await
            .unwrap();
        wait_for_worker_sequence(&state, id, 2).await;
        let service = Arc::clone(&state.session_service);
        let queued = tokio::spawn(async move {
            service
                .enqueue_prompt(
                    id,
                    "queued-after-idle-probe".into(),
                    "keep this intent".into(),
                    vec![],
                    None,
                    None,
                )
                .await
        });
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), reaper)
            .await
            .unwrap()
            .unwrap();
        let queued = tokio::time::timeout(Duration::from_secs(5), queued)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(state.acp_supervisor.running_lease(id), Some(lease));
        let memory = state.instances.read().await[0].clone();
        let persisted = storage.load().unwrap().remove(0);
        for row in [&memory, &persisted] {
            assert!(
                row.idle_dormant_since.is_none(),
                "only the old mark is rolled back"
            );
            assert_eq!(row.queued_prompts.as_slice(), std::slice::from_ref(&queued));
        }
        assert!(
            state
                .session_service
                .try_prompt_submission(id)
                .await
                .is_some(),
            "the decision barrier is released independently of retirement"
        );
        pump.abort();
        drop(events);
        let _ = pump.await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn fresh_substantive_completion_before_sequence_capture_vetoes_idle_retirement() {
        let id = "idle-fresh-completion";
        let (_home, state, _project, lease) = idle_fixture(id).await;
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        *IDLE_PROBE_GATE.lock().unwrap() = Some((entered_tx, release_rx));
        let reap_state = Arc::clone(&state);
        let reaper = tokio::spawn(async move { reap_idle_workers(&reap_state).await });
        tokio::time::timeout(Duration::from_secs(5), entered_rx)
            .await
            .unwrap()
            .unwrap();
        let (events, inbound) = tokio::sync::mpsc::channel(4);
        let pump = state
            .acp_supervisor
            .test_start_worker_pump(id, lease.clone(), inbound);
        events
            .send(Event::AgentMessageChunk {
                text: "new substantive output".into(),
                message_id: None,
            })
            .await
            .unwrap();
        events
            .send(Event::Stopped {
                reason: "prompt_complete".into(),
            })
            .await
            .unwrap();
        wait_for_worker_sequence(&state, id, 3).await;
        assert!(!state.acp_event_store.has_in_flight_turn(id));
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), reaper)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.acp_supervisor.running_lease(id), Some(lease));
        assert!(state.instances.read().await[0].idle_dormant_since.is_none());
        assert!(crate::session::Storage::open_reference("default")
            .unwrap()
            .load()
            .unwrap()[0]
            .idle_dormant_since
            .is_none());
        pump.abort();
        drop(events);
        let _ = pump.await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn idle_mark_and_rollback_decline_replaced_native_ownership_or_newer_state() {
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
                let id = "idle-native-witness";
                let (_home, state, project, lease) = idle_fixture(id).await;
                let scope = lease.native_scope().unwrap().unwrap();
                let receipt = if after_mark {
                    Some(
                        state
                            .session_service
                            .mark_idle_dormant(id, scope.clone())
                            .await
                            .unwrap()
                            .unwrap(),
                    )
                } else {
                    None
                };
                let storage = crate::session::Storage::open_reference("default").unwrap();
                let mut replacement = storage.load().unwrap().remove(0);
                match change {
                    "creation" => replacement.created_at += chrono::Duration::nanoseconds(1),
                    "generation" => replacement.lifecycle_generation += 1,
                    "wake" => replacement.idle_dormant_since = None,
                    "queue" => {
                        replacement.idle_dormant_since =
                            Some(Utc::now() + chrono::Duration::seconds(1));
                        replacement
                            .queued_prompts
                            .push(super::super::test_fixtures::queued_prompt());
                    }
                    "profile" | "absent" => {}
                    _ => unreachable!(),
                }
                if change == "profile" {
                    std::fs::rename(
                        crate::session::get_profile_dir("default").unwrap(),
                        project.path().join("retained-old-profile"),
                    )
                    .unwrap();
                    crate::server::test_support::seed_instances_on_disk_for_test(
                        "default",
                        vec![replacement.clone()],
                    );
                } else {
                    storage
                        .update_complete(|rows, _| {
                            rows.clear();
                            if change != "absent" {
                                rows.push(replacement.clone());
                            }
                            Ok(())
                        })
                        .unwrap();
                }
                let current = crate::session::Storage::open_reference("default").unwrap();
                let before = current.load().unwrap();
                if let Some(receipt) = receipt {
                    let rolled_back = state
                        .session_service
                        .rollback_idle_dormancy(id, receipt)
                        .await;
                    assert!(!matches!(rolled_back, Ok(true)), "{change}");
                } else {
                    let marked = state.session_service.mark_idle_dormant(id, scope).await;
                    assert!(!matches!(marked, Ok(Some(_))), "{change}");
                }
                assert_eq!(
                    serde_json::to_value(current.load().unwrap()).unwrap(),
                    serde_json::to_value(before).unwrap(),
                    "{change}, after_mark={after_mark}"
                );
                assert_eq!(state.acp_supervisor.running_lease(id), Some(lease));
            }
        }
    }

    /// Case 0 replays the real occurrence: an agent-initiated turn after the
    /// prompt's own `Stopped`, ending on the cost-bearing marker. The rest are vetoes.
    #[tokio::test]
    #[serial_test::serial]
    async fn terminal_repair_publishes_only_for_a_finished_agent_turn() {
        fn usage(cost: bool) -> Event {
            Event::UsageUpdated {
                usage: SessionUsage {
                    used: 400_000,
                    size: 1_000_000,
                    cost: cost.then(|| UsageCost {
                        amount: 21.4,
                        currency: "USD".to_string(),
                    }),
                },
            }
        }
        fn tool_call(id: &str) -> ToolCall {
            ToolCall {
                id: id.to_string(),
                name: "Terminal".to_string(),
                kind: "execute".to_string(),
                args_preview: "{}".to_string(),
                started_at: Utc::now(),
                parent_tool_call_id: None,
                memory_recall: None,
                diffs: Vec::new(),
            }
        }
        fn prompt(text: &str) -> Event {
            Event::UserPromptSent {
                prompt_id: None,
                text: text.to_string(),
                attachments: Vec::new(),
                synthesized: false,
                origin_plugin: None,
            }
        }
        let finished_turn = |extra: Vec<Event>, tail: Vec<Event>| {
            let mut evs = vec![
                prompt("continue"),
                Event::Stopped {
                    reason: "prompt_complete".to_string(),
                },
                Event::ToolCallStarted {
                    tool_call: tool_call("t1"),
                },
                Event::ToolCallCompleted {
                    tool_call_id: "t1".to_string(),
                    is_error: false,
                    content: String::new(),
                    output: Vec::new(),
                    completed_at: Utc::now(),
                    async_subagent: false,
                },
                Event::AgentMessageChunk {
                    text: "Done.".to_string(),
                    message_id: None,
                },
            ];
            evs.extend(extra);
            evs.push(usage(true));
            evs.extend(tail);
            evs
        };
        use crate::session::Status::{Running, Waiting};
        let approval = Event::ApprovalRequested {
            approval: crate::acp::approvals::Approval {
                nonce: crate::acp::approvals::Nonce("n-1".to_string()),
                tool_call: tool_call("t-approval"),
                destructive: false,
                options: Vec::new(),
                choice: false,
                plan: None,
                requested_at: Utc::now(),
                resolved: None,
            },
        };
        // (name, events, status, age_secs, expect_repair)
        let cases = vec![
            (
                "finished agent-initiated turn",
                finished_turn(vec![], vec![]),
                Running,
                120,
                true,
            ),
            (
                // The seq counter advances for ambient events too.
                "ambient event trails the marker",
                finished_turn(
                    vec![],
                    vec![Event::AcpSessionAssigned {
                        acp_session_id: "acp-1".to_string(),
                        connection_ready: false,
                        native_subject: None,
                    }],
                ),
                Running,
                120,
                true,
            ),
            (
                "inside the grace window",
                finished_turn(vec![], vec![]),
                Running,
                5,
                false,
            ),
            (
                "latest is cost-free usage",
                finished_turn(vec![], vec![usage(false)]),
                Running,
                120,
                false,
            ),
            (
                "prompt lacks its terminator",
                vec![prompt("go"), usage(true)],
                Running,
                120,
                false,
            ),
            (
                "tool still open",
                finished_turn(
                    vec![Event::ToolCallStarted {
                        tool_call: tool_call("t2"),
                    }],
                    vec![],
                ),
                Running,
                120,
                false,
            ),
            // An approval can outlive the Waiting status; seeded before the marker.
            (
                "unresolved approval",
                finished_turn(vec![approval], vec![]),
                Running,
                120,
                false,
            ),
            (
                "waiting on the user",
                finished_turn(vec![], vec![]),
                Waiting,
                120,
                false,
            ),
        ];

        for (name, events, status, age_secs, expect_repair) in cases {
            let id = "acp-terminal-repair";
            let project = tempfile::TempDir::new().unwrap();
            let mut inst = structured_instance(id, &project.path().to_string_lossy());
            inst.status = status;
            let state = crate::server::test_support::build_test_app_state(vec![inst]);
            let at_ms = Utc::now().timestamp_millis() - age_secs * 1000;
            let last_seq = events.len() as u64;
            for (idx, event) in events.iter().enumerate() {
                state
                    .acp_event_store
                    .record_at(id, idx as u64 + 1, event, at_ms)
                    .unwrap();
            }
            state
                .acp_supervisor
                .hydrate_seqs([(id.to_string(), last_seq)]);

            repair_missing_terminal(&state).await;

            let repaired: Vec<u64> = state
                .acp_event_store
                .replay_from(id, 0)
                .into_iter()
                .filter(|(_, e)| matches!(e, Event::Stopped { reason } if reason == "inferred_prompt_complete"))
                .map(|(seq, _)| seq)
                .collect();
            let expected = if expect_repair {
                vec![last_seq + 1]
            } else {
                vec![]
            };
            assert_eq!(repaired, expected, "{name}");
        }
    }
}
