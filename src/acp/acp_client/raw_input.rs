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

/// Borrowed launch shape only. SDK mapping retains its existing fallback and
/// profile gates; recognizing a shape grants no permission or prompt causality.
#[derive(Debug, Clone, Copy)]
pub(crate) enum BackgroundResourceCue<'a> {
    Agent {
        agent_id: &'a str,
        tool_call_id: &'a str,
        description: &'a str,
        prompt: &'a str,
        model: &'a str,
        output_file: &'a str,
    },
    Bash {
        task_id: &'a str,
        tool_call_id: Option<&'a str>,
    },
    Workflow {
        task_id: &'a str,
        tool_call_id: &'a str,
        run_id: &'a str,
        name: &'a str,
        summary: Option<&'a str>,
        transcript_dir: &'a str,
    },
}

/// One pure extractor shared by native framing and typed SDK mapping.
/// Native framing only tests its borrowed result; it allocates no events,
/// strings, timestamps, or JSON copies.
pub(crate) fn background_resource_from_parts<'a>(
    tool_call_id: Option<&'a str>,
    claude_code: Option<&'a serde_json::Value>,
) -> Option<BackgroundResourceCue<'a>> {
    let cc = claude_code?;
    let response = cc.get("toolResponse")?;
    let field = |key: &str| response.get(key).and_then(serde_json::Value::as_str);
    let nonempty = |key: &str| field(key).filter(|value| !value.is_empty());
    match cc.get("toolName").and_then(serde_json::Value::as_str)? {
        "Agent" if field("status") == Some("async_launched") => {
            Some(BackgroundResourceCue::Agent {
                agent_id: field("agentId")?,
                tool_call_id: tool_call_id.unwrap_or_default(),
                description: field("description").unwrap_or_default(),
                prompt: field("prompt").unwrap_or_default(),
                model: field("resolvedModel").unwrap_or_default(),
                output_file: field("outputFile").unwrap_or_default(),
            })
        }
        "Bash" => Some(BackgroundResourceCue::Bash {
            task_id: field("backgroundTaskId")?,
            tool_call_id,
        }),
        "Workflow" if nonempty("status") == Some("async_launched") => {
            Some(BackgroundResourceCue::Workflow {
                task_id: nonempty("taskId")?,
                tool_call_id: tool_call_id?,
                run_id: nonempty("runId")?,
                name: nonempty("workflowName").unwrap_or("Workflow"),
                summary: nonempty("summary"),
                transcript_dir: nonempty("transcriptDir").unwrap_or_default(),
            })
        }
        _ => None,
    }
}

pub(crate) fn background_resource_from_value(
    value: &serde_json::Value,
) -> Option<BackgroundResourceCue<'_>> {
    background_resource_from_parts(
        value.get("toolCallId").and_then(serde_json::Value::as_str),
        value.get("_meta").and_then(|meta| meta.get("claudeCode")),
    )
}

impl BackgroundResourceCue<'_> {
    pub(super) fn into_agent_event(self) -> Option<Event> {
        let Self::Agent {
            agent_id,
            tool_call_id,
            description,
            prompt,
            model,
            output_file,
        } = self
        else {
            return None;
        };
        Some(Event::BackgroundAgentLaunched {
            agent_id: agent_id.into(),
            tool_call_id: tool_call_id.into(),
            description: description.into(),
            prompt: prompt.into(),
            model: model.into(),
            output_file: output_file.into(),
            started_at: chrono::Utc::now(),
            workflow: None,
        })
    }

    pub(super) fn into_monitor_event(self) -> Option<Event> {
        matches!(self, Self::Bash { .. }).then_some(Event::MonitorArmed { description: None })
    }

    pub(super) fn into_task_event(self) -> Option<Event> {
        let (task_id, tool_call_id, description, workflow) = match self {
            Self::Bash {
                task_id,
                tool_call_id,
            } => (
                task_id.into(),
                tool_call_id.map(str::to_string),
                String::new(),
                None,
            ),
            Self::Workflow {
                task_id,
                tool_call_id,
                run_id,
                name,
                summary,
                transcript_dir,
            } => (
                task_id.into(),
                Some(tool_call_id.into()),
                name.into(),
                Some(WorkflowLaunch {
                    run_id: run_id.into(),
                    name: name.into(),
                    summary: summary.map(str::to_string),
                    transcript_dir: transcript_dir.into(),
                }),
            ),
            Self::Agent { .. } => return None,
        };
        Some(Event::BackgroundTaskStarted {
            task_id,
            tool_call_id,
            description,
            workflow,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_command_launch_arms_badge() {
        let launched = |tool: &str, response: serde_json::Value| serde_json::json!({"_meta": {"claudeCode": {"toolName": tool, "toolResponse": response}}});
        let bg = serde_json::json!({"stdout": "", "backgroundTaskId": "byiee4aex"});
        assert!(matches!(
            background_resource_from_value(&launched("Bash", bg.clone()))
                .and_then(BackgroundResourceCue::into_monitor_event),
            Some(Event::MonitorArmed { description: None })
        ));
        assert!(background_resource_from_value(&launched("Read", bg))
            .and_then(BackgroundResourceCue::into_monitor_event)
            .is_none());
        assert!(background_resource_from_value(&launched(
            "Bash",
            serde_json::json!({"stdout": "ok"})
        ))
        .and_then(BackgroundResourceCue::into_monitor_event)
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
        match background_resource_from_value(&update("Bash", bg.clone()))
            .and_then(BackgroundResourceCue::into_task_event)
        {
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
        assert!(background_resource_from_value(&update("Read", bg))
            .and_then(BackgroundResourceCue::into_task_event)
            .is_none());
        assert!(background_resource_from_value(&update(
            "Bash",
            serde_json::json!({"stdout": "ok"})
        ))
        .and_then(BackgroundResourceCue::into_task_event)
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
        match background_resource_from_value(&update(Some("toolu_1"), launched.clone()))
            .and_then(BackgroundResourceCue::into_task_event)
        {
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
                background_resource_from_value(&update(tool_call_id, response))
                    .and_then(BackgroundResourceCue::into_task_event)
                    .is_none(),
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
                    background_resource_from_value(&raw)
                        .and_then(BackgroundResourceCue::into_agent_event)
                        .is_none(),
                    "{raw}"
                );
            }
        }
    }
}
