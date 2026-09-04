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

/// Monotonic counter appended to synthetic tool-call IDs so two events
/// minted within the same millisecond don't collide on the
/// `(session_id, tool_id)` keys used by the structured view event store.
pub(super) static SYNTHETIC_TOOL_SEQ: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Heuristic detector for the end of a `/compact` cycle. The Claude ACP
/// adapter emits "Compacting..." while the compaction runs and
/// "Compacting completed." once the model's context window has been
/// replaced by a summary; both as plain `agent_message_chunk`s with no
/// `_meta` flag (see #1050 for the upstream gap). String-matching on
/// the completion message is fragile to localisation but the wrong-firing
/// failure mode (an extra "context reset" divider) is harmless; it can
/// never destroy transcript data.
pub(super) fn is_compact_completion(text: &str) -> bool {
    text.contains("Compacting completed.")
}

/// Heuristic detector for the start of a `/compact` cycle. The Claude ACP
/// adapter emits "Compacting..." as a plain `agent_message_chunk` and then
/// runs the summarization API call with no further ACP progress until
/// "Compacting completed.". Without a signal, the silent-orphan watchdog
/// reads that quiet window as a wedged agent and cancels the compaction
/// after the base grace. Same fragility trade-off as `is_compact_completion`:
/// a missed match only reverts to that false-positive kill, never data loss.
/// See #2898.
pub(super) fn is_compact_start(text: &str) -> bool {
    text.contains("Compacting...")
}

/// Heuristic detector for a `/compact` cycle that ended without replacing
/// the context. The adapter emits `\n\nCompacting failed{reason}` for a
/// user cancel ("API Error: Request was aborted."), an API error, or too
/// little to summarize, as another bare `agent_message_chunk`.
///
/// Matching the prefix rather than a full sentence because the reason is
/// interpolated. Same fragility trade-off as the other two markers: a
/// missed match only reverts to the pre-fix behavior of reading the tail
/// as a fresh agent-initiated turn.
pub(super) fn is_compact_failure(text: &str) -> bool {
    text.contains("Compacting failed")
}

/// Tracks the in-flight assistant text block so claude-agent-acp's leaked
/// consolidated `agent_message_chunk` restatement can be dropped before it
/// reaches the watchdog, the event store, or any client. The adapter streams a
/// text block as incremental chunks, then re-sends the whole block as one
/// chunk; its own dedup (`streamedTextIds`) is meant to suppress that copy but
/// misses on a message-id mismatch (deterministic right after an Opus to Sonnet
/// switch, intermittent otherwise), so both reach us and every reducer appends
/// both, doubling the message. See #2281.
///
/// The adapter's `message_id` only ever marks a new message, never reuses one
/// across messages, so a same-id chunk is always a genuine delta and is never
/// dropped (a legitimately repeated delta keeps the same id, for example
/// streamed "ha" then "ha"). Any other chunk whose text restates the open
/// block's accumulated text verbatim is the leaked consolidated copy, regardless
/// of whether its id is present, absent, or merely different from the block's.
/// Matching on content rather than on both ids being present closes the
/// id-presence-asymmetry hole the adapter hits when `message_start` carries no
/// id: the streamed deltas then arrive id-less while the restatement still
/// carries a fallback uuid, so the two ids differ and the restatement is caught.
/// claude-agent-acp >=0.49.0 dedups by content upstream (#800), so this is a
/// backstop for that and for any other adapter that leaks the copy. The only
/// shape left unfiltered is both sides genuinely id-less, where a verbatim
/// repeat is indistinguishable from a leak; that degrades to the same-id append
/// path rather than risk corrupting real output.
#[derive(Default)]
pub(super) struct AgentMessageDedup {
    block: Option<AgentTextBlock>,
}

pub(super) struct AgentTextBlock {
    id: Option<MessageId>,
    text: String,
}

impl AgentMessageDedup {
    /// Forget any in-flight block. Called while post-load history replay is
    /// suppressed so replayed chunks cannot poison live block tracking once
    /// suppression lifts.
    pub(super) fn reset(&mut self) {
        self.block = None;
    }

    /// Returns true when `update` is the leaked consolidated restatement and the
    /// whole notification should be skipped (not mapped, not emitted).
    pub(super) fn observe(&mut self, update: &SessionUpdate) -> bool {
        let SessionUpdate::AgentMessageChunk(chunk) = update else {
            // Any non-message-chunk update ends the current text block. The
            // event stream never interleaves ambient updates inside a streamed
            // block, so this is a safe block terminator.
            self.block = None;
            return false;
        };
        let ContentBlock::Text(t) = &chunk.content else {
            // Non-text content (image, audio) ends the text block.
            self.block = None;
            return false;
        };
        if t.text.is_empty() {
            // The adapter emits an empty chunk at each block start; treat it as
            // the boundary so adjacent blocks never merge in the accumulator.
            // Empty text renders nothing, so keep forwarding it.
            self.block = Some(AgentTextBlock {
                id: chunk.message_id.clone(),
                text: String::new(),
            });
            return false;
        }
        match &mut self.block {
            Some(block) if block.id == chunk.message_id => {
                // Same message: a genuine streamed delta (or both ids absent).
                // Never dropped.
                block.text.push_str(&t.text);
                false
            }
            Some(block) if block.text == t.text => {
                // Arm 1 already consumed the equal-id case, so the ids differ
                // here (present-and-different, or one side absent). A non-empty
                // chunk restating the whole block verbatim under a different id
                // is the leaked consolidated copy. Drop it and close the block.
                self.block = None;
                true
            }
            _ => {
                // A genuinely new block (id changed, text differs) or no open
                // block: start tracking fresh.
                self.block = Some(AgentTextBlock {
                    id: chunk.message_id.clone(),
                    text: t.text.clone(),
                });
                false
            }
        }
    }
}

/// Claude Code emits keepalive progress pings for long-running tools under a
/// derived id `<baseToolId>-heartbeat-<N>` (title/args/content all empty,
/// `InProgress` only). They are transport liveness, not tool boundaries, and
/// they never carry a start or completion. Forwarding them spawns a phantom
/// titleless "tool call" card per ping that never resolves (see #3084), so we
/// drop them at ingress.
/// ponytail: string-suffix match on a claude-side id contract; revisit if the
/// keepalive id scheme changes or claude starts shipping data under these ids.
pub(super) fn is_heartbeat_tool_call_id(id: &str) -> bool {
    id.rsplit_once("-heartbeat-")
        .is_some_and(|(_, suffix)| !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()))
}

/// Map an ACP `SessionUpdate` to the structured view's typed `Event`. Variants we
/// don't yet handle pass through as `RawAgentUpdate` so UI clients can at
/// least see them; we'll narrow these as the schema stabilises.
///
/// `profile` carries per-agent gates for claude-specific synthesis
/// (subagent linkage namespace, ExitPlanMode-to-Plan, ScheduleWakeup);
/// other agents pass these through as plain tool calls.
pub(super) fn map_update_to_events(
    update: SessionUpdate,
    profile: &'static agent_profiles::AgentProfile,
) -> Vec<Event> {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => match chunk.content {
            ContentBlock::Text(text) => {
                // /compact emits a plain text chunk ("Compacting completed.")
                // and a usage_update with used=0; no typed signal. Detect
                // the literal string the adapter uses and append a typed
                // SessionContextReset event so the structured view can render a
                // divider, otherwise the silent context replacement leaves
                // the chat looking unchanged while the model's view has
                // been swapped out underneath the user. See #1050.
                let mut events = vec![Event::AgentMessageChunk {
                    text: text.text.clone(),
                }];
                if is_compact_start(&text.text) {
                    // The adapter goes silent for 90 to 170 seconds from
                    // here. Publish the phase so the clients relabel the
                    // spinner and park follow-ups instead of reading the
                    // quiet as a wedge. Same marker the silent-orphan
                    // watchdog already latches (#2898). See #3219.
                    events.push(Event::ConversationCompactionStarted);
                }
                if is_compact_completion(&text.text) {
                    events.push(Event::ConversationCompacted);
                    // /compact wipes the model's tool-state alongside the
                    // chat history, so any TodoWrite plan it was tracking
                    // is gone from its perspective. The structured view plan strip
                    // (PlanStrip + sidebar PlanProgressMini) lives in our
                    // own event log though, so without this clear it keeps
                    // showing a plan Claude no longer remembers; the user
                    // then asks "resolve the first task" and Claude
                    // responds "no task list." Emit an empty PlanUpdated
                    // so the UI matches the model's actual context.
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
        // Replayed user turns (#2276): claude-agent-acp re-emits each prior
        // user message as a user_message_chunk during session/load. Live user
        // prompts are recorded by the send_prompt path as UserPromptSent, so
        // this only fires on replay; mapping it to UserPromptSent makes the
        // imported transcript show the user's bubbles. On a normal reattach
        // these are suppressed (already in the store) by is_transcript_event.
        SessionUpdate::UserMessageChunk(chunk) => match chunk.content {
            ContentBlock::Text(text) => vec![Event::UserPromptSent {
                text: text.text,
                attachments: Vec::new(),
                prompt_id: None,
            }],
            other => vec![raw_event(&other)],
        },
        SessionUpdate::AgentThoughtChunk(_) => vec![Event::ThinkingStarted],
        SessionUpdate::ToolCall(tc) => {
            let raw_args = tc.raw_input.clone().unwrap_or(serde_json::Value::Null);
            // Empty (not the literal "null") when the agent ships no
            // raw_input, so argless tool cards render a clean empty-state.
            // See #1713.
            let args_preview = preview_optional_args(tc.raw_input.as_ref());
            let parent_tool_call_id = profile.parent_tool_use_id_from_meta(&tc.meta);
            if let Some(parent) = parent_tool_call_id.as_deref() {
                // Breadcrumb so AOE_ACP_TRACE=1 sessions can verify the
                // subagent linkage round-trip (parent Task id → child
                // tool_call id) end-to-end. See #1041 layer C.
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
            // Codex (and any ACP agent) can attach structured file diffs to
            // the initial tool_call via `ToolCallContent::Diff`. Bridge them
            // onto the ToolCall so the edit card shows the path + preview
            // instead of "(unknown file)". See #1721.
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
            // claude-agent-acp routes Claude's built-in ExitPlanMode through
            // the tool channel (kind=switch_mode, plan markdown in
            // raw_input.plan) instead of the structured SessionUpdate::Plan
            // channel. Synthesise a PlanUpdated event so the structured view's
            // PlanStrip and the rest of the plan-aware UI light up. See
            // #1059. Gated on the agent's profile so codex / opencode /
            // gemini mode switches don't spuriously emit empty Plans.
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
            // The Claude Agent SDK's `ScheduleWakeup` tool sleeps the
            // session until `now + delaySeconds`, with `/loop` dynamic
            // mode self-firing a fresh prompt when the wake triggers.
            // Capture an absolute `at` timestamp here so the sidebar
            // countdown survives daemon restarts and never has to parse
            // the natural-language output string. See #1091. Gated on
            // the agent's profile (claude-only today) so coincidental
            // tool names on other agents don't fire a wakeup event.
            if profile.supports_wakeup_tools && tc.title == "ScheduleWakeup" {
                if let Some(event) = wakeup_event_from_raw(&raw_args) {
                    events.push(event);
                }
            }
            events
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let id = update.tool_call_id.0.to_string();
            // Drop claude keepalive heartbeats before they become an orphan
            // `ToolCallUpdated` that renders a phantom card. Gated on the
            // profile so another adapter that legitimately names a tool
            // `*-heartbeat-<N>` is not silenced. See #3084 and
            // `is_heartbeat_tool_call_id`.
            if profile.emits_heartbeat_keepalives && is_heartbeat_tool_call_id(&id) {
                return Vec::new();
            }
            let is_error = matches!(
                update.fields.status,
                Some(agent_client_protocol::schema::v1::ToolCallStatus::Failed)
            );
            let completed = matches!(
                update.fields.status,
                Some(agent_client_protocol::schema::v1::ToolCallStatus::Completed)
                    | Some(agent_client_protocol::schema::v1::ToolCallStatus::Failed)
            );
            // claude-agent-acp emits the initial `tool_call` frame
            // eagerly, often well before the underlying bash / read /
            // edit actually starts running. Use `status: InProgress` as
            // the canonical "running now" signal and re-stamp the
            // tool's `started_at` so the duration label measures real
            // tool runtime rather than adapter scheduling overhead.
            // See #1060.
            let in_progress = matches!(
                update.fields.status,
                Some(agent_client_protocol::schema::v1::ToolCallStatus::InProgress)
            );
            let content_text = update
                .fields
                .content
                .as_ref()
                .map(|blocks| extract_tool_content_text(blocks))
                .unwrap_or_default();
            // Codex emits `apply_patch` diffs on the in-progress and
            // completion updates, not only the initial tool_call. Pull any
            // Diff blocks off this frame so the edit card's path + preview
            // survive when they arrive late. `Some` here REPLACES the card's
            // diffs in the reducer; absent diff blocks stay `None` so a
            // text-only update can't wipe diffs from an earlier frame. See
            // #1721.
            let new_diffs = update
                .fields
                .content
                .as_ref()
                .and_then(|blocks| {
                    let diffs = extract_diffs_from_content(blocks);
                    (!diffs.is_empty()).then_some(diffs)
                })
                .or_else(|| write_diff_from_meta(&update.meta));
            // Drop an explicit JSON null so a late-arriving update never
            // patches the card's args with the literal "null"; leaving it
            // None means the reducer keeps whatever args it already has.
            // See #1713.
            let new_args_preview = update
                .fields
                .raw_input
                .as_ref()
                .filter(|value| !value.is_null())
                .map(preview_args);
            // Structured completion payload: media/resource blocks that the
            // text concat above drops. Only extracted on the terminal frame
            // (the card renders it once on completion). See #1818.
            let output_blocks = if completed {
                update
                    .fields
                    .content
                    .as_ref()
                    .map(|blocks| extract_tool_output_blocks(blocks))
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
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
                    started_at: if in_progress {
                        Some(chrono::Utc::now())
                    } else {
                        None
                    },
                    diffs: new_diffs,
                });
            }
            if completed {
                // An async sub-agent launch (Claude `Task` with isAsync)
                // completes immediately with the `Async agent launched
                // successfully` marker while the real work runs
                // off-protocol. Flag it so renderers draw a neutral
                // background-sub-agent card and drop the marker body
                // (which leaks an internal agent id). Same detector the
                // silent-orphan watchdog uses; this just forwards it to
                // the UI event stream.
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
                // The async sub-agent launch rides a metadata-only
                // ToolCallUpdate (`_meta.claudeCode.toolName == "Agent"`,
                // status "async_launched", no status/content/title), so it
                // lands here rather than the unknown-variant catch-all
                // below. Promote it to a typed BackgroundAgentLaunched so
                // the daemon tails the agent's transcript; otherwise pass
                // the raw payload through unchanged.
                let payload = serde_json::to_value(&update).unwrap_or(serde_json::Value::Null);
                match background_agent_launched_from_value(&payload) {
                    Some(event) => events.push(event),
                    None => events.push(Event::RawAgentUpdate { payload }),
                }
            }
            // claude-agent-acp emits the initial `tool_call` frame for
            // ScheduleWakeup with empty `raw_input`; the actual
            // `delaySeconds` lands on a subsequent `ToolCallUpdate`. The
            // emit path in the `ToolCall` branch above therefore never
            // sees real args and `wakeup_event_from_raw` returns None,
            // so re-check here when the update carries both the title
            // and a populated raw_input. See #1091. Gated on profile so
            // non-claude agents don't fire WakeupScheduled on coincidence.
            if profile.supports_wakeup_tools
                && matches!(update.fields.title.as_deref(), Some("ScheduleWakeup"))
            {
                if let Some(raw) = update.fields.raw_input.as_ref() {
                    if let Some(event) = wakeup_event_from_raw(raw) {
                        events.push(event);
                    }
                }
            }
            // The Claude SDK's `Monitor` tool is fire-and-forget: the tool
            // call completes immediately while the background watch keeps
            // running off-protocol, so the turn ends and the session sits
            // Idle while the monitor is still armed. Like ScheduleWakeup the
            // initial `tool_call` frame carries empty args; the real
            // `command` / `description` land on this update. Emit MonitorArmed
            // so the sidebar can flag the session instead of showing a plain
            // grey "idle" dot that looks dead. Gated on the same claude-only
            // profile flag as the wakeup tools.
            if profile.supports_wakeup_tools
                && matches!(update.fields.title.as_deref(), Some("Monitor"))
            {
                if let Some(raw) = update.fields.raw_input.as_ref() {
                    if let Some(event) = monitor_event_from_raw(raw) {
                        events.push(event);
                    }
                }
            }
            events
        }
        SessionUpdate::Plan(p) => {
            // Build the structured plan + a synthetic TodoWrite tool call
            // from the same entries. claude-agent-acp routes Claude's
            // TodoWrite through the structured `SessionUpdate::Plan`
            // channel (not the tool channel), so without this synthesis
            // the structured view's PlanStrip + sidebar light up but no tool
            // card ever renders; the user sees a plan appear "from
            // nowhere" and has no per-update record of which calls
            // produced which states. Emit a ToolCallStarted /
            // ToolCallCompleted pair shaped to match what the
            // TodoUpdateCard classifier in ToolCards.tsx expects
            // (`name = "TodoWrite"`, `args.todos = [...]`), one per
            // adapter update.
            // Append a session-local monotonic counter so two plan updates
            // arriving in the same millisecond don't share a synthetic ID
            // (which would collide in the acp_events row keys and
            // render as a single card instead of two).
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
            // Emit both events: CurrentModeChanged (the real id) and
            // a best-effort ModeChanged (for the legacy enum-based
            // UI, in case that path is still used somewhere).
            // Gemini surfaces its approval modes over `gemini --acp` with the
            // gemini-cli `ApprovalMode` ids (`auto_edit`, `yolo`); fold them
            // onto the existing semantic equivalents so a Gemini session is
            // classified the same as the claude-agent-acp modes. See #1819.
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
        // Ignore agent-pushed session titles. AoE owns automatic renaming via
        // `session::smart_rename`; forwarding Claude ACP title suggestions here
        // gives the session two competing auto-title writers.
        SessionUpdate::SessionInfoUpdate(_) => Vec::new(),
        // Variants we don't have a typed mapping for yet pass through as
        // RawAgentUpdate so the UI can render best-effort and we can
        // narrow these as we go.
        other => vec![raw_event(&other)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::acp_client::test_helpers::text_chunk;
    use crate::acp::acp_client::transcript_filter::{is_transcript_event, transcript_event_kind};
    use crate::acp::state::ConfigOptionCategory;

    #[test]
    fn heartbeat_tool_call_id_predicate() {
        assert!(is_heartbeat_tool_call_id("toolu_01ABC-heartbeat-0"));
        assert!(is_heartbeat_tool_call_id("toolu_01ABC-heartbeat-123"));
        // Not a heartbeat: non-numeric suffix, empty suffix, plain id, or the
        // marker embedded mid-id rather than as the trailing segment.
        assert!(!is_heartbeat_tool_call_id("toolu_01ABC-heartbeat-x"));
        assert!(!is_heartbeat_tool_call_id("toolu_01ABC-heartbeat-"));
        assert!(!is_heartbeat_tool_call_id("toolu_01ABC"));
        assert!(!is_heartbeat_tool_call_id("toolu_01ABC-heartbeat-1-extra"));
    }

    #[test]
    fn map_update_to_events_drops_heartbeat_keepalive() {
        // A claude keepalive ping (derived `-heartbeat-N` id, InProgress,
        // no title/args/content) must emit nothing; forwarding it as
        // ToolCallUpdated renders a phantom card that never completes. #3084.
        use agent_client_protocol::schema::v1::{
            SessionUpdate, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        };
        let fields = ToolCallUpdateFields::new().status(ToolCallStatus::InProgress);
        let update = ToolCallUpdate::new("toolu_01ABC-heartbeat-0", fields);
        let events = map_update_to_events(
            SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CLAUDE,
        );
        assert!(events.is_empty(), "heartbeat emitted events: {events:?}");
    }

    #[test]
    fn map_update_to_events_keeps_heartbeat_id_for_non_claude_profile() {
        // The drop is gated on the profile: a non-claude adapter that
        // legitimately uses a `-heartbeat-N` id must NOT be silenced. #3084.
        use agent_client_protocol::schema::v1::{
            SessionUpdate, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        };
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::InProgress)
            .title("real-tool-heartbeat-0".to_string());
        let update = ToolCallUpdate::new("real-tool-heartbeat-0", fields);
        let events = map_update_to_events(
            SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CODEX,
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::ToolCallUpdated { .. })),
            "non-claude heartbeat-suffixed id should be kept, got {events:?}"
        );
    }

    #[test]
    fn map_update_to_events_keeps_normal_in_progress_update() {
        // Control: a normal InProgress update on a non-heartbeat id still
        // emits ToolCallUpdated (the drop is scoped to keepalive ids only).
        use agent_client_protocol::schema::v1::{
            SessionUpdate, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        };
        let fields = ToolCallUpdateFields::new().status(ToolCallStatus::InProgress);
        let update = ToolCallUpdate::new("toolu_01ABC", fields);
        let events = map_update_to_events(
            SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CLAUDE,
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::ToolCallUpdated { .. })),
            "expected ToolCallUpdated, got {events:?}"
        );
    }

    #[test]
    fn map_update_to_events_threads_parent_tool_call_id() {
        use agent_client_protocol::schema::v1::{SessionUpdate, ToolCall as AcpToolCall};
        let mut meta = serde_json::Map::new();
        meta.insert(
            "claudeCode".to_string(),
            serde_json::json!({ "parentToolUseId": "tc-task-1" }),
        );
        let mut tc = AcpToolCall::new("tc-child-1", "Read");
        tc.raw_input = Some(serde_json::json!({"path": "x"}));
        tc.meta = Some(meta);
        let events = map_update_to_events(SessionUpdate::ToolCall(tc), &agent_profiles::CLAUDE);
        let started = events.iter().find_map(|e| match e {
            Event::ToolCallStarted { tool_call } => Some(tool_call),
            _ => None,
        });
        let started = started.expect("ToolCallStarted emitted");
        assert_eq!(started.parent_tool_call_id.as_deref(), Some("tc-task-1"),);
    }

    #[test]
    fn map_update_to_events_leaves_parent_none_when_meta_missing() {
        use agent_client_protocol::schema::v1::{SessionUpdate, ToolCall as AcpToolCall};
        let mut tc = AcpToolCall::new("tc-1", "Read");
        tc.raw_input = Some(serde_json::json!({"path": "x"}));
        let events = map_update_to_events(SessionUpdate::ToolCall(tc), &agent_profiles::CLAUDE);
        let started = events.iter().find_map(|e| match e {
            Event::ToolCallStarted { tool_call } => Some(tool_call),
            _ => None,
        });
        assert!(started.unwrap().parent_tool_call_id.is_none());
    }

    fn tool_update() -> SessionUpdate {
        use agent_client_protocol::schema::v1::ToolCall as AcpToolCall;
        SessionUpdate::ToolCall(AcpToolCall::new("t-dedup", "Read"))
    }

    #[test]
    fn dedup_drops_consolidated_restatement_after_deltas() {
        // The reported leak: empty marker + two streamed deltas sharing the
        // streamed id, then the whole block re-sent under a different id.
        let mut d = AgentMessageDedup::default();
        assert!(!d.observe(&text_chunk("", Some("m1"))));
        assert!(!d.observe(&text_chunk(
            "Concrete repro. Let me inspect the events around lgtm and",
            Some("m1")
        )));
        assert!(!d.observe(&text_chunk(
            " the \"Plan approved\" message in that session.",
            Some("m1")
        )));
        // Consolidated copy carries the mismatched id and restates the block.
        assert!(d.observe(&text_chunk(
            "Concrete repro. Let me inspect the events around lgtm and the \"Plan approved\" message in that session.",
            Some("m2")
        )));
    }

    #[test]
    fn dedup_drops_restatement_when_delta_ids_absent() {
        // The recurrence (sessions 0c425453, 614231): when `message_start`
        // carries no id, the streamed deltas arrive id-less, but the leaked
        // consolidated copy still carries a fallback uuid. The ids differ by
        // presence, so the verbatim restatement must still be dropped.
        let mut d = AgentMessageDedup::default();
        assert!(!d.observe(&text_chunk("", None)));
        assert!(!d.observe(&text_chunk("Getting the real failure log,", None)));
        assert!(!d.observe(&text_chunk(" not guessing this time.", None)));
        assert!(d.observe(&text_chunk(
            "Getting the real failure log, not guessing this time.",
            Some("uuid-1")
        )));
    }

    #[test]
    fn dedup_drops_single_delta_restatement() {
        let mut d = AgentMessageDedup::default();
        assert!(!d.observe(&text_chunk("hello world", Some("m1"))));
        assert!(d.observe(&text_chunk("hello world", Some("m2"))));
    }

    #[test]
    fn dedup_keeps_legitimate_repeated_same_id_delta() {
        // Two identical deltas that share a message id are genuine streamed
        // output ("haha"), not a restatement. Never dropped.
        let mut d = AgentMessageDedup::default();
        assert!(!d.observe(&text_chunk("", Some("m1"))));
        assert!(!d.observe(&text_chunk("ha", Some("m1"))));
        assert!(!d.observe(&text_chunk("ha", Some("m1"))));
    }

    #[test]
    fn dedup_resets_on_boundary_and_handles_adjacent_blocks() {
        let mut d = AgentMessageDedup::default();
        // Block 1: delta then restatement, dropped.
        assert!(!d.observe(&text_chunk("ab", Some("m1"))));
        assert!(d.observe(&text_chunk("ab", Some("m2"))));
        // A tool call ends the block.
        assert!(!d.observe(&tool_update()));
        // Block 2 reuses text "ab": the first chunk after the boundary must
        // not be mistaken for a restatement of the closed block.
        assert!(!d.observe(&text_chunk("ab", Some("m3"))));
        assert!(d.observe(&text_chunk("ab", Some("m4"))));
    }

    #[test]
    fn dedup_never_drops_when_ids_absent() {
        // Without message ids the delta-vs-restatement distinction is
        // ambiguous; degrade to never-drop so real output is never corrupted.
        let mut d = AgentMessageDedup::default();
        assert!(!d.observe(&text_chunk("", None)));
        assert!(!d.observe(&text_chunk("done", None)));
        assert!(!d.observe(&text_chunk("done", None)));
    }

    #[test]
    fn dedup_reset_forgets_in_flight_block() {
        // Mirrors the suppression path: reset() between a block's deltas and
        // its restatement means the restatement is treated as a fresh block.
        let mut d = AgentMessageDedup::default();
        assert!(!d.observe(&text_chunk("ab", Some("m1"))));
        d.reset();
        assert!(!d.observe(&text_chunk("ab", Some("m2"))));
    }

    #[test]
    fn map_update_to_events_does_not_link_parent_for_unverified_agents() {
        use agent_client_protocol::schema::v1::{SessionUpdate, ToolCall as AcpToolCall};
        let mut meta = serde_json::Map::new();
        meta.insert(
            "claudeCode".to_string(),
            serde_json::json!({ "parentToolUseId": "tc-task-1" }),
        );
        let mut tc = AcpToolCall::new("tc-child-1", "Read");
        tc.raw_input = Some(serde_json::json!({"path": "x"}));
        tc.meta = Some(meta);
        // Codex profile lists no parent_meta_namespaces, so the linkage
        // doesn't render even when claude's namespace happens to be on
        // the wire.
        let events = map_update_to_events(SessionUpdate::ToolCall(tc), &agent_profiles::CODEX);
        let started = events.iter().find_map(|e| match e {
            Event::ToolCallStarted { tool_call } => Some(tool_call),
            _ => None,
        });
        assert!(started.unwrap().parent_tool_call_id.is_none());
    }

    #[test]
    fn is_compact_completion_matches_adapter_string() {
        assert!(is_compact_completion("Compacting completed."));
        assert!(is_compact_completion("\n\nCompacting completed.\n"));
        assert!(!is_compact_completion("Compacting..."));
        assert!(!is_compact_completion("compact done"));
        assert!(!is_compact_completion(""));
    }

    #[test]
    fn is_compact_start_matches_adapter_string() {
        assert!(is_compact_start("Compacting..."));
        assert!(is_compact_start("\n\nCompacting...\n"));
        // The completion marker must not be read as a fresh start.
        assert!(!is_compact_start("Compacting completed."));
        assert!(!is_compact_start("compacting"));
        assert!(!is_compact_start(""));
    }

    #[test]
    fn map_tool_call_update_meta_emits_background_agent_launched() {
        // The real path: the async launch arrives as a metadata-only
        // ToolCallUpdate (no status/content/title), carrying the Agent
        // payload under `_meta.claudeCode`. It must map to a typed
        // BackgroundAgentLaunched, not a raw passthrough. This is the
        // path the unit test on the helper alone did not cover.
        use agent_client_protocol::schema::v1::{
            SessionUpdate, ToolCallUpdate, ToolCallUpdateFields,
        };
        let mut meta = serde_json::Map::new();
        meta.insert(
            "claudeCode".to_string(),
            serde_json::json!({
                "toolName": "Agent",
                "toolResponse": {
                    "agentId": "a6654829ea0a19032",
                    "description": "grep tmux mentions repo-wide",
                    "prompt": "Grep the repo for tmux.",
                    "resolvedModel": "claude-opus-4-8[1m]",
                    "outputFile": "/tmp/x/tasks/a6654829ea0a19032.output",
                    "status": "async_launched"
                }
            }),
        );
        let mut update = ToolCallUpdate::new("toolu_01HzYCZK", ToolCallUpdateFields::new());
        update.meta = Some(meta);
        let events = map_update_to_events(
            SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CLAUDE,
        );
        match events.iter().find_map(|e| match e {
            Event::BackgroundAgentLaunched {
                agent_id,
                description,
                output_file,
                ..
            } => Some((agent_id.clone(), description.clone(), output_file.clone())),
            _ => None,
        }) {
            Some((id, desc, out)) => {
                assert_eq!(id, "a6654829ea0a19032");
                assert_eq!(desc, "grep tmux mentions repo-wide");
                assert!(out.ends_with(".output"));
            }
            None => panic!("expected BackgroundAgentLaunched, got {events:?}"),
        }
        // It must NOT also leak a RawAgentUpdate for the same payload.
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::RawAgentUpdate { .. })),
            "async launch should not also pass through as RawAgentUpdate"
        );
    }

    #[test]
    fn map_tool_call_update_completed_carries_content() {
        use agent_client_protocol::schema::v1::{
            Content, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        };
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .content(vec![ToolCallContent::Content(Content::new(
                "abc1234 first commit",
            ))]);
        let update = ToolCallUpdate::new("tc-1", fields);
        let events = map_update_to_events(
            SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CLAUDE,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            Event::ToolCallCompleted {
                tool_call_id,
                is_error,
                content,
                ..
            } => {
                assert_eq!(tool_call_id, "tc-1");
                assert!(!*is_error);
                assert_eq!(content, "abc1234 first commit");
            }
            other => panic!("expected ToolCallCompleted, got {other:?}"),
        }
    }

    #[test]
    fn map_tool_call_update_flags_async_subagent_launch() {
        // Claude's async `Task` tool completes immediately with the SDK
        // marker while the sub-agent runs off-protocol. The completion
        // event must carry async_subagent so renderers draw a background
        // card and drop the marker body (it leaks an internal agent id).
        use agent_client_protocol::schema::v1::{
            Content, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        };
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .content(vec![ToolCallContent::Content(Content::new(
                "Async agent launched successfully\nagentId: ae6f0567246843e25 (internal ID)",
            ))]);
        let update = ToolCallUpdate::new("tc-async", fields);
        let events = map_update_to_events(
            SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CLAUDE,
        );
        match events.iter().find_map(|e| match e {
            Event::ToolCallCompleted { async_subagent, .. } => Some(*async_subagent),
            _ => None,
        }) {
            Some(flag) => assert!(flag, "async sub-agent launch must set async_subagent"),
            None => panic!("expected a ToolCallCompleted event, got {events:?}"),
        }
    }

    #[test]
    fn map_tool_call_update_normal_completion_is_not_async_subagent() {
        use agent_client_protocol::schema::v1::{
            Content, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        };
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .content(vec![ToolCallContent::Content(Content::new("done"))]);
        let update = ToolCallUpdate::new("tc-normal", fields);
        let events = map_update_to_events(
            SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CLAUDE,
        );
        match events.iter().find_map(|e| match e {
            Event::ToolCallCompleted { async_subagent, .. } => Some(*async_subagent),
            _ => None,
        }) {
            Some(flag) => assert!(!flag, "normal completion must not set async_subagent"),
            None => panic!("expected a ToolCallCompleted event, got {events:?}"),
        }
    }

    #[test]
    fn map_user_message_chunk_becomes_user_prompt_sent() {
        // Imported sessions replay prior user turns as user_message_chunk
        // (#2276); they must map to UserPromptSent so the user's bubbles
        // render, not get dropped to a raw event.
        use agent_client_protocol::schema::v1::{ContentBlock, ContentChunk, TextContent};
        let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new("hello from the past")));
        let events = map_update_to_events(
            SessionUpdate::UserMessageChunk(chunk),
            &agent_profiles::CLAUDE,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            Event::UserPromptSent {
                text, attachments, ..
            } => {
                assert_eq!(text, "hello from the past");
                assert!(attachments.is_empty());
            }
            other => panic!("expected UserPromptSent, got {other:?}"),
        }
    }

    /// `/compact` surfaces only as text chunks, so the mapper turns both
    /// markers into typed lifecycle events alongside the visible chunk.
    /// The start half is what tells the clients to stop reading the 90 to
    /// 170 second silence as a wedged agent (#3219); the completion half
    /// keeps its pre-existing divider plus plan wipe (#1050).
    #[test]
    fn map_agent_message_chunk_emits_compaction_lifecycle_events() {
        use agent_client_protocol::schema::v1::{ContentBlock, ContentChunk, TextContent};
        // (chunk text, expected event kinds after the chunk itself)
        let cases: [(&str, &[&str]); 4] = [
            ("just some prose", &[]),
            ("Compacting...", &["conversation_compaction_started"]),
            (
                "\n\nCompacting completed.",
                &["conversation_compacted", "plan_updated"],
            ),
            // Near-miss prose must not latch the phase.
            ("I am compacting the list", &[]),
        ];
        for (text, expected_tail) in cases {
            let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
            let events = map_update_to_events(
                SessionUpdate::AgentMessageChunk(chunk),
                &agent_profiles::CLAUDE,
            );
            let kinds: Vec<&str> = events.iter().map(transcript_event_kind).collect();
            let mut want = vec!["agent_message_chunk"];
            want.extend_from_slice(expected_tail);
            assert_eq!(kinds, want, "chunk {text:?}");
        }
    }

    /// The wire form the web reducer matches on. `Event` is untagged for
    /// unit variants, so a rename here silently breaks the TypeScript
    /// side rather than failing to compile.
    #[test]
    fn compaction_started_serializes_as_a_bare_string() {
        assert_eq!(
            serde_json::to_string(&Event::ConversationCompactionStarted).unwrap(),
            "\"ConversationCompactionStarted\""
        );
    }

    /// Both halves of a compaction are synthesized from an
    /// `AgentMessageChunk` that the suppression window drops, so they
    /// must drop with it. Before #3219 the completion leaked through and
    /// re-ran its side effects on every reattach.
    #[test]
    fn compaction_events_are_suppressed_during_load_replay() {
        // (event, suppressed during the post-session/load window)
        let cases = [
            (Event::ConversationCompactionStarted, true),
            (Event::ConversationCompacted, true),
            (
                Event::AgentMessageChunk {
                    text: "Compacting...".into(),
                },
                true,
            ),
            // Ambient and lifecycle state must still reach the UI on
            // resume, or the composer footer stays stale.
            (Event::SessionCleared, false),
            (
                Event::Stopped {
                    reason: "prompt_complete".into(),
                },
                false,
            ),
        ];
        for (event, expected) in cases {
            assert_eq!(
                is_transcript_event(&event),
                expected,
                "{}",
                transcript_event_kind(&event)
            );
        }
    }

    fn mode_from_current_mode_update(id: &str) -> SessionMode {
        use agent_client_protocol::schema::v1::CurrentModeUpdate;
        let events = map_update_to_events(
            SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(id.to_string())),
            &agent_profiles::CLAUDE,
        );
        // The arm always emits the raw id alongside the legacy enum, in order.
        match events.as_slice() {
            [Event::CurrentModeChanged { current_mode_id }, Event::ModeChanged { mode }] => {
                assert_eq!(current_mode_id, id, "raw mode id must be preserved");
                *mode
            }
            other => panic!("expected [CurrentModeChanged, ModeChanged], got {other:?}"),
        }
    }

    #[test]
    fn current_mode_update_classifies_gemini_mode_ids() {
        // Gemini-cli ApprovalMode ids surfaced over `gemini --acp`. See #1819.
        assert_eq!(
            mode_from_current_mode_update("yolo"),
            SessionMode::BypassPermissions
        );
        assert_eq!(
            mode_from_current_mode_update("auto_edit"),
            SessionMode::AcceptEdits
        );
        assert_eq!(
            mode_from_current_mode_update("autoEdit"),
            SessionMode::AcceptEdits
        );
    }

    #[test]
    fn current_mode_update_keeps_existing_mode_ids() {
        // Regression guard: non-Gemini classification is unchanged.
        assert_eq!(
            mode_from_current_mode_update("default"),
            SessionMode::Default
        );
        assert_eq!(mode_from_current_mode_update("plan"), SessionMode::Plan);
        assert_eq!(
            mode_from_current_mode_update("accept_edits"),
            SessionMode::AcceptEdits
        );
        assert_eq!(
            mode_from_current_mode_update("acceptEdits"),
            SessionMode::AcceptEdits
        );
        assert_eq!(
            mode_from_current_mode_update("bypass_permissions"),
            SessionMode::BypassPermissions
        );
        assert_eq!(
            mode_from_current_mode_update("bypassPermissions"),
            SessionMode::BypassPermissions
        );
        // Unknown ids still fall back to Default.
        assert_eq!(
            mode_from_current_mode_update("some_future_mode"),
            SessionMode::Default
        );
    }

    #[test]
    fn map_tool_call_update_in_progress_with_content_emits_streaming_event() {
        use agent_client_protocol::schema::v1::{
            Content, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        };
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::InProgress)
            .content(vec![ToolCallContent::Content(Content::new(
                "partial output",
            ))]);
        let update = ToolCallUpdate::new("tc-2", fields);
        let events = map_update_to_events(
            SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CLAUDE,
        );
        // InProgress now emits a ToolCallUpdated re-stamping started_at
        // (#1060 follow-up) plus the streaming ToolCallContent.
        assert_eq!(events.len(), 2);
        match &events[0] {
            Event::ToolCallUpdated {
                tool_call_id,
                started_at,
                ..
            } => {
                assert_eq!(tool_call_id, "tc-2");
                assert!(started_at.is_some());
            }
            other => panic!("expected ToolCallUpdated, got {other:?}"),
        }
        match &events[1] {
            Event::ToolCallContent {
                tool_call_id,
                content,
            } => {
                assert_eq!(tool_call_id, "tc-2");
                assert_eq!(content, "partial output");
            }
            other => panic!("expected ToolCallContent, got {other:?}"),
        }
    }

    #[test]
    fn map_tool_call_update_in_progress_restamps_started_at() {
        use agent_client_protocol::schema::v1::{
            ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        };
        let fields = ToolCallUpdateFields::new().status(ToolCallStatus::InProgress);
        let update = ToolCallUpdate::new("tc-3", fields);
        let events = map_update_to_events(
            SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CLAUDE,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            Event::ToolCallUpdated {
                tool_call_id,
                started_at,
                title,
                args_preview,
                diffs,
            } => {
                assert_eq!(tool_call_id, "tc-3");
                assert!(
                    started_at.is_some(),
                    "InProgress must carry a re-stamped started_at"
                );
                assert!(title.is_none());
                assert!(args_preview.is_none());
                assert!(diffs.is_none());
            }
            other => panic!("expected ToolCallUpdated, got {other:?}"),
        }
    }

    #[test]
    fn map_tool_call_bridges_diff_content_onto_started_tool() {
        // Codex attaches the apply_patch diff to the initial `tool_call`
        // frame as ToolCallContent::Diff. The edit card reads path + preview
        // from ToolCall.diffs, so it must survive ingest. See #1721.
        use agent_client_protocol::schema::v1::{Diff, ToolCall, ToolCallContent, ToolKind};
        let mut tc = ToolCall::new("tc-edit-1", "Edit src/foo.rs");
        tc.kind = ToolKind::Edit;
        tc.content = vec![ToolCallContent::Diff(
            Diff::new("src/foo.rs", "new").old_text("old"),
        )];
        let events = map_update_to_events(SessionUpdate::ToolCall(tc), &agent_profiles::CODEX);
        match &events[0] {
            Event::ToolCallStarted { tool_call } => {
                assert_eq!(tool_call.diffs.len(), 1);
                assert_eq!(tool_call.diffs[0].path, "src/foo.rs");
                assert_eq!(tool_call.diffs[0].new_text.as_deref(), Some("new"));
            }
            other => panic!("expected ToolCallStarted, got {other:?}"),
        }
    }

    #[test]
    fn map_tool_call_update_carries_diff_content() {
        // Codex also re-sends the diff on the in-progress and completion
        // updates; those must reach the reducer via ToolCallUpdated.diffs so
        // a late-arriving diff still lands on the card. See #1721.
        use agent_client_protocol::schema::v1::{
            Diff, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        };
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .content(vec![ToolCallContent::Diff(
                Diff::new("src/foo.rs", "new").old_text("old"),
            )]);
        let update = ToolCallUpdate::new("tc-edit-1", fields);
        let events = map_update_to_events(
            SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CODEX,
        );
        let updated = events
            .iter()
            .find_map(|e| match e {
                Event::ToolCallUpdated { diffs, .. } => Some(diffs),
                _ => None,
            })
            .expect("a ToolCallUpdated event must be emitted for a diff-only update");
        let diffs = updated.as_ref().expect("diffs must be Some");
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].path, "src/foo.rs");
    }

    #[test]
    fn map_tool_call_update_text_only_leaves_diffs_none() {
        // A text-only update must not carry Some([]) (which would wipe an
        // earlier frame's diffs in the reducer). See #1721.
        use agent_client_protocol::schema::v1::{
            Content, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        };
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .content(vec![ToolCallContent::Content(Content::new("done"))]);
        let update = ToolCallUpdate::new("tc-edit-1", fields);
        let events = map_update_to_events(
            SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CODEX,
        );
        for e in &events {
            if let Event::ToolCallUpdated { diffs, .. } = e {
                assert!(diffs.is_none(), "text-only update must leave diffs None");
            }
        }
    }

    #[test]
    fn map_tool_call_update_emits_wakeup_when_title_and_raw_input_land_in_update() {
        // claude-agent-acp sends the initial `ToolCall` for ScheduleWakeup
        // with `raw_input = {}`; the real `delaySeconds` arrives on a
        // follow-up `ToolCallUpdate` that carries both `title` and
        // `raw_input`. The initial-path emit therefore returns `None`
        // from `wakeup_event_from_raw`, and the update-path must pick up
        // the slack so `Event::WakeupScheduled` lands in the store
        // (sidebar `⏰ in Nm` chip + structured view "Asleep until…" banner
        // depend on it). Regression for #1091.
        use agent_client_protocol::schema::v1::{ToolCallUpdate, ToolCallUpdateFields};
        let fields = ToolCallUpdateFields::new()
            .title("ScheduleWakeup".to_string())
            .raw_input(serde_json::json!({
                "delaySeconds": 600,
                "prompt": "Wake-up fired. Confirm.",
                "reason": "Test 10-minute wake-up card countdown",
            }));
        let update = ToolCallUpdate::new("toolu_test", fields);
        let events = map_update_to_events(
            SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CLAUDE,
        );
        let wakeup = events
            .iter()
            .find(|e| matches!(e, Event::WakeupScheduled { .. }))
            .expect(
                "ToolCallUpdate with title=ScheduleWakeup + delaySeconds must emit WakeupScheduled",
            );
        match wakeup {
            Event::WakeupScheduled { at, reason } => {
                let delta = (*at - chrono::Utc::now()).num_seconds();
                assert!(
                    (590..=610).contains(&delta),
                    "wakeup `at` should be ~600s in the future, got {delta}s",
                );
                assert_eq!(
                    reason.as_deref(),
                    Some("Test 10-minute wake-up card countdown"),
                );
            }
            other => panic!("expected WakeupScheduled, got {other:?}"),
        }
    }

    #[test]
    fn map_tool_call_update_skips_wakeup_when_raw_input_missing() {
        // Title-only update (the initial frame's mirror, before
        // raw_input arrives) must NOT emit a WakeupScheduled, otherwise
        // we'd publish a "wakeup at epoch zero" placeholder.
        use agent_client_protocol::schema::v1::{ToolCallUpdate, ToolCallUpdateFields};
        let fields = ToolCallUpdateFields::new().title("ScheduleWakeup".to_string());
        let update = ToolCallUpdate::new("toolu_test", fields);
        let events = map_update_to_events(
            SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CLAUDE,
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::WakeupScheduled { .. })),
            "no WakeupScheduled should fire without delaySeconds",
        );
    }

    #[test]
    fn map_tool_call_update_emits_monitor_armed_when_title_and_args_land() {
        // Mirrors the ScheduleWakeup path: the Monitor tool's initial
        // `ToolCall` frame has empty args; the real `command` /
        // `description` arrive on a follow-up `ToolCallUpdate`. That update
        // must emit MonitorArmed so the sidebar shows a "monitoring" badge
        // instead of a plain grey idle dot.
        use agent_client_protocol::schema::v1::{ToolCallUpdate, ToolCallUpdateFields};
        let fields = ToolCallUpdateFields::new()
            .title("Monitor".to_string())
            .raw_input(serde_json::json!({
                "command": "until cargo clippy; do sleep 5; done",
                "description": "clippy passes",
                "timeout_ms": 600000,
                "persistent": false,
            }));
        let update = ToolCallUpdate::new("toolu_test", fields);
        let events = map_update_to_events(
            SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CLAUDE,
        );
        let armed = events
            .iter()
            .find(|e| matches!(e, Event::MonitorArmed { .. }))
            .expect("ToolCallUpdate with title=Monitor + args must emit MonitorArmed");
        match armed {
            Event::MonitorArmed { description } => {
                assert_eq!(description.as_deref(), Some("clippy passes"));
            }
            other => panic!("expected MonitorArmed, got {other:?}"),
        }
    }

    #[test]
    fn map_tool_call_update_skips_monitor_when_args_empty() {
        // The initial title-only / empty-args frame must NOT arm the badge;
        // only the populated follow-up update does.
        use agent_client_protocol::schema::v1::{ToolCallUpdate, ToolCallUpdateFields};
        let fields = ToolCallUpdateFields::new()
            .title("Monitor".to_string())
            .raw_input(serde_json::json!({}));
        let update = ToolCallUpdate::new("toolu_test", fields);
        let events = map_update_to_events(
            SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CLAUDE,
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Event::MonitorArmed { .. })),
            "no MonitorArmed should fire without command or description",
        );
    }

    #[test]
    fn map_session_info_update_ignores_pushed_title() {
        use agent_client_protocol::schema::v1::SessionInfoUpdate;
        let info = SessionInfoUpdate::new().title("Fix the flaky test".to_string());
        let events = map_update_to_events(
            SessionUpdate::SessionInfoUpdate(info),
            &agent_profiles::CLAUDE,
        );
        assert!(events.is_empty());
    }

    #[test]
    fn map_session_info_update_without_title_emits_nothing() {
        use agent_client_protocol::schema::v1::SessionInfoUpdate;
        // Null/undefined title (e.g. a timestamp-only update) yields no event.
        let info = SessionInfoUpdate::new().updated_at("2026-06-25T00:00:00Z".to_string());
        let events = map_update_to_events(
            SessionUpdate::SessionInfoUpdate(info),
            &agent_profiles::CLAUDE,
        );
        assert!(events.is_empty());
    }

    #[test]
    fn map_usage_update_emits_typed_usage_event() {
        use agent_client_protocol::schema::v1::{Cost, UsageUpdate};
        let u = UsageUpdate::new(12_345, 200_000).cost(Cost::new(0.42, "USD"));
        let events = map_update_to_events(SessionUpdate::UsageUpdate(u), &agent_profiles::CLAUDE);
        assert_eq!(events.len(), 1);
        match &events[0] {
            Event::UsageUpdated { usage } => {
                assert_eq!(usage.used, 12_345);
                assert_eq!(usage.size, 200_000);
                let cost = usage.cost.as_ref().expect("cost present");
                assert!((cost.amount - 0.42).abs() < f64::EPSILON);
                assert_eq!(cost.currency, "USD");
            }
            other => panic!("expected UsageUpdated, got {other:?}"),
        }
    }

    #[test]
    fn map_available_commands_update_emits_typed_event() {
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
        let update = AvailableCommandsUpdate::new(cmds);
        let events = map_update_to_events(
            SessionUpdate::AvailableCommandsUpdate(update),
            &agent_profiles::CLAUDE,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            Event::AvailableCommandsUpdated { commands } => {
                assert_eq!(commands.len(), 2);
                assert_eq!(commands[0].name, "review");
                assert!(commands[0].accepts_input);
                assert_eq!(commands[1].name, "clear");
                assert!(!commands[1].accepts_input);
            }
            other => panic!("expected AvailableCommandsUpdated, got {other:?}"),
        }
    }

    #[test]
    fn map_config_option_update_emits_typed_event_with_categories() {
        use agent_client_protocol::schema::v1::{
            ConfigOptionUpdate, SessionConfigKind, SessionConfigOption,
            SessionConfigOptionCategory, SessionConfigSelect, SessionConfigSelectOption,
            SessionConfigSelectOptions,
        };
        let model_option = SessionConfigOption::new(
            "model",
            "Model",
            SessionConfigKind::Select(SessionConfigSelect::new(
                "claude-opus-4-7",
                SessionConfigSelectOptions::Ungrouped(vec![
                    SessionConfigSelectOption::new("claude-opus-4-7", "Claude Opus 4.7"),
                    SessionConfigSelectOption::new("claude-sonnet-4-6", "Claude Sonnet 4.6"),
                ]),
            )),
        )
        .category(SessionConfigOptionCategory::Model);
        let effort_option = SessionConfigOption::new(
            "effort",
            "Reasoning Effort",
            SessionConfigKind::Select(SessionConfigSelect::new(
                "default",
                SessionConfigSelectOptions::Ungrouped(vec![
                    SessionConfigSelectOption::new("default", "Default"),
                    SessionConfigSelectOption::new("high", "High"),
                ]),
            )),
        )
        .category(SessionConfigOptionCategory::ThoughtLevel);
        let mode_option = SessionConfigOption::new(
            "mode",
            "Mode",
            SessionConfigKind::Select(SessionConfigSelect::new(
                "default",
                SessionConfigSelectOptions::Ungrouped(vec![
                    SessionConfigSelectOption::new("default", "Default"),
                    SessionConfigSelectOption::new("plan", "Plan"),
                ]),
            )),
        )
        .category(SessionConfigOptionCategory::Mode);
        let update = ConfigOptionUpdate::new(vec![model_option, effort_option, mode_option]);

        let events = map_update_to_events(
            SessionUpdate::ConfigOptionUpdate(update),
            &agent_profiles::CLAUDE,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            Event::ConfigOptionsUpdated { options } => {
                assert_eq!(options.len(), 3);
                assert_eq!(options[0].id, "model");
                assert_eq!(options[0].category, ConfigOptionCategory::Model);
                assert_eq!(options[0].current_value, "claude-opus-4-7");
                assert_eq!(options[0].options.len(), 2);
                assert_eq!(options[1].category, ConfigOptionCategory::ThoughtLevel);
                assert_eq!(options[1].current_value, "default");
                assert_eq!(options[2].category, ConfigOptionCategory::Mode);
            }
            other => panic!("expected ConfigOptionsUpdated, got {other:?}"),
        }
    }

    #[test]
    fn map_config_option_preserves_unknown_category_name() {
        // Forward-compat path for #1563: a category name aoe doesn't
        // recognize arrives via the upstream untagged `Other(String)`
        // arm. It must pass through as `Other(<name>)` and the option
        // must not be dropped from the descriptor list. (The wildcard
        // `_` arm that warns fires only for a genuinely new *named*
        // upstream variant, which cannot be constructed against the
        // current `#[non_exhaustive]` schema, so it is verified by
        // inspection rather than a unit test.)
        use agent_client_protocol::schema::v1::{
            ConfigOptionUpdate, SessionConfigKind, SessionConfigOption,
            SessionConfigOptionCategory, SessionConfigSelect, SessionConfigSelectOption,
            SessionConfigSelectOptions,
        };
        let unknown = SessionConfigOption::new(
            "future",
            "Future Selector",
            SessionConfigKind::Select(SessionConfigSelect::new(
                "a",
                SessionConfigSelectOptions::Ungrouped(vec![SessionConfigSelectOption::new(
                    "a", "A",
                )]),
            )),
        )
        .category(SessionConfigOptionCategory::Other(
            "future_category".to_string(),
        ));
        let update = ConfigOptionUpdate::new(vec![unknown]);

        let events = map_update_to_events(
            SessionUpdate::ConfigOptionUpdate(update),
            &agent_profiles::CLAUDE,
        );
        assert_eq!(events.len(), 1);
        match &events[0] {
            Event::ConfigOptionsUpdated { options } => {
                assert_eq!(
                    options.len(),
                    1,
                    "unknown-category option must not be dropped"
                );
                assert_eq!(options[0].id, "future");
                assert_eq!(
                    options[0].category,
                    ConfigOptionCategory::Other("future_category".to_string()),
                );
            }
            other => panic!("expected ConfigOptionsUpdated, got {other:?}"),
        }
    }
}
