//! Events synthesized from a tool call's raw input: wakeups, monitors, and
//! background agent launches.

use crate::acp::state::Event;
use tracing::{debug, info, warn};

/// Build a `WakeupScheduled` event from a `ScheduleWakeup` tool's
/// raw_input. Reads `delaySeconds` (number, falls back to numeric
/// string) and the optional `reason`; computes the absolute wake
/// timestamp from `Utc::now()`. Returns `None` if `delaySeconds` is
/// missing, non-finite, or so large the wake time is unrepresentable,
/// better to skip the event than publish a wakeup at epoch zero or
/// panic on overflow. See #1091.
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

/// Build a `MonitorArmed` event from a `Monitor` tool's raw_input. Reads
/// the optional `description` for the badge label. Returns `None` when the
/// frame carries neither `description` nor `command`: claude-agent-acp emits
/// the initial `tool_call` frame with empty args (the real args land on a
/// later `ToolCallUpdate`), and an empty frame should not arm the badge.
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

/// Detect a Claude async sub-agent launch in an otherwise-unmapped ACP
/// update and build a typed `BackgroundAgentLaunched`. The launch arrives
/// as `{ _meta: { claudeCode: { toolName: "Agent", toolResponse: {
/// agentId, description, prompt, resolvedModel, outputFile, status:
/// "async_launched" } } }, toolCallId }`. Returns `None` for anything
/// else (the caller falls back to `RawAgentUpdate`). Field extraction is
/// fully defensive: a missing `agentId` is the only hard requirement.
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wakeup_from_raw_rejects_unusable_delays() {
        let cases = [
            ("missing", serde_json::json!({})),
            ("non-numeric", serde_json::json!({ "delaySeconds": "soon" })),
            ("negative", serde_json::json!({ "delaySeconds": -1.0 })),
            // JSON has no infinity literal, so it arrives as a numeric string.
            ("non-finite", serde_json::json!({ "delaySeconds": "inf" })),
            // Finite, but past the range chrono can add to `now`.
            ("overflowing", serde_json::json!({ "delaySeconds": 1e18 })),
        ];
        for (label, raw) in cases {
            assert!(
                wakeup_event_from_raw(&raw).is_none(),
                "{label} delaySeconds must not emit WakeupScheduled"
            );
        }
    }

    // The JSON-number path is covered end to end by
    // `map_tool_call_update_emits_wakeup_when_title_and_raw_input_land_in_update`;
    // only the numeric-string fallback is unique to this layer.
    #[test]
    fn wakeup_from_raw_schedules_delay_given_as_string() {
        let before = chrono::Utc::now();
        match wakeup_event_from_raw(&serde_json::json!({ "delaySeconds": "600" })) {
            Some(Event::WakeupScheduled { at, .. }) => {
                let delta = (at - before).num_seconds();
                assert!((600..660).contains(&delta), "expected ~600s, got {delta}s");
            }
            other => panic!("expected WakeupScheduled, got {other:?}"),
        }
    }

    #[test]
    fn background_agent_launched_parsed_from_agent_meta() {
        let payload = serde_json::json!({
            "_meta": { "claudeCode": {
                "toolName": "Agent",
                "toolResponse": {
                    "agentId": "a3d5ae46a7a0414b1",
                    "description": "grep tmux mentions repo-wide",
                    "prompt": "Grep the repo for tmux.",
                    "resolvedModel": "claude-opus-4-8[1m]",
                    "outputFile": "/tmp/x/tasks/a3d5ae46a7a0414b1.output",
                    "status": "async_launched"
                }
            }},
            "toolCallId": "toolu_012yUZykQT2vqFXZTvqWev5e"
        });
        match background_agent_launched_from_value(&payload) {
            Some(Event::BackgroundAgentLaunched {
                agent_id,
                tool_call_id,
                description,
                model,
                output_file,
                ..
            }) => {
                assert_eq!(agent_id, "a3d5ae46a7a0414b1");
                assert_eq!(tool_call_id, "toolu_012yUZykQT2vqFXZTvqWev5e");
                assert_eq!(description, "grep tmux mentions repo-wide");
                assert_eq!(model, "claude-opus-4-8[1m]");
                assert!(output_file.ends_with(".output"));
            }
            other => panic!("expected BackgroundAgentLaunched, got {other:?}"),
        }
    }

    #[test]
    fn background_agent_launched_ignores_non_agent_meta() {
        // A normal tool-response RawAgentUpdate must not be promoted.
        let bash = serde_json::json!({
            "_meta": { "claudeCode": { "toolName": "Bash", "toolResponse": {} } }
        });
        assert!(background_agent_launched_from_value(&bash).is_none());
        // An Agent update that is not an async launch (no status) stays raw.
        let sync = serde_json::json!({
            "_meta": { "claudeCode": { "toolName": "Agent", "toolResponse": {
                "agentId": "x"
            }}}
        });
        assert!(background_agent_launched_from_value(&sync).is_none());
        assert!(background_agent_launched_from_value(&serde_json::json!({})).is_none());
    }
}
