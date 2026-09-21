//! `session/update` to `Event` mapping, and the dedup that drops an agent's
//! consolidated restatement of text it already streamed.

use crate::acp::agent_profiles;
use crate::acp::approvals::is_destructive;
use crate::acp::state::{
    AvailableCommand, ConfigOptionDescriptor, Event, Plan, PlanStep, SessionMode, SessionUsage,
    ToolCall, UsageCost,
};
use agent_client_protocol::schema::v1::{ContentBlock, MessageId, SessionUpdate};
use tracing::debug;

use super::config_options::map_acp_config_option;
use super::lifecycle::{detect_off_protocol_work_completed, OffProtocolWorkKind};
use super::plan::{extract_plan_from_switch_mode, map_plan_status, plan_status_to_str};
use super::raw_input::{
    background_agent_launched_from_value, monitor_event_from_raw, wakeup_event_from_raw,
};
use super::tool_output::{
    extract_diffs_from_content, extract_memory_recall, extract_tool_content_text,
    extract_tool_output_blocks, preview_args, preview_optional_args, raw_event, tool_kind_str,
    write_diff_from_meta,
};

/// Keeps synthetic tool-call ids minted in the same millisecond distinct.
pub(super) static SYNTHETIC_TOOL_SEQ: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// `/compact` surfaces only as plain text chunks (#1050). Markers are
/// matched as strings; a miss or false hit never loses transcript data.
pub(super) fn is_compact_completion(text: &str) -> bool {
    text.contains("Compacting completed.")
}

pub(super) fn is_compact_start(text: &str) -> bool {
    text.contains("Compacting...")
}

/// The adapter interpolates a reason after this prefix.
pub(super) fn is_compact_failure(text: &str) -> bool {
    text.contains("Compacting failed")
}

/// Drops the adapter's leaked consolidated `agent_message_chunk`, which
/// re-sends a streamed text block whole under a different message id and
/// would otherwise double the message (#2281).
///
/// A same-id chunk is always a genuine delta. A chunk with a different or
/// one-sided id that restates the open block verbatim is the leak. Two
/// id-less chunks are ambiguous and never dropped.
#[derive(Default)]
pub(super) struct AgentMessageDedup {
    block: Option<AgentTextBlock>,
}

struct AgentTextBlock {
    id: Option<MessageId>,
    text: String,
}

impl AgentMessageDedup {
    pub(super) fn reset(&mut self) {
        self.block = None;
    }

    /// True when `update` is the leaked restatement and must be skipped.
    pub(super) fn observe(&mut self, update: &SessionUpdate) -> bool {
        let SessionUpdate::AgentMessageChunk(chunk) = update else {
            self.block = None;
            return false;
        };
        let ContentBlock::Text(t) = &chunk.content else {
            self.block = None;
            return false;
        };
        if t.text.is_empty() {
            // The adapter opens each block with an empty chunk.
            self.block = Some(AgentTextBlock {
                id: chunk.message_id.clone(),
                text: String::new(),
            });
            return false;
        }
        match &mut self.block {
            Some(block) if block.id == chunk.message_id => {
                block.text.push_str(&t.text);
                false
            }
            Some(block) if block.text == t.text => {
                self.block = None;
                true
            }
            _ => {
                self.block = Some(AgentTextBlock {
                    id: chunk.message_id.clone(),
                    text: t.text.clone(),
                });
                false
            }
        }
    }
}

/// Claude Code keepalive pings use `<toolId>-heartbeat-<N>` and would render
/// as phantom tool cards (#3084).
pub(super) fn is_heartbeat_tool_call_id(id: &str) -> bool {
    id.rsplit_once("-heartbeat-")
        .is_some_and(|(_, suffix)| !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()))
}

/// A wake or monitor tool's own event, `None` for any other tool or a profile
/// that does not synthesize them. Args reach `ToolCall` on some adapters and
/// only a later `ToolCallUpdate` on others (#1091), so both arms call this.
fn wake_tool_event(
    profile: &'static agent_profiles::AgentProfile,
    title: Option<&str>,
    raw: &serde_json::Value,
) -> Option<Event> {
    if !profile.supports_wakeup_tools {
        return None;
    }
    match title? {
        "ScheduleWakeup" => wakeup_event_from_raw(raw),
        "Monitor" => monitor_event_from_raw(raw),
        _ => None,
    }
}

/// Unmapped variants pass through as `RawAgentUpdate`. `profile` gates the
/// claude-specific synthesis (subagent linkage, ExitPlanMode, wake tools).
pub(super) fn map_update_to_events(
    update: SessionUpdate,
    profile: &'static agent_profiles::AgentProfile,
) -> Vec<Event> {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => match chunk.content {
            ContentBlock::Text(text) => {
                let mut events = vec![Event::AgentMessageChunk {
                    text: text.text.clone(),
                }];
                if is_compact_start(&text.text) {
                    events.push(Event::ConversationCompactionStarted);
                }
                if is_compact_completion(&text.text) {
                    events.push(Event::ConversationCompacted);
                    // The model forgot its plan, so clear the plan strip.
                    events.push(Event::PlanUpdated {
                        plan: Plan {
                            plan_id: format!("plan-{}", chrono::Utc::now().timestamp_millis()),
                            version: 1,
                            steps: Vec::new(),
                        },
                    });
                }
                events
            }
            other => vec![raw_event(&other)],
        },
        // Only seen on replay (#2276): live prompts record UserPromptSent.
        SessionUpdate::UserMessageChunk(chunk) => match chunk.content {
            ContentBlock::Text(text) => vec![Event::UserPromptSent {
                text: text.text,
                attachments: Vec::new(),
                prompt_id: None,
                synthesized: false,
            }],
            other => vec![raw_event(&other)],
        },
        SessionUpdate::AgentThoughtChunk(_) => vec![Event::ThinkingStarted],
        SessionUpdate::ToolCall(tc) => {
            let raw_args = tc.raw_input.clone().unwrap_or(serde_json::Value::Null);
            // Empty rather than "null" without raw_input (#1713).
            let args_preview = preview_optional_args(tc.raw_input.as_ref());
            let parent_tool_call_id = profile.parent_tool_use_id_from_meta(&tc.meta);
            if let Some(parent) = parent_tool_call_id.as_deref() {
                debug!(
                    target: "acp.protocol",
                    child = %tc.tool_call_id.0,
                    parent,
                    kind = %tool_kind_str(&tc.kind),
                    "subagent child tool_call linked to parent via _meta.claudeCode.parentToolUseId"
                );
            }
            let memory_recall = if profile.supports_memory_recall_tool() {
                extract_memory_recall(&tc.meta, &tc.locations, &tc.content)
            } else {
                None
            };
            let diffs = extract_diffs_from_content(&tc.content);
            let tool_call = ToolCall {
                id: tc.tool_call_id.0.to_string(),
                name: tc.title.clone(),
                kind: tool_kind_str(&tc.kind),
                args_preview: args_preview.clone(),
                started_at: chrono::Utc::now(),
                parent_tool_call_id,
                memory_recall,
                diffs,
            };
            let mut events = vec![Event::ToolCallStarted { tool_call }];
            if is_destructive(&tc.title, &args_preview) {
                debug!(target: "acp.protocol", "tool {} flagged destructive on tool_call ingest", tc.title);
            }
            // ExitPlanMode arrives as a switch_mode tool, not a Plan update.
            if profile.supports_exit_plan_mode
                && matches!(
                    tc.kind,
                    agent_client_protocol::schema::v1::ToolKind::SwitchMode
                )
            {
                if let Some(plan) = extract_plan_from_switch_mode(&raw_args) {
                    events.push(Event::PlanUpdated { plan });
                }
            }
            events.extend(wake_tool_event(profile, Some(tc.title.as_str()), &raw_args));
            events
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let id = update.tool_call_id.0.to_string();
            if profile.emits_heartbeat_keepalives && is_heartbeat_tool_call_id(&id) {
                return Vec::new();
            }
            use agent_client_protocol::schema::v1::ToolCallStatus;
            // `InProgress` re-stamps `started_at` so durations measure the
            // tool, not adapter scheduling (#1060).
            let (is_error, completed, in_progress) = match update.fields.status {
                Some(ToolCallStatus::Failed) => (true, true, false),
                Some(ToolCallStatus::Completed) => (false, true, false),
                Some(ToolCallStatus::InProgress) => (false, false, true),
                _ => (false, false, false),
            };
            let content_text = update
                .fields
                .content
                .as_ref()
                .map(|blocks| extract_tool_content_text(blocks))
                .unwrap_or_default();
            // `Some` replaces the card's diffs, so frames without diff blocks
            // stay `None` (#1721).
            let new_diffs = update
                .fields
                .content
                .as_ref()
                .and_then(|blocks| {
                    let diffs = extract_diffs_from_content(blocks);
                    (!diffs.is_empty()).then_some(diffs)
                })
                .or_else(|| write_diff_from_meta(&update.meta));
            let new_args_preview = update
                .fields
                .raw_input
                .as_ref()
                .filter(|value| !value.is_null())
                .map(preview_args);
            let output_blocks = completed
                .then(|| {
                    update
                        .fields
                        .content
                        .as_deref()
                        .map(extract_tool_output_blocks)
                })
                .flatten()
                .unwrap_or_default();
            let new_title = update.fields.title.clone();
            let mut events: Vec<Event> = Vec::new();
            if new_title.is_some()
                || new_args_preview.is_some()
                || in_progress
                || new_diffs.is_some()
            {
                events.push(Event::ToolCallUpdated {
                    tool_call_id: id.clone(),
                    title: new_title,
                    args_preview: new_args_preview,
                    started_at: in_progress.then(chrono::Utc::now),
                    diffs: new_diffs,
                });
            }
            if completed {
                let async_subagent = matches!(
                    detect_off_protocol_work_completed(&update.fields.content),
                    Some(OffProtocolWorkKind::AsyncAgent)
                );
                events.push(Event::ToolCallCompleted {
                    tool_call_id: id,
                    is_error,
                    content: content_text,
                    output: output_blocks,
                    completed_at: chrono::Utc::now(),
                    async_subagent,
                });
            } else if !content_text.is_empty() {
                events.push(Event::ToolCallContent {
                    tool_call_id: id,
                    content: content_text,
                });
            } else if events.is_empty() {
                // A metadata-only update may be an async sub-agent launch.
                let payload = serde_json::to_value(&update).unwrap_or(serde_json::Value::Null);
                match background_agent_launched_from_value(&payload) {
                    Some(event) => events.push(event),
                    None => events.push(Event::RawAgentUpdate { payload }),
                }
            }
            if let Some(raw) = update.fields.raw_input.as_ref() {
                events.extend(wake_tool_event(
                    profile,
                    update.fields.title.as_deref(),
                    raw,
                ));
            }
            events
        }
        SessionUpdate::Plan(p) => {
            // TodoWrite arrives as a Plan update; a synthetic TodoWrite card
            // pair records each update in the transcript.
            let ts_ms = chrono::Utc::now().timestamp_millis();
            let seq = SYNTHETIC_TOOL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let plan_id = format!("plan-{ts_ms}-{seq}");
            let tool_id = format!("todo-{ts_ms}-{seq}");
            let todos_json: Vec<serde_json::Value> = p
                .entries
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "content": e.content,
                        "status": plan_status_to_str(&e.status),
                    })
                })
                .collect();
            let args_preview = serde_json::json!({ "todos": todos_json }).to_string();
            let steps: Vec<PlanStep> = p
                .entries
                .into_iter()
                .enumerate()
                .map(|(i, e)| PlanStep {
                    id: format!("step-{i}"),
                    title: e.content,
                    detail: None,
                    status: map_plan_status(e.status),
                })
                .collect();
            let now = chrono::Utc::now();
            vec![
                Event::ToolCallStarted {
                    tool_call: ToolCall {
                        id: tool_id.clone(),
                        name: "TodoWrite".to_string(),
                        kind: "think".to_string(),
                        args_preview,
                        started_at: now,
                        parent_tool_call_id: None,
                        memory_recall: None,
                        diffs: Vec::new(),
                    },
                },
                Event::PlanUpdated {
                    plan: Plan {
                        plan_id,
                        version: 1,
                        steps,
                    },
                },
                Event::ToolCallCompleted {
                    tool_call_id: tool_id,
                    is_error: false,
                    content: String::new(),
                    output: Vec::new(),
                    completed_at: now,
                    async_subagent: false,
                },
            ]
        }
        SessionUpdate::CurrentModeUpdate(mode_update) => {
            let id = mode_update.current_mode_id.0.to_string();
            // The legacy enum also folds gemini ApprovalMode ids (#1819).
            let mode = match id.as_str() {
                "default" => SessionMode::Default,
                "plan" => SessionMode::Plan,
                "accept_edits" | "acceptEdits" | "auto_edit" | "autoEdit" => {
                    SessionMode::AcceptEdits
                }
                "bypass_permissions" | "bypassPermissions" | "yolo" => {
                    SessionMode::BypassPermissions
                }
                _ => SessionMode::Default,
            };
            vec![
                Event::CurrentModeChanged {
                    current_mode_id: id,
                },
                Event::ModeChanged { mode },
            ]
        }
        SessionUpdate::UsageUpdate(u) => {
            let usage = SessionUsage {
                used: u.used,
                size: u.size,
                cost: u.cost.map(|c| UsageCost {
                    amount: c.amount,
                    currency: c.currency,
                }),
            };
            vec![Event::UsageUpdated { usage }]
        }
        SessionUpdate::AvailableCommandsUpdate(u) => {
            use agent_client_protocol::schema::v1::AvailableCommandInput;
            let commands: Vec<AvailableCommand> = u
                .available_commands
                .into_iter()
                .map(|c| AvailableCommand {
                    name: c.name,
                    description: c.description,
                    accepts_input: matches!(c.input, Some(AvailableCommandInput::Unstructured(_))),
                })
                .collect();
            debug!(
                target: "acp.protocol",
                count = commands.len(),
                "received AvailableCommandsUpdate from agent"
            );
            vec![Event::AvailableCommandsUpdated { commands }]
        }
        SessionUpdate::ConfigOptionUpdate(update) => {
            let options: Vec<ConfigOptionDescriptor> = update
                .config_options
                .into_iter()
                .filter_map(map_acp_config_option)
                .collect();
            debug!(
                target: "acp.protocol",
                count = options.len(),
                "received ConfigOptionUpdate from agent"
            );
            vec![Event::ConfigOptionsUpdated { options }]
        }
        // AoE owns automatic renaming, so agent titles are ignored.
        SessionUpdate::SessionInfoUpdate(_) => Vec::new(),
        other => vec![raw_event(&other)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::acp_client::test_helpers::text_chunk;
    use crate::acp::acp_client::transcript_filter::{is_transcript_event, transcript_event_kind};
    use crate::acp::state::ConfigOptionCategory;
    use agent_client_protocol::schema::v1::{
        Content, ContentChunk, TextContent, ToolCall as AcpToolCall, ToolCallContent,
        ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
    };

    fn claude(update: SessionUpdate) -> Vec<Event> {
        map_update_to_events(update, &agent_profiles::CLAUDE)
    }

    fn tool_update(id: &'static str, fields: ToolCallUpdateFields) -> SessionUpdate {
        SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(id, fields))
    }

    fn completed(text: &str) -> ToolCallUpdateFields {
        ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .content(vec![ToolCallContent::Content(Content::new(text))])
    }

    fn kinds(events: &[Event]) -> Vec<&'static str> {
        events.iter().map(transcript_event_kind).collect()
    }

    #[test]
    fn heartbeat_tool_call_ids() {
        for (id, want) in [
            ("toolu_01ABC-heartbeat-0", true),
            ("toolu_01ABC-heartbeat-123", true),
            ("toolu_01ABC-heartbeat-x", false),
            ("toolu_01ABC-heartbeat-", false),
            ("toolu_01ABC", false),
            ("toolu_01ABC-heartbeat-1-extra", false),
        ] {
            assert_eq!(is_heartbeat_tool_call_id(id), want, "{id}");
        }
        // #3084: dropped for claude only, and only for heartbeat ids.
        let in_progress = || ToolCallUpdateFields::new().status(ToolCallStatus::InProgress);
        assert!(claude(tool_update("toolu_01ABC-heartbeat-0", in_progress())).is_empty());
        assert_eq!(
            kinds(&claude(tool_update("toolu_01ABC", in_progress()))),
            ["tool_call_updated"]
        );
        let codex = map_update_to_events(
            tool_update("real-heartbeat-0", in_progress()),
            &agent_profiles::CODEX,
        );
        assert_eq!(kinds(&codex), ["tool_call_updated"]);
    }

    #[test]
    fn tool_call_parent_linkage_is_profile_gated() {
        let mut meta = serde_json::Map::new();
        meta.insert(
            "claudeCode".to_string(),
            serde_json::json!({ "parentToolUseId": "tc-task-1" }),
        );
        for (with_meta, profile, want) in [
            (true, &agent_profiles::CLAUDE, Some("tc-task-1")),
            (false, &agent_profiles::CLAUDE, None),
            (true, &agent_profiles::CODEX, None),
        ] {
            let mut tc = AcpToolCall::new("tc-child-1", "Read");
            tc.raw_input = Some(serde_json::json!({"path": "x"}));
            tc.meta = with_meta.then(|| meta.clone());
            let events = map_update_to_events(SessionUpdate::ToolCall(tc), profile);
            let Event::ToolCallStarted { tool_call } = &events[0] else {
                panic!("expected ToolCallStarted, got {events:?}");
            };
            assert_eq!(tool_call.parent_tool_call_id.as_deref(), want);
        }
    }

    #[test]
    fn dedup_cases() {
        let restated = "Concrete repro. Let me inspect the events around lgtm and the \"Plan approved\" message in that session.";
        // (chunks as (text, id), dropped flags)
        let cases: Vec<Vec<(&str, Option<&str>, bool)>> = vec![
            // The reported leak: deltas, then the whole block under a new id.
            vec![
                ("", Some("m1"), false),
                (
                    "Concrete repro. Let me inspect the events around lgtm and",
                    Some("m1"),
                    false,
                ),
                (
                    " the \"Plan approved\" message in that session.",
                    Some("m1"),
                    false,
                ),
                (restated, Some("m2"), true),
            ],
            // Id-less deltas, restatement with a fallback uuid.
            vec![
                ("", None, false),
                ("Getting the real failure log,", None, false),
                (" not guessing this time.", None, false),
                (
                    "Getting the real failure log, not guessing this time.",
                    Some("uuid-1"),
                    true,
                ),
            ],
            vec![
                ("hello world", Some("m1"), false),
                ("hello world", Some("m2"), true),
            ],
            // Repeated same-id deltas are real output.
            vec![
                ("", Some("m1"), false),
                ("ha", Some("m1"), false),
                ("ha", Some("m1"), false),
            ],
            // Both ids absent is ambiguous: never drop.
            vec![
                ("", None, false),
                ("done", None, false),
                ("done", None, false),
            ],
        ];
        for chunks in cases {
            let mut d = AgentMessageDedup::default();
            for (text, id, dropped) in chunks {
                assert_eq!(d.observe(&text_chunk(text, id)), dropped, "{text:?} {id:?}");
            }
        }

        let mut d = AgentMessageDedup::default();
        assert!(!d.observe(&text_chunk("ab", Some("m1"))));
        assert!(d.observe(&text_chunk("ab", Some("m2"))));
        // A tool call ends the block, so identical text starts a new one.
        assert!(!d.observe(&SessionUpdate::ToolCall(AcpToolCall::new("t", "Read"))));
        assert!(!d.observe(&text_chunk("ab", Some("m3"))));
        assert!(d.observe(&text_chunk("ab", Some("m4"))));
        // reset() forgets the open block.
        d.reset();
        assert!(!d.observe(&text_chunk("ab", Some("m5"))));
    }

    #[test]
    fn compaction_markers() {
        for (text, completion, start) in [
            ("Compacting completed.", true, false),
            ("\n\nCompacting completed.\n", true, false),
            ("Compacting...", false, true),
            ("\n\nCompacting...\n", false, true),
            ("compact done", false, false),
            ("compacting", false, false),
            ("", false, false),
        ] {
            assert_eq!(is_compact_completion(text), completion, "{text:?}");
            assert_eq!(is_compact_start(text), start, "{text:?}");
        }
        // #1050, #3219: typed lifecycle events follow the visible chunk.
        let cases: [(&str, &[&str]); 4] = [
            ("just some prose", &[]),
            ("Compacting...", &["conversation_compaction_started"]),
            (
                "\n\nCompacting completed.",
                &["conversation_compacted", "plan_updated"],
            ),
            ("I am compacting the list", &[]),
        ];
        for (text, tail) in cases {
            let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
            let mut want = vec!["agent_message_chunk"];
            want.extend_from_slice(tail);
            assert_eq!(
                kinds(&claude(SessionUpdate::AgentMessageChunk(chunk))),
                want
            );
        }
        // The web reducer matches this bare-string wire form.
        assert_eq!(
            serde_json::to_string(&Event::ConversationCompactionStarted).unwrap(),
            "\"ConversationCompactionStarted\""
        );
    }

    #[test]
    fn transcript_events_suppressed_during_load_replay() {
        let cases = [
            (Event::ConversationCompactionStarted, true),
            (Event::ConversationCompacted, true),
            (
                Event::AgentMessageChunk {
                    text: "Compacting...".into(),
                },
                true,
            ),
            (Event::SessionCleared, false),
            (
                Event::Stopped {
                    reason: "prompt_complete".into(),
                },
                false,
            ),
        ];
        for (event, expected) in cases {
            let kind = transcript_event_kind(&event);
            assert_eq!(is_transcript_event(&event), expected, "{kind}");
        }
    }

    #[test]
    fn tool_call_update_completion() {
        let events = claude(tool_update("tc-1", completed("abc1234 first commit")));
        let [Event::ToolCallCompleted {
            tool_call_id,
            is_error,
            content,
            async_subagent,
            ..
        }] = events.as_slice()
        else {
            panic!("expected ToolCallCompleted, got {events:?}");
        };
        assert_eq!(tool_call_id, "tc-1");
        assert!(!is_error);
        assert_eq!(content, "abc1234 first commit");
        assert!(!async_subagent);

        let events = claude(tool_update(
            "tc-async",
            completed(
                "Async agent launched successfully\nagentId: ae6f0567246843e25 (internal ID)",
            ),
        ));
        assert!(matches!(
            events.as_slice(),
            [Event::ToolCallCompleted {
                async_subagent: true,
                ..
            }]
        ));
    }

    #[test]
    fn tool_call_update_metadata_launch_becomes_background_agent() {
        let mut meta = serde_json::Map::new();
        meta.insert(
            "claudeCode".to_string(),
            serde_json::json!({
                "toolName": "Agent",
                "toolResponse": {
                    "agentId": "a6654829ea0a19032",
                    "description": "grep tmux mentions repo-wide",
                    "prompt": "Grep the repo for tmux.",
                    "outputFile": "/tmp/x/tasks/a6654829ea0a19032.output",
                    "status": "async_launched"
                }
            }),
        );
        let mut update = ToolCallUpdate::new("toolu_01HzYCZK", ToolCallUpdateFields::new());
        update.meta = Some(meta);
        let events = claude(SessionUpdate::ToolCallUpdate(update));
        let [Event::BackgroundAgentLaunched {
            agent_id,
            description,
            output_file,
            ..
        }] = events.as_slice()
        else {
            panic!("expected only BackgroundAgentLaunched, got {events:?}");
        };
        assert_eq!(agent_id, "a6654829ea0a19032");
        assert_eq!(description, "grep tmux mentions repo-wide");
        assert!(output_file.ends_with(".output"));
    }

    #[test]
    fn user_message_chunk_becomes_user_prompt_sent() {
        let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new("hello from the past")));
        let events = claude(SessionUpdate::UserMessageChunk(chunk));
        assert!(matches!(
            events.as_slice(),
            [Event::UserPromptSent { text, attachments, .. }]
                if text == "hello from the past" && attachments.is_empty()
        ));
    }

    #[test]
    fn current_mode_update_classification() {
        use agent_client_protocol::schema::v1::CurrentModeUpdate;
        for (id, want) in [
            ("yolo", SessionMode::BypassPermissions),
            ("auto_edit", SessionMode::AcceptEdits),
            ("autoEdit", SessionMode::AcceptEdits),
            ("default", SessionMode::Default),
            ("plan", SessionMode::Plan),
            ("accept_edits", SessionMode::AcceptEdits),
            ("acceptEdits", SessionMode::AcceptEdits),
            ("bypass_permissions", SessionMode::BypassPermissions),
            ("bypassPermissions", SessionMode::BypassPermissions),
            ("some_future_mode", SessionMode::Default),
        ] {
            let events = claude(SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(
                id.to_string(),
            )));
            let [Event::CurrentModeChanged { current_mode_id }, Event::ModeChanged { mode }] =
                events.as_slice()
            else {
                panic!("expected [CurrentModeChanged, ModeChanged], got {events:?}");
            };
            assert_eq!(current_mode_id, id);
            assert_eq!(*mode, want, "{id}");
        }
    }

    #[test]
    fn tool_call_update_in_progress_restamps_and_streams() {
        let bare = claude(tool_update(
            "tc-3",
            ToolCallUpdateFields::new().status(ToolCallStatus::InProgress),
        ));
        assert!(matches!(
            bare.as_slice(),
            [Event::ToolCallUpdated {
                started_at: Some(_),
                title: None,
                args_preview: None,
                diffs: None,
                ..
            }]
        ));
        let streaming = claude(tool_update(
            "tc-2",
            ToolCallUpdateFields::new()
                .status(ToolCallStatus::InProgress)
                .content(vec![ToolCallContent::Content(Content::new(
                    "partial output",
                ))]),
        ));
        assert!(matches!(
            streaming.as_slice(),
            [
                Event::ToolCallUpdated { started_at: Some(_), .. },
                Event::ToolCallContent { tool_call_id, content },
            ] if tool_call_id == "tc-2" && content == "partial output"
        ));
    }

    #[test]
    fn diffs_bridge_onto_tool_cards() {
        use agent_client_protocol::schema::v1::{Diff, ToolKind};
        let diff = || ToolCallContent::Diff(Diff::new("src/foo.rs", "new").old_text("old"));
        let codex = |update| map_update_to_events(update, &agent_profiles::CODEX);

        let mut tc = AcpToolCall::new("tc-edit-1", "Edit src/foo.rs");
        tc.kind = ToolKind::Edit;
        tc.content = vec![diff()];
        let events = codex(SessionUpdate::ToolCall(tc));
        let Event::ToolCallStarted { tool_call } = &events[0] else {
            panic!("expected ToolCallStarted, got {events:?}");
        };
        assert_eq!(tool_call.diffs[0].path, "src/foo.rs");
        assert_eq!(tool_call.diffs[0].new_text.as_deref(), Some("new"));

        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .content(vec![diff()]);
        let events = codex(tool_update("tc-edit-1", fields));
        assert!(events.iter().any(|e| matches!(
            e,
            Event::ToolCallUpdated { diffs: Some(d), .. } if d.len() == 1 && d[0].path == "src/foo.rs"
        )));

        // Text-only frames must not wipe earlier diffs.
        let events = codex(tool_update("tc-edit-1", completed("done")));
        assert!(!events
            .iter()
            .any(|e| matches!(e, Event::ToolCallUpdated { diffs: Some(_), .. })));
    }

    #[test]
    fn wake_and_monitor_tools_emit_from_updates() {
        let update = |title: &str, raw: Option<serde_json::Value>| {
            let mut fields = ToolCallUpdateFields::new().title(title.to_string());
            if let Some(raw) = raw {
                fields = fields.raw_input(raw);
            }
            claude(tool_update("toolu_test", fields))
        };
        // #1091: the args arrive on the update, not the initial frame.
        let events = update(
            "ScheduleWakeup",
            Some(serde_json::json!({
                "delaySeconds": 600,
                "prompt": "Wake-up fired. Confirm.",
                "reason": "Test 10-minute wake-up card countdown",
            })),
        );
        let Some(Event::WakeupScheduled { at, reason }) = events
            .iter()
            .find(|e| matches!(e, Event::WakeupScheduled { .. }))
        else {
            panic!("expected WakeupScheduled, got {events:?}");
        };
        assert!((590..=610).contains(&(*at - chrono::Utc::now()).num_seconds()));
        assert_eq!(
            reason.as_deref(),
            Some("Test 10-minute wake-up card countdown")
        );
        assert!(!update("ScheduleWakeup", None)
            .iter()
            .any(|e| matches!(e, Event::WakeupScheduled { .. })));

        let events = update(
            "Monitor",
            Some(serde_json::json!({
                "command": "until cargo clippy; do sleep 5; done",
                "description": "clippy passes",
            })),
        );
        assert!(events.iter().any(|e| matches!(
            e,
            Event::MonitorArmed { description } if description.as_deref() == Some("clippy passes")
        )));
        assert!(!update("Monitor", Some(serde_json::json!({})))
            .iter()
            .any(|e| matches!(e, Event::MonitorArmed { .. })));
    }

    #[test]
    fn session_info_updates_are_ignored() {
        use agent_client_protocol::schema::v1::SessionInfoUpdate;
        for info in [
            SessionInfoUpdate::new().title("Fix the flaky test".to_string()),
            SessionInfoUpdate::new().updated_at("2026-06-25T00:00:00Z".to_string()),
        ] {
            assert!(claude(SessionUpdate::SessionInfoUpdate(info)).is_empty());
        }
    }

    #[test]
    fn usage_update_emits_typed_usage_event() {
        use agent_client_protocol::schema::v1::{Cost, UsageUpdate};
        let u = UsageUpdate::new(12_345, 200_000).cost(Cost::new(0.42, "USD"));
        let events = claude(SessionUpdate::UsageUpdate(u));
        let [Event::UsageUpdated { usage }] = events.as_slice() else {
            panic!("expected UsageUpdated, got {events:?}");
        };
        assert_eq!((usage.used, usage.size), (12_345, 200_000));
        let cost = usage.cost.as_ref().unwrap();
        assert!((cost.amount - 0.42).abs() < f64::EPSILON);
        assert_eq!(cost.currency, "USD");
    }

    #[test]
    fn available_commands_update_emits_typed_event() {
        use agent_client_protocol::schema::v1::{
            AvailableCommand as AcpAvailableCommand, AvailableCommandInput,
            AvailableCommandsUpdate, UnstructuredCommandInput,
        };
        let cmds = vec![
            AcpAvailableCommand::new("review", "Review changes").input(
                AvailableCommandInput::Unstructured(UnstructuredCommandInput::new("PR url")),
            ),
            AcpAvailableCommand::new("clear", "Reset context"),
        ];
        let events = claude(SessionUpdate::AvailableCommandsUpdate(
            AvailableCommandsUpdate::new(cmds),
        ));
        let [Event::AvailableCommandsUpdated { commands }] = events.as_slice() else {
            panic!("expected AvailableCommandsUpdated, got {events:?}");
        };
        let got: Vec<_> = commands
            .iter()
            .map(|c| (c.name.as_str(), c.accepts_input))
            .collect();
        assert_eq!(got, [("review", true), ("clear", false)]);
    }

    #[test]
    fn config_option_update_maps_categories() {
        use agent_client_protocol::schema::v1::{
            ConfigOptionUpdate, SessionConfigOption, SessionConfigOptionCategory,
            SessionConfigSelectOption,
        };
        let option =
            |id: &'static str, current: &'static str, values: &[&'static str], category| {
                SessionConfigOption::select(
                    id,
                    id,
                    current,
                    values
                        .iter()
                        .map(|v| SessionConfigSelectOption::new(*v, *v))
                        .collect::<Vec<_>>(),
                )
                .category(category)
            };
        let update = ConfigOptionUpdate::new(vec![
            option(
                "model",
                "claude-opus-4-7",
                &["claude-opus-4-7", "claude-sonnet-4-6"],
                SessionConfigOptionCategory::Model,
            ),
            option(
                "effort",
                "default",
                &["default", "high"],
                SessionConfigOptionCategory::ThoughtLevel,
            ),
            option(
                "mode",
                "default",
                &["default", "plan"],
                SessionConfigOptionCategory::Mode,
            ),
            // #1563: an unknown category name passes through, not dropped.
            option(
                "future",
                "a",
                &["a"],
                SessionConfigOptionCategory::Other("future_category".into()),
            ),
        ]);
        let events = claude(SessionUpdate::ConfigOptionUpdate(update));
        let [Event::ConfigOptionsUpdated { options }] = events.as_slice() else {
            panic!("expected ConfigOptionsUpdated, got {events:?}");
        };
        let got: Vec<_> = options
            .iter()
            .map(|o| {
                (
                    o.id.as_str(),
                    o.category.clone(),
                    o.current_value.as_str(),
                    o.options.len(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("model", ConfigOptionCategory::Model, "claude-opus-4-7", 2),
                ("effort", ConfigOptionCategory::ThoughtLevel, "default", 2),
                ("mode", ConfigOptionCategory::Mode, "default", 2),
                (
                    "future",
                    ConfigOptionCategory::Other("future_category".into()),
                    "a",
                    1
                ),
            ]
        );
    }
}
