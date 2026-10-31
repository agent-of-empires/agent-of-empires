//! Events synthesized from a tool call's raw input: wakeups, monitors, and
//! background agent launches.

use crate::acp::state::{Event, WorkflowLaunch};
use tracing::{debug, info, warn};

/// Reads `delaySeconds` (a number, or a numeric string) into an absolute wake
/// time. A missing, non-finite, or unrepresentable delay skips the event
/// rather than publishing a wakeup at epoch zero or overflowing (#1091).
pub(super) fn wakeup_event_from_raw(raw_input: &serde_json::Value) -> Option<Event> {
    let Some(delay_value) = raw_input.get("delaySeconds") else {
        debug!(
            target: "acp.protocol.wakeup",
            "ScheduleWakeup raw_input missing `delaySeconds`; not emitting WakeupScheduled"
        );
        return None;
    };
    let Some(delay_secs) = delay_value
        .as_f64()
        .or_else(|| delay_value.as_str().and_then(|s| s.parse().ok()))
    else {
        debug!(
            target: "acp.protocol.wakeup",
            value = %delay_value,
            "ScheduleWakeup `delaySeconds` not numeric; not emitting WakeupScheduled"
        );
        return None;
    };
    if !delay_secs.is_finite() || delay_secs < 0.0 {
        warn!(
            target: "acp.protocol.wakeup",
            delay_secs,
            "ScheduleWakeup `delaySeconds` non-finite or negative; refusing to emit"
        );
        return None;
    }
    let delay_ms = (delay_secs * 1000.0).clamp(0.0, i64::MAX as f64) as i64;
    let Some(at) = chrono::Utc::now().checked_add_signed(chrono::Duration::milliseconds(delay_ms))
    else {
        warn!(
            target: "acp.protocol.wakeup",
            delay_secs,
            "ScheduleWakeup `delaySeconds` overflows the representable wake time; refusing to emit"
        );
        return None;
    };
    let reason = raw_input
        .get("reason")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    info!(
        target: "acp.protocol.wakeup",
        delay_secs,
        wake_at = %at,
        reason = ?reason,
        "emitting WakeupScheduled from ScheduleWakeup tool args"
    );
    Some(Event::WakeupScheduled { at, reason })
}

/// `None` for a frame carrying neither `description` nor `command`:
/// claude-agent-acp sends the initial `tool_call` with empty args, and that
/// must not arm the badge.
pub(super) fn monitor_event_from_raw(raw_input: &serde_json::Value) -> Option<Event> {
    let description = raw_input
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let has_command = raw_input.get("command").and_then(|v| v.as_str()).is_some();
    if description.is_none() && !has_command {
        return None;
    }
    info!(
        target: "acp.protocol.wakeup",
        description = ?description,
        "emitting MonitorArmed from Monitor tool args"
    );
    Some(Event::MonitorArmed { description })
}

/// A `Bash` launched with `run_in_background`: the adapter's metadata-only
/// update carries `toolResponse.backgroundTaskId`. Shown with the Monitor
/// badge since both are off-turn work that reports back later.
pub(super) fn background_command_launched_from_value(v: &serde_json::Value) -> Option<Event> {
    let cc = v.get("_meta")?.get("claudeCode")?;
    if cc.get("toolName").and_then(|t| t.as_str()) != Some("Bash") {
        return None;
    }
    cc.get("toolResponse")?.get("backgroundTaskId")?.as_str()?;
    Some(Event::MonitorArmed { description: None })
}

/// The background task such a `Bash` launch, or a `Workflow` run, started, so
/// the session stays busy until the task ends (reported through the session
/// transcript, see `crate::acp::task_transcript`).
pub(super) fn background_task_started_from_value(v: &serde_json::Value) -> Option<Event> {
    let cc = v.get("_meta")?.get("claudeCode")?;
    let tr = cc.get("toolResponse")?;
    let tool_call_id = v
        .get("toolCallId")
        .and_then(|id| id.as_str())
        .map(str::to_string);
    let str_field = |key: &str| {
        tr.get(key)
            .and_then(|s| s.as_str())
            .filter(|s| !s.is_empty())
    };
    match cc.get("toolName").and_then(|t| t.as_str())? {
        "Bash" => Some(Event::BackgroundTaskStarted {
            task_id: tr.get("backgroundTaskId")?.as_str()?.to_string(),
            tool_call_id,
            description: String::new(),
            workflow: None,
        }),
        // The task tailer needs the tool call id to recognise the run's end.
        "Workflow" if str_field("status") == Some("async_launched") && tool_call_id.is_some() => {
            let name = str_field("workflowName").unwrap_or("Workflow").to_string();
            Some(Event::BackgroundTaskStarted {
                task_id: str_field("taskId")?.to_string(),
                tool_call_id,
                description: name.clone(),
                workflow: Some(WorkflowLaunch {
                    run_id: str_field("runId")?.to_string(),
                    name,
                    summary: str_field("summary").map(str::to_string),
                    transcript_dir: str_field("transcriptDir").unwrap_or_default().to_string(),
                }),
            })
        }
        _ => None,
    }
}

/// A Claude async sub-agent launch, arriving as `_meta.claudeCode` with
/// `toolName: "Agent"` and a `toolResponse` whose `status` is
/// `"async_launched"`. `agentId` is the only required field; anything else
/// falls back to `RawAgentUpdate`.
pub(super) fn background_agent_launched_from_value(v: &serde_json::Value) -> Option<Event> {
    let cc = v.get("_meta")?.get("claudeCode")?;
    if cc.get("toolName").and_then(|t| t.as_str()) != Some("Agent") {
        return None;
    }
    let tr = cc.get("toolResponse")?;
    if tr.get("status").and_then(|s| s.as_str()) != Some("async_launched") {
        return None;
    }
    let agent_id = tr.get("agentId").and_then(|s| s.as_str())?.to_string();
    let str_field = |key: &str| {
        tr.get(key)
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string()
    };
    Some(Event::BackgroundAgentLaunched {
        agent_id,
        tool_call_id: v
            .get("toolCallId")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        description: str_field("description"),
        prompt: str_field("prompt"),
        model: str_field("resolvedModel"),
        output_file: str_field("outputFile"),
        started_at: chrono::Utc::now(),
        workflow: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_command_launch_arms_badge() {
        let launched = |tool: &str, response: serde_json::Value| serde_json::json!({"_meta": {"claudeCode": {"toolName": tool, "toolResponse": response}}});
        let bg = serde_json::json!({"stdout": "", "backgroundTaskId": "byiee4aex"});
        assert!(matches!(
            background_command_launched_from_value(&launched("Bash", bg.clone())),
            Some(Event::MonitorArmed { description: None })
        ));
        assert!(background_command_launched_from_value(&launched("Read", bg)).is_none());
        assert!(background_command_launched_from_value(&launched(
            "Bash",
            serde_json::json!({"stdout": "ok"})
        ))
        .is_none());
    }

    #[test]
    fn background_command_launch_starts_a_task() {
        let update = |tool: &str, response: serde_json::Value| {
            serde_json::json!({
                "toolCallId": "toolu_1",
                "_meta": {"claudeCode": {"toolName": tool, "toolResponse": response}},
            })
        };
        let bg = serde_json::json!({"stdout": "", "backgroundTaskId": "byiee4aex"});
        match background_task_started_from_value(&update("Bash", bg.clone())) {
            Some(Event::BackgroundTaskStarted {
                task_id,
                tool_call_id,
                ..
            }) => {
                assert_eq!(task_id, "byiee4aex");
                assert_eq!(tool_call_id.as_deref(), Some("toolu_1"));
            }
            other => panic!("expected BackgroundTaskStarted, got {other:?}"),
        }
        assert!(background_task_started_from_value(&update("Read", bg)).is_none());
        assert!(background_task_started_from_value(&update(
            "Bash",
            serde_json::json!({"stdout": "ok"})
        ))
        .is_none());
    }

    #[test]
    fn workflow_launch_starts_a_task() {
        // Trimmed from a real claude-agent-acp 0.96 launch result.
        let launched = serde_json::json!({
            "status": "async_launched",
            "taskId": "wfgxmundi",
            "taskType": "local_workflow",
            "workflowName": "acp-0991-issue-drafts",
            "runId": "wf_c10b3836-59f",
            "summary": "Draft and verify 5 issues",
            "transcriptDir": "/home/u/.claude/projects/p/s/subagents/workflows/wf_c10b3836-59f",
        });
        let update = |tool_call_id: Option<&str>, response: serde_json::Value| {
            serde_json::json!({
                "toolCallId": tool_call_id,
                "_meta": {"claudeCode": {"toolName": "Workflow", "toolResponse": response}},
            })
        };
        match background_task_started_from_value(&update(Some("toolu_1"), launched.clone())) {
            Some(Event::BackgroundTaskStarted {
                task_id,
                tool_call_id,
                description,
                workflow,
            }) => {
                assert_eq!(task_id, "wfgxmundi");
                assert_eq!(tool_call_id.as_deref(), Some("toolu_1"));
                assert_eq!(description, "acp-0991-issue-drafts");
                assert_eq!(
                    workflow,
                    Some(WorkflowLaunch {
                        run_id: "wf_c10b3836-59f".into(),
                        name: "acp-0991-issue-drafts".into(),
                        summary: Some("Draft and verify 5 issues".into()),
                        transcript_dir:
                            "/home/u/.claude/projects/p/s/subagents/workflows/wf_c10b3836-59f"
                                .into(),
                    })
                );
            }
            other => panic!("expected BackgroundTaskStarted, got {other:?}"),
        }
        let without = |key: &str| {
            let mut r = launched.clone();
            r.as_object_mut().unwrap().remove(key);
            r
        };
        for (label, tool_call_id, response) in [
            ("no tool call id", None, launched.clone()),
            ("no task id", Some("toolu_1"), without("taskId")),
            ("no run id", Some("toolu_1"), without("runId")),
            ("not launched", Some("toolu_1"), without("status")),
        ] {
            assert!(
                background_task_started_from_value(&update(tool_call_id, response)).is_none(),
                "{label}"
            );
        }
    }

    #[test]
    fn raw_input_parsers() {
        for (label, raw) in [
            ("missing", serde_json::json!({})),
            ("non-numeric", serde_json::json!({ "delaySeconds": "soon" })),
            ("negative", serde_json::json!({ "delaySeconds": -1.0 })),
            // JSON has no infinity literal, so it arrives as a numeric string.
            ("non-finite", serde_json::json!({ "delaySeconds": "inf" })),
            // Finite, but past the range chrono can add to `now`.
            ("overflowing", serde_json::json!({ "delaySeconds": 1e18 })),
        ] {
            assert!(
                wakeup_event_from_raw(&raw).is_none(),
                "{label} delaySeconds must not emit WakeupScheduled"
            );
        }

        // The numeric-string fallback is unique to this layer.
        let before = chrono::Utc::now();
        match wakeup_event_from_raw(&serde_json::json!({ "delaySeconds": "600" })) {
            Some(Event::WakeupScheduled { at, .. }) => {
                let delta = (at - before).num_seconds();
                assert!((600..660).contains(&delta), "expected ~600s, got {delta}s");
            }
            other => panic!("expected WakeupScheduled, got {other:?}"),
        }

        // Another tool, a synchronous Agent call, and an empty payload all stay
        // `RawAgentUpdate`.
        {
            for raw in [
                serde_json::json!({
                    "_meta": { "claudeCode": { "toolName": "Bash", "toolResponse": {} } }
                }),
                serde_json::json!({
                    "_meta": { "claudeCode": {
                        "toolName": "Agent",
                        "toolResponse": { "agentId": "x" }
                    }}
                }),
                serde_json::json!({}),
            ] {
                assert!(
                    background_agent_launched_from_value(&raw).is_none(),
                    "{raw}"
                );
            }
        }
    }
}
