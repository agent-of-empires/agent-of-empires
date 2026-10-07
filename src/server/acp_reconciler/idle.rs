//! Idle auto-stop (#1689) and repair of turns that ended with no terminal event (#3190).

use std::sync::Arc;
use std::time::Duration;

use super::{is_resumable, query_store, resolve_per_profile, AppState};
use crate::session::Instance;

/// The idle threshold is hours, so the batched activity query runs coarsely.
pub(super) const IDLE_REAP_INTERVAL: Duration = Duration::from_secs(60);

pub(super) const TERMINAL_REPAIR_INTERVAL: Duration = Duration::from_secs(30);

/// How long a cost-bearing `UsageUpdated` must stand as the latest event before
/// the turn counts as finished. Far more patient than `acp_client`'s 3s watchdog
/// because this writes a terminal into the canonical log.
const TERMINAL_REPAIR_GRACE_SECS: u32 = 60;

/// Publishes the `Stopped` an agent-initiated turn never got, inferred from the
/// adapter's end-of-turn marker being latest, never from silence alone. It
/// only appends the event; it never stops or restarts a worker.
pub(super) async fn repair_missing_terminal(state: &Arc<AppState>) {
    // `Waiting` is excluded: an approval is legitimately silent.
    let candidates: Vec<String> = {
        let instances = state.instances.read().await;
        instances
            .iter()
            .filter(|i| i.is_structured() && i.status == crate::session::Status::Running)
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

fn set_dormant(inst: &mut Instance, dormant: bool) {
    if dormant {
        inst.mark_idle_dormant();
    } else {
        inst.idle_dormant_since = None;
    }
}

/// Returns false when the session is gone.
async fn set_dormant_in_memory(state: &AppState, id: &str, dormant: bool) -> bool {
    let mut instances = state.instances.write().await;
    instances
        .iter_mut()
        .find(|i| i.id == id)
        .map(|inst| set_dormant(inst, dormant))
        .is_some()
}

async fn persist_dormant(state: &AppState, profile: &str, id: &str, dormant: bool) -> bool {
    let Ok(storage) = crate::session::Storage::new(profile, state.file_watch.clone()) else {
        return false;
    };
    let id = id.to_string();
    tokio::task::spawn_blocking(move || {
        storage.update(|instances, _groups| {
            if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                set_dormant(inst, dormant);
            }
            Ok(())
        })
    })
    .await
    .is_ok_and(|r| r.is_ok())
}

/// Shuts down idle workers and marks them dormant so the resume pass skips
/// them. Dormancy is persisted before shutdown so a persist failure leaves the
/// worker alive rather than orphaned.
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
    let mut live = Vec::new();
    for (id, profile) in candidates {
        let idle_secs = idle_by_profile.get(&profile).copied().unwrap_or(0);
        if idle_secs > 0 && state.acp_supervisor.is_running(&id).await {
            live.push((id, profile, idle_secs));
        }
    }
    if live.is_empty() {
        return;
    }
    let ids: Vec<String> = live.iter().map(|(id, _, _)| id.clone()).collect();
    let store = Arc::clone(&state.acp_event_store);
    let latest = match tokio::task::spawn_blocking(move || store.last_event_at_for_sessions(&ids))
        .await
    {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(target: "acp.supervisor", error = %e, "idle-reap activity query failed");
            return;
        }
    };
    let now_ms = chrono::Utc::now().timestamp_millis();
    for (id, profile, idle_secs) in live {
        let last_ms = latest.get(&id).copied();
        if !should_auto_stop(now_ms, last_ms, idle_secs, false) {
            continue;
        }
        // Re-check mid-turn: a turn may have started since the snapshot.
        let in_flight = query_store(
            &state.acp_event_store,
            &id,
            "idle-reap in-flight",
            |s, id| s.has_in_flight_turn(id),
        )
        .await
        .unwrap_or(false);
        if !should_auto_stop(now_ms, last_ms, idle_secs, in_flight)
            || !set_dormant_in_memory(state, &id, true).await
        {
            continue;
        }
        if !persist_dormant(state, &profile, &id, true).await {
            set_dormant_in_memory(state, &id, false).await;
            tracing::warn!(target: "acp.supervisor", session = %id, "idle-reap persist failed; leaving worker alive");
            continue;
        }
        match state.acp_supervisor.shutdown_idle(&id).await {
            Ok(()) | Err(crate::acp::supervisor::SupervisorError::UnknownSession(_)) => {
                tracing::info!(target: "acp.supervisor", session = %id, idle_secs, "auto-stopped idle structured view worker");
            }
            Err(e) => {
                // The worker may still run; clear dormancy so it is not blocked forever.
                set_dormant_in_memory(state, &id, false).await;
                persist_dormant(state, &profile, &id, false).await;
                tracing::warn!(target: "acp.supervisor", session = %id, "idle-reap shutdown failed; cleared dormant marker: {e}");
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
