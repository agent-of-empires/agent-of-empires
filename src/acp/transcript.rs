//! `TranscriptModel`: the server-side render model for the structured-view transcript.

use std::collections::{HashMap, HashSet};

use crate::time::Timestamp;
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use super::elicitations::ElicitationAnswer;
use super::state::{
    notice_id, DiffComment, DiffPreview, Event, ToolCall, ToolOutputBlock, ARGS_CUT_MARKER,
};
use crate::daemon::PromptAttachmentRef;

/// Reasons with this prefix are hidden before the first prompt, since a never-prompted
/// session can legitimately fail `session/load`.
pub(crate) const LOAD_FAILED_PREFIX: &str = "session/load failed";

/// One renderable row of the transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptRow {
    pub id: String,
    /// Journal sequence that created this row; patches retain this position.
    #[serde(default)]
    pub seq: u64,
    /// Certified history boundary on a session-clear row, absent for legacy clears.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clear_after_seq: Option<u64>,
    pub group_id: String,
    pub kind: TranscriptRowKind,
    pub at: Timestamp,
    pub text: String,
    /// Set on tool-lifecycle rows so a client pairs a completion with its start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Full tool payload on `tool_start` rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<ToolCall>,
    /// Structured completion payload on `tool_complete` / `tool_error` rows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output: Vec<ToolOutputBlock>,
    /// Attachment metadata on a `user_prompt` row; bytes are fetched lazily.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<PromptAttachmentRef>,
    /// Plugin that sent a `user_prompt` row; absent when the user typed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_plugin: Option<String>,
    /// Ids of other queued rows folded into a `user_prompt` row, so a client that
    /// only sees this row (snapshot or replay) still settles their optimistic rows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub merged_prompt_ids: Vec<String>,
    /// Structured payload on a `user_diff_comments` row; `text` holds the markdown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_comments: Option<DiffCommentsPayload>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub elicitation_answers: Vec<ElicitationAnswer>,
    /// A `tool_complete` row that launched an async sub-agent.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub async_subagent: bool,
    /// A `message` or `thinking` row that opens a new agent message, so a
    /// client keeps it apart from the previous one's text.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub message_start: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptRowKind {
    ToolStart,
    ToolComplete,
    ToolError,
    ToolStopped,
    Message,
    /// Summarized model thinking; consecutive rows of one group form one block.
    Thinking,
    UserPrompt,
    UserDiffComments,
    ElicitationAnswered,
    ContextReset,
    SessionCleared,
    Compacted,
    /// The agent's retained summary; `text` holds its markdown.
    CompactionSummary,
    Summary,
    /// An error or lifecycle notice the user needs in the timeline.
    Notice,
    /// An agent session advisory. Unlike `Notice`, every surface keeps it in
    /// the timeline, since its banner is capped and retired by the next turn.
    Advisory,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffCommentsPayload {
    pub intro: String,
    pub outro: String,
    pub is_multi_repo: bool,
    pub comments: Vec<DiffComment>,
}

/// An incremental change to the row list, emitted per event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TranscriptDelta {
    Append(TranscriptRow),
    Patch {
        id: String,
        row: TranscriptRow,
    },
    /// A row was removed (an AskUserQuestion card superseded by its form).
    Remove(String),
}

/// The row carries no severity field, so the text spells it out.
fn session_notice_text(severity: &str, title: &str, description: &Option<String>) -> String {
    match description
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        Some(description) => format!("{severity}: {title}: {description}"),
        None => format!("{severity}: {title}"),
    }
}

#[derive(Debug, Default)]
pub(crate) struct RetainedTranscript {
    pub rows: Vec<TranscriptRow>,
    /// Row id to the seq that appended it.
    pub row_seq: HashMap<String, u64>,
    /// Tool call id to the seq of its latest start.
    pub tool_start_seq: HashMap<String, u64>,
    pub unfinished_outputs: Vec<UnfinishedToolOutput>,
}

#[derive(Debug, Clone)]
pub(crate) struct UnfinishedToolOutput {
    pub tool_call_id: String,
    pub text: String,
    first_seq: u64,
}

#[derive(Debug, Clone, Default)]
struct BufferedToolOutput {
    first_seq: Option<u64>,
    text: String,
}

impl BufferedToolOutput {
    fn into_unfinished(self, tool_call_id: &str) -> Option<UnfinishedToolOutput> {
        if self.text.is_empty() {
            return None;
        }
        Some(UnfinishedToolOutput {
            tool_call_id: tool_call_id.to_owned(),
            text: self.text,
            first_seq: self.first_seq?,
        })
    }
}

/// Projection lifecycle is independent of whether its start row is in this page.
#[derive(Debug, Clone)]
enum ToolLifecycle {
    Buffered(BufferedToolOutput),
    Open(BufferedToolOutput),
    Terminal(BufferedToolOutput),
}

impl ToolLifecycle {
    fn finish(&mut self) -> BufferedToolOutput {
        match std::mem::replace(self, Self::Terminal(BufferedToolOutput::default())) {
            Self::Buffered(output) | Self::Open(output) | Self::Terminal(output) => output,
        }
    }
}

pub(crate) fn closes_tool_lifecycle(
    event: &Event,
    metadata: &crate::acp::event_sender::EventMetadata,
) -> bool {
    matches!(
        event,
        Event::AgentStartupError { .. }
            | Event::IncompatibleAgent { .. }
            | Event::AgentSwitched { .. }
    ) || (matches!(event, Event::Stopped { .. })
        && metadata.operation.is_none()
        && metadata.native_autonomous.is_none())
}

/// Folds the ACP `Event` stream into an ordered [`TranscriptRow`] list.
#[derive(Debug, Clone, Default)]
pub struct TranscriptModel {
    rows: Vec<TranscriptRow>,
    row_ids: HashSet<String>,
    /// Seq at which each row was appended, for clients that order by event order.
    row_seq: HashMap<String, u64>,
    /// Seq of each tool id's latest start; a reused id keeps its first row but restarts here.
    tool_start_seq: HashMap<String, u64>,
    /// Tool lifecycle and buffered output, in opening order, including skipped starts.
    tools: IndexMap<String, ToolLifecycle>,
    /// Orphan snapshots remain readable but never feed subsequent tool invocations.
    retained_orphans: Vec<UnfinishedToolOutput>,
    /// Tool calls surfaced as an elicitation (AskUserQuestion); their cards are suppressed.
    elicitation_tool_ids: HashSet<String>,
    /// Group of the still-open run of consecutive message chunks.
    open_message_group: Option<String>,
    /// ACP message id of the last chunk that carried one.
    last_message_id: Option<String>,
    /// The thought counterparts of `open_message_group` and `last_message_id`.
    open_thought_group: Option<String>,
    last_thought_id: Option<String>,
    /// A blank chunk changed the thought id, so the next row starts a thought.
    thought_start_pending: bool,
    group_counter: u64,
    /// Frames at or below this seq are dropped, so replay overlap is harmless.
    last_seq: u64,
    turn_active: bool,
    /// Whether the running turn produced visible output, for the empty-output notice.
    turn_has_output: bool,
    /// Whether a mid-turn prompt steers the running turn rather than opening a new one.
    steering: bool,
    active_operation: Option<crate::acp::establishment::NativeOperationId>,
}

impl TranscriptModel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn rows(&self) -> &[TranscriptRow] {
        &self.rows
    }

    pub fn into_rows(self) -> Vec<TranscriptRow> {
        self.rows
    }

    pub(crate) fn into_retained(self) -> RetainedTranscript {
        let mut unfinished_outputs: Vec<_> = self
            .tools
            .into_iter()
            .filter_map(|(tool_call_id, lifecycle)| {
                let (ToolLifecycle::Buffered(output)
                | ToolLifecycle::Open(output)
                | ToolLifecycle::Terminal(output)) = lifecycle;
                if self.elicitation_tool_ids.contains(&tool_call_id) {
                    return None;
                }
                output.into_unfinished(&tool_call_id)
            })
            .chain(self.retained_orphans)
            .collect();
        unfinished_outputs.sort_unstable_by_key(|output| output.first_seq);
        RetainedTranscript {
            rows: self.rows,
            row_seq: self.row_seq,
            tool_start_seq: self.tool_start_seq,
            unfinished_outputs,
        }
    }

    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// Observe prefix lifecycle and context without rows, advancing the replay watermark.
    pub(crate) fn observe_prior(
        &mut self,
        seq: u64,
        event: &Event,
        metadata: &crate::acp::event_sender::EventMetadata,
    ) {
        if seq <= self.last_seq {
            return;
        }
        self.last_seq = seq;
        match event {
            Event::ToolCallStarted { tool_call }
                if !self.elicitation_tool_ids.contains(&tool_call.id) =>
            {
                self.track_tool_start(&tool_call.id, true);
            }
            Event::ToolCallUpdated { tool_call_id, .. }
                if !self.elicitation_tool_ids.contains(tool_call_id) =>
            {
                self.track_tool_start(tool_call_id, false);
            }
            Event::ToolCallContent {
                tool_call_id,
                content,
            } => {
                self.buffer_tool_output(tool_call_id, content);
            }
            Event::ElicitationRequested { elicitation } => {
                if let Some(id) = &elicitation.tool_call_id {
                    self.elicitation_tool_ids.insert(id.clone());
                    self.retained_orphans
                        .retain(|output| output.tool_call_id != *id);
                    self.finish_tool(id);
                }
            }
            Event::ToolCallCompleted { tool_call_id, .. } => {
                self.finish_tool(tool_call_id);
                self.elicitation_tool_ids.remove(tool_call_id);
            }
            Event::AgentMessageChunk {
                message_id: Some(id),
                ..
            } => self.last_message_id = Some(id.clone()),
            Event::AgentThoughtChunk {
                message_id: Some(id),
                ..
            } => self.last_thought_id = Some(id.clone()),
            Event::UserPromptSent { .. } | Event::UserDiffCommentsPrompt { .. } => {
                self.last_message_id = None;
                self.last_thought_id = None;
                self.thought_start_pending = false;
            }
            Event::Stopped { .. }
            | Event::AgentStartupError { .. }
            | Event::IncompatibleAgent { .. }
            | Event::AgentSwitched { .. }
                if closes_tool_lifecycle(event, metadata) =>
            {
                for (id, tool) in &mut self.tools {
                    if matches!(tool, ToolLifecycle::Buffered(_)) {
                        if let Some(output) = tool.finish().into_unfinished(id) {
                            self.retained_orphans.push(output);
                        }
                    } else if matches!(tool, ToolLifecycle::Open(_)) {
                        drop(tool.finish());
                    }
                }
            }
            _ => {}
        }
    }

    /// Apply one event at `seq`, returning the row changes it produced.
    pub(crate) fn apply_scoped_event(
        &mut self,
        seq: u64,
        event: &Event,
        metadata: &crate::acp::event_sender::EventMetadata,
    ) -> Vec<TranscriptDelta> {
        if seq <= self.last_seq {
            return Vec::new();
        }
        self.last_seq = seq;
        if !matches!(event, Event::AgentMessageChunk { .. }) {
            self.open_message_group = None;
        }
        if !matches!(event, Event::AgentThoughtChunk { .. }) {
            self.open_thought_group = None;
        }
        use crate::acp::event_sender::EventScope;
        let current = matches!(
            metadata.scope,
            EventScope::SessionCurrent | EventScope::OperationCurrent
        );
        let original_result = metadata
            .operation
            .filter(|operation| operation.outcome.is_some());
        if matches!(
            event,
            Event::Stopped { .. } | Event::PromptRuntimeError { .. }
        ) && metadata.operation.is_some_and(|operation| {
            self.active_operation == Some(operation.correlation.operation_id)
        }) {
            self.active_operation = None;
        }

        match event {
            Event::AgentMessageChunk { text, message_id } => {
                // Chunks without an id keep joining whatever message is open.
                let message_start = message_id.is_some()
                    && self.last_message_id.is_some()
                    && self.last_message_id != *message_id;
                if message_id.is_some() {
                    self.last_message_id = message_id.clone();
                }
                if message_start {
                    self.open_message_group = None;
                }
                let group_id = self.message_group();
                let mut row = TranscriptRow::new(
                    format!("msg-{seq}"),
                    group_id,
                    TranscriptRowKind::Message,
                    text.clone(),
                );
                row.message_start = message_start;
                vec![self.append(row)]
            }
            Event::AgentThoughtChunk { text, message_id } => {
                self.turn_has_output = true;
                if message_id.is_some()
                    && self.last_thought_id.is_some()
                    && self.last_thought_id != *message_id
                {
                    self.open_thought_group = None;
                    self.thought_start_pending = true;
                }
                if message_id.is_some() {
                    self.last_thought_id = message_id.clone();
                }
                if text.trim().is_empty() && self.open_thought_group.is_none() {
                    return Vec::new();
                }
                let group_id = match &self.open_thought_group {
                    Some(group) => group.clone(),
                    None => {
                        let group = self.fresh_group();
                        self.open_thought_group = Some(group.clone());
                        group
                    }
                };
                let mut row = TranscriptRow::new(
                    format!("thought-{seq}"),
                    group_id,
                    TranscriptRowKind::Thinking,
                    text.clone(),
                );
                row.message_start = std::mem::take(&mut self.thought_start_pending);
                vec![self.append(row)]
            }
            Event::UserPromptSent {
                text,
                attachments,
                prompt_id,
                merged_prompt_ids,
                synthesized,
                origin_plugin,
                ..
            } => {
                self.last_message_id = None;
                self.last_thought_id = None;
                self.thought_start_pending = false;
                // A rate-limit resume continuation replays a prompt the user
                // already saw once, before the park; render nothing so the
                // transcript reads as an uninterrupted continuation instead
                // of the same message appearing twice.
                if *synthesized {
                    Vec::new()
                } else {
                    let id = match prompt_id {
                        Some(pid) if !pid.is_empty() => pid.clone(),
                        _ => format!("user-seq-{seq}"),
                    };
                    let mut row = self.grouped_row(id, TranscriptRowKind::UserPrompt, text.clone());
                    row.attachments = attachments.clone();
                    row.origin_plugin = origin_plugin.clone();
                    row.merged_prompt_ids = merged_prompt_ids.clone();
                    // A queue drain that failed after publishing retries with the same ids.
                    match self.rows.iter_mut().find(|r| r.id == row.id) {
                        Some(existing) => {
                            row.group_id = existing.group_id.clone();
                            *existing = row;
                            vec![patch(existing)]
                        }
                        None => vec![self.append(row)],
                    }
                }
            }
            Event::UserDiffCommentsPrompt {
                intro,
                outro,
                is_multi_repo,
                comments,
                assembled_markdown,
            } => {
                self.last_message_id = None;
                self.last_thought_id = None;
                self.thought_start_pending = false;
                let mut row = self.grouped_row(
                    format!("user-seq-{seq}"),
                    TranscriptRowKind::UserDiffComments,
                    assembled_markdown.clone(),
                );
                row.diff_comments = Some(DiffCommentsPayload {
                    intro: intro.clone(),
                    outro: outro.clone(),
                    is_multi_repo: *is_multi_repo,
                    comments: comments.clone(),
                });
                vec![self.append(row)]
            }
            Event::ToolCallStarted { tool_call } => self.on_tool_started(tool_call),
            Event::ToolCallCompleted {
                tool_call_id,
                is_error,
                content,
                output,
                completed_at,
                async_subagent,
            } => {
                if self.elicitation_tool_ids.remove(tool_call_id) {
                    self.finish_tool(tool_call_id);
                    return Vec::new();
                }
                let mut deltas = if matches!(
                    self.tools.get(tool_call_id),
                    Some(ToolLifecycle::Open(_) | ToolLifecycle::Terminal(_))
                ) {
                    Vec::new()
                } else {
                    self.ensure_tool_start(tool_call_id, None, None, None, *completed_at, None)
                };
                self.turn_has_output = true;
                let mut row = self.completion_row(seq, tool_call_id, *is_error, content);
                row.at = *completed_at;
                row.output = output.clone();
                row.async_subagent = *async_subagent;
                deltas.push(self.append(row));
                deltas
            }
            Event::ToolCallContent {
                tool_call_id,
                content,
            } => {
                // Each content frame replaces the previous snapshot without inventing an orphan start.
                self.buffer_tool_output(tool_call_id, content);
                if !matches!(self.tools.get(tool_call_id), Some(ToolLifecycle::Open(_))) {
                    return Vec::new();
                }
                if let Some(index) = self.find_tool_start(tool_call_id) {
                    let row = &mut self.rows[index];
                    row.output = vec![ToolOutputBlock::Text {
                        text: content.clone(),
                    }];
                    vec![patch(row)]
                } else {
                    Vec::new()
                }
            }
            Event::ToolCallUpdated {
                tool_call_id,
                title,
                args_preview,
                prompt,
                started_at,
                diffs,
            } => self.on_tool_updated(
                tool_call_id,
                title.as_deref(),
                args_preview.as_deref(),
                prompt.as_deref(),
                *started_at,
                diffs.as_deref(),
            ),
            Event::ElicitationRequested { elicitation } => {
                let Some(tool_call_id) = elicitation.tool_call_id.as_ref() else {
                    return Vec::new();
                };
                self.elicitation_tool_ids.insert(tool_call_id.clone());
                self.retained_orphans
                    .retain(|output| output.tool_call_id != *tool_call_id);
                self.finish_tool(tool_call_id);
                self.remove_rows_for_tool(tool_call_id)
            }
            Event::ElicitationResolved { nonce, answers, .. } => {
                let id = format!("elicitation-{}", nonce.0);
                if answers.is_empty() || self.row_ids.contains(&id) {
                    return Vec::new();
                }
                let text = answers
                    .iter()
                    .map(|a| format!("{}: {}", a.question, a.answer))
                    .collect::<Vec<_>>()
                    .join("\n");
                let mut row = self.grouped_row(id, TranscriptRowKind::ElicitationAnswered, text);
                row.elicitation_answers = answers.clone();
                vec![self.append(row)]
            }
            Event::Stopped { .. } => {
                let mut deltas = if closes_tool_lifecycle(event, metadata) {
                    self.turn_active = false;
                    self.sweep_open_tools(seq)
                } else {
                    Vec::new()
                };
                let uncertain = matches!(
                    (metadata.prompt_observation, metadata.output_uncertainty),
                    (Some(crate::acp::protocol::AcpPromptObservation::Written { window_id }), Some(uncertain))
                        if window_id == uncertain
                );
                if original_result.is_some() && uncertain {
                    deltas.push(self.notice(
                        seq,
                        "Output attribution is uncertain for this prompt.".into(),
                    ));
                }
                deltas
            }
            Event::AgentStartupError { message, .. } => {
                let mut deltas = self.sweep_open_tools(seq);
                self.turn_active = false;
                deltas.push(self.notice(seq, format!("agent startup failed: {message}")));
                deltas
            }
            Event::AcpConnectionUnavailable { source, cause, .. } => {
                if current {
                    self.active_operation = None;
                }
                vec![self.notice(
                    seq,
                    format!("ACP connection unavailable ({source:?}, {cause:?})"),
                )]
            }
            Event::IncompatibleAgent { .. } => {
                self.turn_active = false;
                self.sweep_open_tools(seq)
            }
            Event::AgentSwitched { from, to, reason } => {
                if current {
                    self.active_operation = None;
                }
                let mut deltas = self.sweep_open_tools(seq);
                // Clients render the handoff with the session_cleared divider.
                deltas.push(self.push(
                    format!("agent-switched-{seq}"),
                    TranscriptRowKind::SessionCleared,
                    format!("Switched structured view agent from {from} to {to} ({reason})."),
                ));
                deltas
            }
            Event::SessionCleared => {
                let mut row = self.grouped_row(
                    format!("cleared-{seq}"),
                    TranscriptRowKind::SessionCleared,
                    "Conversation cleared, the model no longer remembers earlier turns."
                        .to_string(),
                );
                row.clear_after_seq = metadata.clear_after_seq;
                vec![self.append(row)]
            }
            Event::ConversationCompacted => {
                vec![self.push(
                    format!("compacted-{seq}"),
                    TranscriptRowKind::Compacted,
                    "Conversation compacted; earlier turns above are summarised in the model's \
                 context."
                        .to_string(),
                )]
            }
            Event::ConversationCompactionSummary {
                compaction_id,
                text,
            } => {
                // Kept even when empty (clients hide it), so a later summary
                // lands beside its divider.
                let id = format!("compaction-summary-{compaction_id}");
                match self.rows.iter().position(|r| r.id == id) {
                    Some(i) => {
                        self.rows[i].text = text.clone();
                        vec![patch(&self.rows[i])]
                    }
                    None => vec![self.push(id, TranscriptRowKind::CompactionSummary, text.clone())],
                }
            }
            Event::SessionContextReset { reason, .. } => {
                let has_prior_prompt = self.rows.iter().any(|r| {
                    matches!(
                        r.kind,
                        TranscriptRowKind::UserPrompt | TranscriptRowKind::UserDiffComments
                    )
                });
                if !has_prior_prompt
                    && (reason.is_empty() || reason.starts_with(LOAD_FAILED_PREFIX))
                {
                    return Vec::new();
                }
                let text = if reason.is_empty() {
                    "Conversation context reset; agent transcript was unavailable.".to_string()
                } else {
                    reason.clone()
                };
                vec![self.push(
                    format!("reset-{seq}"),
                    TranscriptRowKind::ContextReset,
                    text,
                )]
            }
            Event::PromptRuntimeError { message, .. } => {
                vec![self.notice(seq, format!("prompt failed: {message}"))]
            }
            Event::ModeSwitchFailed { mode_id, reason } => vec![self.notice(
                seq,
                format!("mode switch to \"{mode_id}\" failed: {reason}"),
            )],
            Event::SessionNotice {
                severity,
                title,
                description,
                key,
            } => {
                let id = notice_id(key.as_deref(), seq);
                let text = session_notice_text(severity, title, description);
                match self.rows.iter().position(|r| r.id == id) {
                    Some(i) => {
                        self.rows[i].text = text;
                        vec![patch(&self.rows[i])]
                    }
                    None => vec![self.push(id, TranscriptRowKind::Advisory, text)],
                }
            }
            Event::RateLimitAutoResumed { resets_at, manual } => {
                let how = if *manual { "resumed" } else { "auto-resumed" };
                vec![self.notice(seq, format!("{how} at {resets_at} after rate-limit park"))]
            }
            Event::ConversationSummary { text, .. } => vec![self.push(
                format!("summary-{seq}"),
                TranscriptRowKind::Summary,
                text.clone(),
            )],
            Event::ThinkingStarted => {
                if metadata.native_autonomous.is_some() {
                    return Vec::new();
                }
                self.turn_active = true;
                self.turn_has_output = true;
                if current {
                    self.active_operation = metadata
                        .operation
                        .map(|operation| operation.correlation.operation_id);
                }
                Vec::new()
            }
            Event::PromptCapabilities { steering, .. } => {
                self.steering = *steering;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn track_tool_start(&mut self, id: &str, reopen_terminal: bool) {
        let moved = match self.tools.get_full_mut(id) {
            Some((index, _, tool @ ToolLifecycle::Buffered(_))) => {
                let output = tool.finish();
                *tool = ToolLifecycle::Open(output);
                Some(index)
            }
            Some((index, _, tool @ ToolLifecycle::Terminal(_))) if reopen_terminal => {
                *tool = ToolLifecycle::Open(BufferedToolOutput::default());
                Some(index)
            }
            Some(_) => None,
            None => {
                self.tools.insert(
                    id.to_owned(),
                    ToolLifecycle::Open(BufferedToolOutput::default()),
                );
                None
            }
        };
        if let Some(index) = moved {
            self.tools.move_index(index, self.tools.len() - 1);
        }
    }

    fn buffer_tool_output(&mut self, id: &str, content: &str) {
        if self.elicitation_tool_ids.contains(id) {
            return;
        }
        match self.tools.get_mut(id) {
            Some(
                ToolLifecycle::Buffered(output)
                | ToolLifecycle::Open(output)
                | ToolLifecycle::Terminal(output),
            ) => {
                output.first_seq.get_or_insert(self.last_seq);
                output.text.clear();
                output.text.push_str(content);
            }
            None => {
                self.tools.insert(
                    id.to_owned(),
                    ToolLifecycle::Buffered(BufferedToolOutput {
                        first_seq: Some(self.last_seq),
                        text: content.to_owned(),
                    }),
                );
            }
        }
    }

    fn finish_tool(&mut self, id: &str) -> String {
        match self.tools.get_mut(id) {
            Some(tool) => tool.finish().text,
            None => {
                self.tools.insert(
                    id.to_owned(),
                    ToolLifecycle::Terminal(BufferedToolOutput::default()),
                );
                String::new()
            }
        }
    }

    fn on_tool_started(&mut self, tool_call: &ToolCall) -> Vec<TranscriptDelta> {
        if self.elicitation_tool_ids.contains(&tool_call.id) {
            return Vec::new();
        }
        self.track_tool_start(&tool_call.id, true);
        self.tool_start_seq
            .insert(tool_call.id.clone(), self.last_seq);
        self.turn_has_output = true;
        let Some(idx) = self.find_tool_start(&tool_call.id) else {
            return vec![self.append(tool_start_row(tool_call.clone()))];
        };
        let row = &mut self.rows[idx];
        let merged = merge_tool_start(row.tool.as_ref().unwrap_or(tool_call), tool_call);
        row.text = merged.name.clone();
        row.at = merged.started_at;
        row.tool = Some(merged);
        vec![patch(row)]
    }

    fn on_tool_updated(
        &mut self,
        tool_call_id: &str,
        title: Option<&str>,
        args_preview: Option<&str>,
        prompt: Option<&str>,
        started_at: Option<Timestamp>,
        diffs: Option<&[DiffPreview]>,
    ) -> Vec<TranscriptDelta> {
        if self.elicitation_tool_ids.contains(tool_call_id)
            || (matches!(
                self.tools.get(tool_call_id),
                Some(ToolLifecycle::Terminal(_))
            ) && self.find_tool_start(tool_call_id).is_none())
        {
            return Vec::new();
        }
        // A non-empty diff list replaces wholesale; none or empty keeps earlier diffs.
        let diffs = diffs.filter(|d| !d.is_empty());
        let synthesized = self.ensure_tool_start(
            tool_call_id,
            title,
            args_preview,
            prompt,
            started_at.unwrap_or_else(Timestamp::now),
            diffs,
        );
        if !synthesized.is_empty() {
            return synthesized;
        }
        let idx = self
            .find_tool_start(tool_call_id)
            .expect("tool_start present");
        let row = &mut self.rows[idx];
        if let Some(title) = title {
            row.text = title.to_string();
        }
        if let Some(tool) = row.tool.as_mut() {
            if let Some(title) = title {
                tool.name = title.to_string();
            }
            if let Some(args_preview) = args_preview {
                if prompt.is_some() || !args_preview.ends_with(ARGS_CUT_MARKER) {
                    tool.prompt = prompt.map(str::to_string);
                }
                tool.args_preview = args_preview.to_string();
            }
            if let Some(started_at) = started_at {
                tool.started_at = started_at;
            }
            if let Some(diffs) = diffs {
                tool.diffs = diffs.to_vec();
            }
        }
        vec![patch(row)]
    }

    /// Synthesize a `tool_start` for a call first seen by a later frame, so a card renders.
    fn ensure_tool_start(
        &mut self,
        tool_call_id: &str,
        name: Option<&str>,
        args_preview: Option<&str>,
        prompt: Option<&str>,
        started_at: Timestamp,
        diffs: Option<&[DiffPreview]>,
    ) -> Vec<TranscriptDelta> {
        if self.find_tool_start(tool_call_id).is_some() {
            return Vec::new();
        }
        self.track_tool_start(tool_call_id, false);
        self.turn_has_output = true;
        let tool = ToolCall {
            prompt: prompt.map(str::to_string),
            id: tool_call_id.to_string(),
            name: name
                .filter(|n| !n.is_empty())
                .unwrap_or("tool call")
                .to_string(),
            kind: "other".to_string(),
            args_preview: args_preview.unwrap_or_default().to_string(),
            started_at,
            parent_tool_call_id: None,
            memory_recall: None,
            diffs: diffs.map(<[_]>::to_vec).unwrap_or_default(),
        };
        vec![self.append(tool_start_row(tool))]
    }

    fn completion_row(
        &mut self,
        seq: u64,
        tool_call_id: &str,
        is_error: bool,
        content: &str,
    ) -> TranscriptRow {
        // Completion content wins, then streamed output, then a status word.
        let buffered = self.finish_tool(tool_call_id);
        let text = if !content.is_empty() {
            content.to_string()
        } else if !buffered.is_empty() {
            buffered.to_owned()
        } else if is_error {
            "tool failed".to_string()
        } else {
            "completed".to_string()
        };
        // Adapters can reuse a tool call id after reconnecting; keep row ids unique.
        let base_id = format!("done-{tool_call_id}");
        let row_id = if self.row_ids.contains(&base_id) {
            format!("{base_id}-{seq}")
        } else {
            base_id
        };
        let kind = if is_error {
            TranscriptRowKind::ToolError
        } else {
            TranscriptRowKind::ToolComplete
        };
        let mut row = TranscriptRow::new(row_id, format!("tool-{tool_call_id}"), kind, text);
        row.tool_call_id = Some(tool_call_id.to_string());
        row
    }

    fn remove_rows_for_tool(&mut self, tool_call_id: &str) -> Vec<TranscriptDelta> {
        let mut removed = Vec::new();
        self.rows.retain(|r| {
            let keep = r.tool_call_id.as_deref() != Some(tool_call_id);
            if !keep {
                removed.push(TranscriptDelta::Remove(r.id.clone()));
            }
            keep
        });
        if !removed.is_empty() {
            self.row_ids = self.rows.iter().map(|r| r.id.clone()).collect();
        }
        removed
    }

    fn append(&mut self, mut row: TranscriptRow) -> TranscriptDelta {
        row.seq = self.last_seq;
        self.row_ids.insert(row.id.clone());
        self.row_seq.insert(row.id.clone(), self.last_seq);
        self.rows.push(row.clone());
        TranscriptDelta::Append(row)
    }

    fn grouped_row(&mut self, id: String, kind: TranscriptRowKind, text: String) -> TranscriptRow {
        let group_id = self.fresh_group();
        TranscriptRow::new(id, group_id, kind, text)
    }

    /// Append a row in its own fresh group.
    fn push(&mut self, id: String, kind: TranscriptRowKind, text: String) -> TranscriptDelta {
        let row = self.grouped_row(id, kind, text);
        self.append(row)
    }

    fn notice(&mut self, seq: u64, text: String) -> TranscriptDelta {
        self.push(format!("notice-{seq}"), TranscriptRowKind::Notice, text)
    }

    fn message_group(&mut self) -> String {
        if let Some(group) = &self.open_message_group {
            return group.clone();
        }
        let group = self.fresh_group();
        self.open_message_group = Some(group.clone());
        group
    }

    fn fresh_group(&mut self) -> String {
        self.group_counter += 1;
        format!("g{}", self.group_counter)
    }

    /// Close every `tool_start` without a terminal row with a `tool_stopped`
    /// carrying any buffered output.
    fn sweep_open_tools(&mut self, seq: u64) -> Vec<TranscriptDelta> {
        let now = Timestamp::now();
        let mut deltas = Vec::new();
        for index in 0..self.tools.len() {
            let (id, tool) = self.tools.get_index_mut(index).expect("tool index");
            if !matches!(tool, ToolLifecycle::Open(_)) {
                if matches!(tool, ToolLifecycle::Buffered(_)) {
                    if let Some(output) = tool.finish().into_unfinished(id) {
                        self.retained_orphans.push(output);
                    }
                }
                continue;
            }
            let buffered = tool.finish().text;
            let id = id.clone();
            let mut row = TranscriptRow::new(
                format!("stopped-{id}-{seq}"),
                format!("tool-{id}"),
                TranscriptRowKind::ToolStopped,
                buffered,
            );
            row.at = now;
            row.tool_call_id = Some(id);
            deltas.push(self.append(row));
        }
        deltas
    }

    fn find_tool_start(&self, tool_call_id: &str) -> Option<usize> {
        self.rows.iter().position(|r| {
            r.kind == TranscriptRowKind::ToolStart
                && r.tool_call_id.as_deref() == Some(tool_call_id)
        })
    }
}

/// Place each certified clear before its retained prefix without copying rows.
///
/// Unknown legacy positions and earlier clear markers remain barriers. Repeated
/// projection is idempotent, including histories containing multiple clears.
pub fn project_transcript_clear_boundaries(rows: &mut [TranscriptRow]) {
    for index in 0..rows.len() {
        if rows[index].kind != TranscriptRowKind::SessionCleared {
            continue;
        }
        let Some(cut) = rows[index].clear_after_seq else {
            continue;
        };
        let mut start = index;
        while start > 0
            && rows[start - 1].kind != TranscriptRowKind::SessionCleared
            && rows[start - 1].seq > cut
        {
            start -= 1;
        }
        rows[start..=index].rotate_right(1);
    }
}

impl TranscriptRow {
    fn new(id: String, group_id: String, kind: TranscriptRowKind, text: String) -> Self {
        Self {
            id,
            seq: 0,
            clear_after_seq: None,
            group_id,
            kind,
            at: Timestamp::now(),
            text,
            tool_call_id: None,
            tool: None,
            output: Vec::new(),
            attachments: Vec::new(),
            origin_plugin: None,
            merged_prompt_ids: Vec::new(),
            diff_comments: None,
            elicitation_answers: Vec::new(),
            async_subagent: false,
            message_start: false,
        }
    }
}

fn tool_start_row(tool: ToolCall) -> TranscriptRow {
    let mut row = TranscriptRow::new(
        format!("start-{}", tool.id),
        format!("tool-{}", tool.id),
        TranscriptRowKind::ToolStart,
        tool.name.clone(),
    );
    row.at = tool.started_at;
    row.tool_call_id = Some(tool.id.clone());
    row.tool = Some(tool);
    row
}

fn patch(row: &TranscriptRow) -> TranscriptDelta {
    TranscriptDelta::Patch {
        id: row.id.clone(),
        row: row.clone(),
    }
}

/// Merge a repeated `ToolCallStarted` without letting a sparser frame clobber richer data.
pub(crate) fn merge_tool_start(prev: &ToolCall, incoming: &ToolCall) -> ToolCall {
    let pick = |use_incoming: bool, incoming: &String, prev: &String| {
        (if use_incoming { incoming } else { prev }).clone()
    };
    ToolCall {
        id: incoming.id.clone(),
        name: pick(!incoming.name.is_empty(), &incoming.name, &prev.name),
        kind: pick(
            !incoming.kind.is_empty() && incoming.kind != "other",
            &incoming.kind,
            &prev.kind,
        ),
        args_preview: pick(
            !incoming.args_preview.trim().is_empty(),
            &incoming.args_preview,
            &prev.args_preview,
        ),
        // A repeat that carries no `prompt` of its own (sparse, or bookkeeping-only
        // args) keeps the earlier one; a repeat with its own prompt replaces it.
        prompt: incoming.prompt.clone().or_else(|| {
            let restates = incoming.args_preview.contains(r#""prompt""#);
            prev.prompt.clone().filter(|_| !restates)
        }),
        started_at: incoming.started_at.max(prev.started_at),
        parent_tool_call_id: incoming
            .parent_tool_call_id
            .clone()
            .or_else(|| prev.parent_tool_call_id.clone()),
        memory_recall: incoming
            .memory_recall
            .clone()
            .or_else(|| prev.memory_recall.clone()),
        diffs: (if incoming.diffs.is_empty() {
            &prev.diffs
        } else {
            &incoming.diffs
        })
        .clone(),
    }
}

/// Reconcile one server-folded row into a client's rows by id, merging a
/// repeated tool start (the twin of the web's `mergeServerRows` step).
pub(crate) fn upsert_transcript_row(rows: &mut Vec<TranscriptRow>, incoming: TranscriptRow) {
    let Some(idx) = rows.iter().position(|r| r.id == incoming.id) else {
        rows.push(incoming);
        return;
    };
    let row = &mut rows[idx];
    if row.kind == TranscriptRowKind::ToolStart && incoming.kind == TranscriptRowKind::ToolStart {
        if let (Some(prev_tool), Some(inc_tool)) = (row.tool.as_ref(), incoming.tool.as_ref()) {
            let merged = merge_tool_start(prev_tool, inc_tool);
            row.text = merged.name.clone();
            row.at = merged.started_at;
            row.tool = Some(merged);
            return;
        }
    }
    *row = incoming;
}

/// Replace the row with `row.id` by the server's full row, appending when absent.
pub(crate) fn patch_transcript_row(rows: &mut Vec<TranscriptRow>, row: TranscriptRow) {
    match rows.iter().position(|r| r.id == row.id) {
        Some(idx) => rows[idx] = row,
        None => rows.push(row),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::approvals::Nonce;
    use crate::acp::elicitations::{Elicitation, ElicitationOutcome};

    #[test]
    fn certified_clear_projects_retained_prefix_and_keeps_creation_sequences() {
        let mut model = fold([prompt("old"), started(tool("call", "work"))]);
        model.apply_scoped_event(
            3,
            &content("call", "patched"),
            &crate::acp::event_sender::EventMetadata::history(),
        );
        assert_eq!(row(&model, "start-call").seq, 2);
        let metadata = crate::acp::event_sender::EventMetadata {
            clear_after_seq: Some(1),
            ..crate::acp::event_sender::EventMetadata::history()
        };
        model.apply_scoped_event(4, &Event::SessionCleared, &metadata);
        let mut rows = model.rows().to_vec();
        project_transcript_clear_boundaries(&mut rows);
        assert_eq!(
            rows.iter().map(|row| row.seq).collect::<Vec<_>>(),
            [1, 4, 2]
        );
        assert_eq!(rows[1].clear_after_seq, Some(1));
        let once = rows.clone();
        project_transcript_clear_boundaries(&mut rows);
        assert_eq!(rows, once);
    }

    #[test]
    fn certified_clear_handles_empty_prefix_cold_ranges_and_multiple_clears() {
        let mut model = fold([prompt("old")]);
        for (seq, cut) in [(2, 1), (3, 2)] {
            model.apply_scoped_event(
                seq,
                &Event::SessionCleared,
                &crate::acp::event_sender::EventMetadata {
                    clear_after_seq: Some(cut),
                    ..crate::acp::event_sender::EventMetadata::history()
                },
            );
        }
        model.apply_scoped_event(
            4,
            &prompt("new"),
            &crate::acp::event_sender::EventMetadata::history(),
        );
        model.apply_scoped_event(
            5,
            &Event::SessionCleared,
            &crate::acp::event_sender::EventMetadata {
                clear_after_seq: Some(3),
                ..crate::acp::event_sender::EventMetadata::history()
            },
        );
        let mut rows = model.rows().to_vec();
        project_transcript_clear_boundaries(&mut rows);
        assert_eq!(
            rows.iter().map(|row| row.seq).collect::<Vec<_>>(),
            [1, 2, 3, 5, 4]
        );
        let mut cold = model.rows()[3..].to_vec();
        project_transcript_clear_boundaries(&mut cold);
        assert_eq!(cold.iter().map(|row| row.seq).collect::<Vec<_>>(), [5, 4]);
        cold[0].clear_after_seq = None;
        cold.reverse();
        let legacy = cold.clone();
        project_transcript_clear_boundaries(&mut cold);
        assert_eq!(cold, legacy);
    }
    use crate::acp::state::test_support::{chunk, prompt, stopped};
    use crate::acp::state::MemoryRecall;
    use crate::daemon::PromptAttachmentKind;

    fn at(secs: i64) -> Timestamp {
        Timestamp::from_unix_secs(secs).expect("valid ts")
    }

    fn tool(id: &str, name: &str) -> ToolCall {
        ToolCall {
            prompt: None,
            id: id.into(),
            name: name.into(),
            kind: "execute".into(),
            args_preview: "{}".into(),
            started_at: at(100),
            parent_tool_call_id: None,
            memory_recall: None,
            diffs: Vec::new(),
        }
    }

    #[test]
    fn a_repeated_start_keeps_the_full_prompt_unless_it_restates_a_prompt() {
        let mut prev = tool("t", "Task");
        prev.args_preview = r#"{"prompt":"cut"#.into();
        prev.prompt = Some("full prompt".into());
        let mut sparse = tool("t", "Task");
        sparse.args_preview = String::new();
        assert_eq!(
            merge_tool_start(&prev, &sparse).prompt.as_deref(),
            Some("full prompt")
        );
        let mut changed = tool("t", "Task");
        changed.args_preview = r#"{"prompt":"other"}"#.into();
        assert_eq!(merge_tool_start(&prev, &changed).prompt, None);
        let mut bookkeeping = tool("t", "Task");
        bookkeeping.args_preview = r#"{"_aoe_title":"external_directory"}"#.into();
        assert_eq!(
            merge_tool_start(&prev, &bookkeeping).prompt.as_deref(),
            Some("full prompt")
        );
    }

    #[test]
    fn an_update_keeps_the_stored_prompt_only_while_the_preview_stays_cut() {
        let mut start = tool("t", "Task");
        start.args_preview = "{\"prompt\":\"cut\u{2026}[truncated]".into();
        start.prompt = Some("full prompt".into());
        let m = fold([
            started(start.clone()),
            updated("t", None, Some(&start.args_preview), None),
        ]);
        assert_eq!(
            tool_of(&m, "start-t").prompt.as_deref(),
            Some("full prompt"),
            "a deduplicated repeat of the cut call"
        );

        let m = fold([
            started(start),
            updated("t", None, Some(r#"{"prompt":"B"}"#), None),
        ]);
        assert_eq!(
            tool_of(&m, "start-t").prompt,
            None,
            "an explicit short replacement drops the old prompt"
        );
    }

    fn started(tool_call: ToolCall) -> Event {
        Event::ToolCallStarted { tool_call }
    }

    fn completed(id: &str, is_error: bool, content: &str) -> Event {
        Event::ToolCallCompleted {
            tool_call_id: id.into(),
            is_error,
            content: content.into(),
            output: Vec::new(),
            completed_at: at(200),
            async_subagent: false,
        }
    }

    fn updated(
        id: &str,
        title: Option<&str>,
        args: Option<&str>,
        diffs: Option<Vec<DiffPreview>>,
    ) -> Event {
        Event::ToolCallUpdated {
            prompt: None,
            tool_call_id: id.into(),
            title: title.map(Into::into),
            args_preview: args.map(Into::into),
            started_at: None,
            diffs,
        }
    }

    fn content(id: &str, text: &str) -> Event {
        Event::ToolCallContent {
            tool_call_id: id.into(),
            content: text.into(),
        }
    }

    fn elicitation_requested(tool_call_id: &str) -> Event {
        Event::ElicitationRequested {
            elicitation: Elicitation {
                nonce: Nonce("e-1".into()),
                message: "Pick one".into(),
                title: None,
                description: None,
                tool_call_id: Some(tool_call_id.into()),
                questions: Vec::new(),
                requested_at: at(50),
                resolved: None,
            },
        }
    }

    fn fold(events: impl IntoIterator<Item = Event>) -> TranscriptModel {
        let mut m = TranscriptModel::new();
        for (i, e) in events.into_iter().enumerate() {
            m.apply_scoped_event(
                i as u64 + 1,
                &e,
                &crate::acp::event_sender::EventMetadata::history(),
            );
        }
        m
    }

    #[test]
    fn retained_orphans_survive_turn_endings_without_feeding_later_calls() {
        for closer in [
            stopped("cancelled"),
            Event::startup_error("boom"),
            Event::IncompatibleAgent {
                detail: crate::acp::state::StartupErrorDetail::UnsupportedProtocolVersion {
                    expected: "1".into(),
                    received: "2".into(),
                },
            },
            Event::AgentSwitched {
                from: "claude".into(),
                to: "codex".into(),
                reason: "rate_limit".into(),
            },
        ] {
            let events = [content("t", "orphan text"), closer];
            for split in 0..=events.len() {
                let mut model = TranscriptModel::new();
                for (index, event) in events.iter().enumerate() {
                    if index < split {
                        model.observe_prior(
                            index as u64 + 1,
                            event,
                            &crate::acp::event_sender::EventMetadata::history(),
                        );
                    } else {
                        model.apply_scoped_event(
                            index as u64 + 1,
                            event,
                            &crate::acp::event_sender::EventMetadata::history(),
                        );
                    }
                }
                assert_eq!(count(&model, TranscriptRowKind::ToolStopped), 0);
                let retained = model.clone().into_retained();
                assert_eq!(
                    retained
                        .unfinished_outputs
                        .iter()
                        .map(|output| (output.tool_call_id.as_str(), output.text.as_str()))
                        .collect::<Vec<_>>(),
                    [("t", "orphan text")],
                    "split at {split}: {events:?}"
                );
                model.apply_scoped_event(
                    3,
                    &completed("t", false, ""),
                    &crate::acp::event_sender::EventMetadata::history(),
                );
                assert_eq!(row(&model, "done-t").text, "completed");
                let retained = model.into_retained();
                assert_eq!(retained.unfinished_outputs[0].text, "orphan text");
            }
        }
        let snapshot = fold([
            content("q", "hidden"),
            stopped("cancelled"),
            elicitation_requested("q"),
            completed("q", false, ""),
        ])
        .into_retained();
        assert!(snapshot.unfinished_outputs.is_empty());
    }

    #[test]
    fn retained_output_survives_cursor_boundaries_without_changing_lifecycle() {
        let cases = [
            (
                vec![
                    started(tool("a", "A")),
                    started(tool("b", "B")),
                    content("b", "old"),
                    content("a", "second"),
                    content("b", "latest"),
                ],
                vec![("b", "latest"), ("a", "second")],
            ),
            (
                vec![
                    started(tool("a", "A")),
                    completed("a", false, "done"),
                    content("a", "late"),
                ],
                vec![("a", "late")],
            ),
            (
                vec![
                    completed("a", false, "done"),
                    content("a", "old invocation"),
                    started(tool("a", "A")),
                    content("b", "first"),
                    content("a", "new invocation"),
                ],
                vec![("b", "first"), ("a", "new invocation")],
            ),
            (
                vec![
                    content("q", "hidden"),
                    elicitation_requested("q"),
                    content("q", "still hidden"),
                    completed("q", false, ""),
                    content("q", "ordinary reuse"),
                ],
                vec![("q", "ordinary reuse")],
            ),
        ];
        for (events, expected) in cases {
            for split in 0..=events.len() {
                let mut model = TranscriptModel::new();
                for (index, event) in events.iter().enumerate() {
                    let seq = index as u64 + 1;
                    if index < split {
                        model.observe_prior(
                            seq,
                            event,
                            &crate::acp::event_sender::EventMetadata::history(),
                        );
                    } else {
                        model.apply_scoped_event(
                            seq,
                            event,
                            &crate::acp::event_sender::EventMetadata::history(),
                        );
                    }
                }
                let snapshot = model.into_retained();
                assert_eq!(
                    snapshot
                        .unfinished_outputs
                        .iter()
                        .map(|output| (output.tool_call_id.as_str(), output.text.as_str()))
                        .collect::<Vec<_>>(),
                    expected,
                    "split at {split}: {events:?}"
                );
            }
        }
    }

    #[test]
    fn retained_snapshot_keeps_latest_unfinished_output_without_inventing_rows() {
        let cases = [
            (
                vec![
                    content("a", "old"),
                    content("b", "second"),
                    content("a", "latest"),
                ],
                vec![("a", "latest"), ("b", "second")],
            ),
            (vec![content("a", "old"), content("a", "")], vec![]),
            (
                vec![
                    content("a", "old"),
                    completed("a", false, "done"),
                    content("a", "new invocation"),
                ],
                vec![("a", "new invocation")],
            ),
            (
                vec![
                    content("q", "hidden"),
                    elicitation_requested("q"),
                    content("q", "still hidden"),
                    completed("q", false, ""),
                    content("q", "ordinary reuse"),
                ],
                vec![("q", "ordinary reuse")],
            ),
            (
                vec![
                    content("q", "hidden"),
                    elicitation_requested("q"),
                    content("q", "still hidden"),
                ],
                vec![],
            ),
            (
                vec![content("a", "old"), completed("a", false, "final")],
                vec![],
            ),
        ];
        for (events, expected) in cases {
            let model = fold(events);
            let original_rows = model.rows().to_vec();
            let snapshot = model.into_retained();
            assert_eq!(
                snapshot.rows, original_rows,
                "snapshot must not fabricate lifecycle rows"
            );
            assert_eq!(
                snapshot
                    .unfinished_outputs
                    .iter()
                    .map(|output| (output.tool_call_id.as_str(), output.text.as_str()))
                    .collect::<Vec<_>>(),
                expected
            );
        }
    }

    fn kinds(m: &TranscriptModel) -> Vec<TranscriptRowKind> {
        m.rows().iter().map(|r| r.kind).collect()
    }

    fn count(m: &TranscriptModel, kind: TranscriptRowKind) -> usize {
        m.rows().iter().filter(|r| r.kind == kind).count()
    }

    fn row<'a>(m: &'a TranscriptModel, id: &str) -> &'a TranscriptRow {
        m.rows()
            .iter()
            .find(|r| r.id == id)
            .unwrap_or_else(|| panic!("no row {id}"))
    }

    fn tool_of<'a>(m: &'a TranscriptModel, id: &str) -> &'a ToolCall {
        row(m, id).tool.as_ref().expect("tool payload")
    }

    #[test]
    fn notice_divider_and_control_events_render_per_event() {
        let resets_at = at(1_767_225_600);
        let switched = Event::AgentSwitched {
            from: "claude".into(),
            to: "codex".into(),
            reason: "rate_limit".into(),
        };
        // (event, row id, kind, text)
        let cases = [
            (
                Event::startup_error("node missing"),
                "notice-1",
                TranscriptRowKind::Notice,
                "agent startup failed: node missing".to_string(),
            ),
            (
                Event::PromptRuntimeError { message: "stream died".into(), evidence: None, native_subject: None },
                "notice-1",
                TranscriptRowKind::Notice,
                "prompt failed: stream died".to_string(),
            ),
            (
                Event::ModeSwitchFailed {
                    mode_id: "bypassPermissions".into(),
                    reason: "denied".into(),
                },
                "notice-1",
                TranscriptRowKind::Notice,
                "mode switch to \"bypassPermissions\" failed: denied".to_string(),
            ),
            (
                Event::SessionNotice {
                    severity: "warning".into(),
                    title: "Model fallback".into(),
                    description: Some("Switched to Sonnet.".into()),
                    key: None,
                },
                "notice-1",
                TranscriptRowKind::Advisory,
                "warning: Model fallback: Switched to Sonnet.".to_string(),
            ),
            (
                Event::SessionNotice {
                    severity: "info".into(),
                    title: "Task stopped by user".into(),
                    description: None,
                    key: None,
                },
                "notice-1",
                TranscriptRowKind::Advisory,
                "info: Task stopped by user".to_string(),
            ),
            (
                Event::RateLimitAutoResumed {
                    resets_at,
                    manual: false,
                },
                "notice-1",
                TranscriptRowKind::Notice,
                format!("auto-resumed at {resets_at} after rate-limit park"),
            ),
            // RESUME NOW is the user's own resume, so it is not called automatic.
            (
                Event::RateLimitAutoResumed {
                    resets_at,
                    manual: true,
                },
                "notice-1",
                TranscriptRowKind::Notice,
                format!("resumed at {resets_at} after rate-limit park"),
            ),
            (
                Event::SessionCleared,
                "cleared-1",
                TranscriptRowKind::SessionCleared,
                "Conversation cleared, the model no longer remembers earlier turns.".to_string(),
            ),
            (
                Event::ConversationCompacted,
                "compacted-1",
                TranscriptRowKind::Compacted,
                "Conversation compacted; earlier turns above are summarised in the model's context."
                    .to_string(),
            ),
            (
                Event::ConversationCompactionSummary {
                    compaction_id: "c-1".into(),
                    text: "1. Primary request".into(),
                },
                "compaction-summary-c-1",
                TranscriptRowKind::CompactionSummary,
                "1. Primary request".to_string(),
            ),
            (
                Event::ConversationSummary {
                    text: "did the thing".into(),
                    summarized_until_seq: 0,
                },
                "summary-1",
                TranscriptRowKind::Summary,
                "did the thing".to_string(),
            ),
            (
                switched,
                "agent-switched-1",
                TranscriptRowKind::SessionCleared,
                "Switched structured view agent from claude to codex (rate_limit).".to_string(),
            ),
        ];
        for (event, id, kind, text) in cases {
            let m = fold([event]);
            let got = row(&m, id);
            assert_eq!((got.kind, got.text.as_str()), (kind, text.as_str()), "{id}");
        }

        // Control-only events produce no rows.
        let m = fold([
            Event::PlanUpdated {
                plan: crate::acp::state::Plan {
                    plan_id: "p".into(),
                    version: 1,
                    steps: Vec::new(),
                },
            },
            Event::ThinkingEnded,
            Event::RawAgentUpdate {
                payload: serde_json::json!({"x": 1}),
            },
            Event::TodoListUpdated { todos: Vec::new() },
            Event::ConversationCompactionStarted,
        ]);
        assert!(m.rows().is_empty());
        assert_eq!(m.last_seq(), 5);

        // A context-reset divider needs a reason or a prior prompt.
        let reset = |reason: &str| Event::SessionContextReset {
            reason: reason.into(),
        };
        assert!(fold([reset("")]).rows().is_empty());
        assert!(fold([reset("session/load failed: bad id")])
            .rows()
            .is_empty());

        // A view switch cleared the history, but the worker knew a conversation existed.
        let m = fold([reset(
            "Conversation context reset; session/load failed: bad id",
        )]);
        assert_eq!(
            m.rows().last().unwrap().kind,
            TranscriptRowKind::ContextReset
        );

        let isolated = fold([reset("Sandbox native history was isolated")]);
        let last = isolated.rows().last().unwrap();
        assert_eq!(last.kind, TranscriptRowKind::ContextReset);
        assert!(last.text.contains("isolated"));

        let m = fold([prompt("hi"), reset("session/load failed: bad id")]);
        let last = m.rows().last().unwrap();
        assert_eq!(
            (last.kind, last.text.as_str()),
            (
                TranscriptRowKind::ContextReset,
                "session/load failed: bad id"
            )
        );
        let m = fold([prompt("hi"), reset("")]);
        assert!(m.rows().last().unwrap().text.contains("context reset"));
    }

    #[test]
    fn prompts_append_keyed_rows_and_replays_are_dropped() {
        let mut m = TranscriptModel::new();
        assert_eq!(
            m.apply_scoped_event(
                1,
                &prompt("hi"),
                &crate::acp::event_sender::EventMetadata::history()
            )
            .len(),
            1
        );
        assert!(m
            .apply_scoped_event(
                1,
                &prompt("ignored"),
                &crate::acp::event_sender::EventMetadata::history()
            )
            .is_empty());
        assert!(m
            .apply_scoped_event(
                0,
                &prompt("older"),
                &crate::acp::event_sender::EventMetadata::history()
            )
            .is_empty());
        assert_eq!((m.rows().len(), m.last_seq()), (1, 1));

        let with_attachment = Event::UserPromptSent {
            merged_prompt_ids: Vec::new(),
            text: "look".into(),
            attachments: vec![PromptAttachmentRef {
                id: "att-1".into(),
                kind: PromptAttachmentKind::Image,
                mime_type: "image/png".into(),
                name: Some("shot.png".into()),
                size: 1234,
            }],
            prompt_id: None,
            synthesized: false,
            origin_plugin: None,
            origin: crate::daemon::PromptOrigin::Unknown,
        };
        let mut m = TranscriptModel::new();
        let deltas = m.apply_scoped_event(
            3,
            &with_attachment,
            &crate::acp::event_sender::EventMetadata::history(),
        );
        assert!(matches!(deltas.as_slice(), [TranscriptDelta::Append(_)]));
        let prompt_row = row(&m, "user-seq-3");
        assert_eq!(prompt_row.kind, TranscriptRowKind::UserPrompt);
        assert_eq!(prompt_row.attachments[0].id, "att-1");

        for (prompt_id, want) in [
            (Some("cmp-abc"), "cmp-abc"),
            (None, "user-seq-7"),
            (Some(""), "user-seq-7"),
        ] {
            let mut m = TranscriptModel::new();
            m.apply_scoped_event(
                7,
                &Event::UserPromptSent {
                    merged_prompt_ids: Vec::new(),
                    text: "hi".into(),
                    attachments: Vec::new(),
                    prompt_id: prompt_id.map(Into::into),
                    synthesized: false,
                    origin_plugin: None,
                    origin: crate::daemon::PromptOrigin::Unknown,
                },
                &crate::acp::event_sender::EventMetadata::history(),
            );
            assert_eq!(m.rows()[0].id, want, "prompt_id={prompt_id:?}");
        }

        let mut m = TranscriptModel::new();
        m.apply_scoped_event(
            8,
            &Event::UserPromptSent {
                merged_prompt_ids: vec!["q2".into()],
                text: "a\n\nb".into(),
                attachments: Vec::new(),
                prompt_id: Some("q1".into()),
                synthesized: false,
                origin_plugin: None,
                origin: crate::daemon::PromptOrigin::Unknown,
            },
            &crate::acp::event_sender::EventMetadata::history(),
        );
        assert_eq!(m.rows()[0].id, "q1");
        assert_eq!(m.rows()[0].merged_prompt_ids, ["q2"]);

        // A drain retry republishes the same ids: one row, patched in place.
        let retry = Event::UserPromptSent {
            merged_prompt_ids: vec!["q2".into()],
            text: "a\n\nb".into(),
            attachments: Vec::new(),
            prompt_id: Some("q1".into()),
            synthesized: false,
            origin_plugin: None,
            origin: crate::daemon::PromptOrigin::Unknown,
        };
        let deltas = m.apply_scoped_event(
            9,
            &retry,
            &crate::acp::event_sender::EventMetadata::history(),
        );
        assert!(matches!(deltas.as_slice(), [TranscriptDelta::Patch { id, .. }] if id == "q1"));
        assert_eq!(m.rows().len(), 1);

        let m = fold([Event::UserDiffCommentsPrompt {
            intro: "look".into(),
            outro: "thanks".into(),
            is_multi_repo: true,
            comments: Vec::new(),
            assembled_markdown: "# body".into(),
        }]);
        let diff_row = row(&m, "user-seq-1");
        assert_eq!(
            (diff_row.kind, diff_row.text.as_str()),
            (TranscriptRowKind::UserDiffComments, "# body")
        );
        let payload = diff_row.diff_comments.as_ref().expect("payload");
        assert!(payload.is_multi_repo && payload.intro == "look");

        // Message chunks share a group until another event breaks it.
        let m = fold([
            chunk("Hello"),
            chunk(", world"),
            Event::ThinkingStarted,
            chunk("second"),
        ]);
        let msgs: Vec<&TranscriptRow> = m
            .rows()
            .iter()
            .filter(|r| r.kind == TranscriptRowKind::Message)
            .collect();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].text, "Hello");
        assert_eq!(msgs[0].group_id, msgs[1].group_id);
        assert_ne!(msgs[1].group_id, msgs[2].group_id);

        // Synthesized resume prompts are not transcript rows.
        let ev = Event::UserPromptSent {
            merged_prompt_ids: Vec::new(),
            text: "run the nightly task".into(),
            attachments: Vec::new(),
            prompt_id: None,
            synthesized: true,
            origin_plugin: None,
            origin: crate::daemon::PromptOrigin::Unknown,
        };
        let mut m = TranscriptModel::new();
        let deltas =
            m.apply_scoped_event(1, &ev, &crate::acp::event_sender::EventMetadata::history());
        assert!(deltas.is_empty());
        assert!(m.rows().is_empty());

        // A plugin-sent message keeps its sender on the row.
        let ev = Event::UserPromptSent {
            merged_prompt_ids: Vec::new(),
            text: "PR #7 was merged".into(),
            attachments: Vec::new(),
            prompt_id: None,
            synthesized: false,
            origin_plugin: Some("cron".into()),
            origin: crate::daemon::PromptOrigin::Unknown,
        };
        let mut m = TranscriptModel::new();
        m.apply_scoped_event(1, &ev, &crate::acp::event_sender::EventMetadata::history());
        assert_eq!(m.rows()[0].origin_plugin.as_deref(), Some("cron"));
    }

    #[test]
    fn tool_rows_pick_their_text_stay_unique_and_synthesize_late_starts() {
        let m = fold([
            started(tool("t-1", "Bash")),
            completed("t-1", false, "abc\ndef\n"),
        ]);
        assert_eq!(
            kinds(&m),
            [
                TranscriptRowKind::ToolStart,
                TranscriptRowKind::ToolComplete
            ]
        );
        assert_eq!(tool_of(&m, "start-t-1").name, "Bash");
        assert_eq!(row(&m, "done-t-1").text, "abc\ndef\n");
        assert_eq!(row(&m, "start-t-1").group_id, row(&m, "done-t-1").group_id);

        // (buffered stream, completion content, error, text, kind)
        let cases = [
            (
                Some("streamed"),
                "final",
                false,
                "final",
                TranscriptRowKind::ToolComplete,
            ),
            (
                Some("line1\n"),
                "",
                false,
                "line1\n",
                TranscriptRowKind::ToolComplete,
            ),
            (
                None,
                "",
                false,
                "completed",
                TranscriptRowKind::ToolComplete,
            ),
            (None, "", true, "tool failed", TranscriptRowKind::ToolError),
        ];
        for (i, (buffered, text, is_error, want_text, want_kind)) in cases.into_iter().enumerate() {
            let mut events = vec![started(tool("t", "Bash"))];
            events.extend(buffered.map(|b| content("t", b)));
            events.push(completed("t", is_error, text));
            let m = fold(events);
            let done = row(&m, "done-t");
            assert_eq!(
                (done.text.as_str(), done.kind),
                (want_text, want_kind),
                "case {i}"
            );
            assert!(
                matches!(m.tools.get("t"), Some(ToolLifecycle::Terminal(output)) if output.text.is_empty()),
                "case {i}: buffer drained"
            );
        }

        let m = fold([
            started(tool("t", "Bash")),
            completed("t", false, "first"),
            completed("t", false, "second"),
        ]);
        assert_eq!(
            row(&m, "done-t-3").text,
            "second",
            "a reused id is seq-disambiguated"
        );
        assert_eq!(count(&m, TranscriptRowKind::ToolComplete), 2);

        let mut m = fold([started(tool("task-1", "Task"))]);
        let deltas = m.apply_scoped_event(
            2,
            &content("task-1", "streaming"),
            &crate::acp::event_sender::EventMetadata::history(),
        );
        assert!(
            matches!(deltas.as_slice(), [TranscriptDelta::Patch { row, .. }]
            if matches!(row.output.as_slice(), [ToolOutputBlock::Text { text }] if text == "streaming"))
        );
        m.apply_scoped_event(
            3,
            &Event::ToolCallCompleted {
                tool_call_id: "task-1".into(),
                is_error: false,
                content: "Async agent launched successfully".into(),
                output: Vec::new(),
                completed_at: at(200),
                async_subagent: true,
            },
            &crate::acp::event_sender::EventMetadata::history(),
        );
        assert!(row(&m, "done-task-1").async_subagent);

        // Late frames synthesize a tool start.
        let m = fold([completed("orphan-1", false, "done output")]);
        assert_eq!(
            kinds(&m),
            [
                TranscriptRowKind::ToolStart,
                TranscriptRowKind::ToolComplete
            ]
        );
        assert_eq!(tool_of(&m, "start-orphan-1").name, "tool call");

        let m = fold([updated(
            "orphan-2",
            Some("run_shell_command"),
            Some(r#"{"command":"ls"}"#),
            None,
        )]);
        let synth = tool_of(&m, "start-orphan-2");
        assert_eq!(
            (synth.name.as_str(), synth.args_preview.as_str()),
            ("run_shell_command", r#"{"command":"ls"}"#)
        );
        assert_eq!(synth.kind, "other");
    }

    #[test]
    fn repeated_starts_and_updates_merge_without_losing_richer_fields() {
        let diff = |path: &str| DiffPreview {
            path: path.into(),
            old_text: None,
            new_text: Some("x".into()),
            created_at: at(100),
        };
        let mut m = fold([
            started(ToolCall {
                args_preview: r#"{"x":1}"#.into(),
                started_at: at(110),
                diffs: vec![diff("a.rs")],
                ..tool("dup", "Bash")
            }),
            started(tool("other", "Other")),
        ]);
        let deltas = m.apply_scoped_event(
            3,
            &started(ToolCall {
                kind: "other".into(),
                args_preview: "".into(),
                started_at: at(105),
                parent_tool_call_id: Some("parent-9".into()),
                memory_recall: Some(MemoryRecall {
                    mode: "recall".into(),
                    paths: vec!["/a".into()],
                    synthesized_text: None,
                }),
                ..tool("dup", "Bash")
            }),
            &crate::acp::event_sender::EventMetadata::history(),
        );
        assert!(
            matches!(deltas.as_slice(), [TranscriptDelta::Patch { id, .. }] if id == "start-dup")
        );
        assert_eq!(count(&m, TranscriptRowKind::ToolStart), 2);
        let merged = tool_of(&m, "start-dup");
        assert_eq!(merged.args_preview, r#"{"x":1}"#, "richer args survive");
        assert_eq!(
            merged.kind, "execute",
            "richer kind survives a sparse 'other'"
        );
        assert_eq!(merged.started_at, at(110), "the later started_at wins");
        assert_eq!(merged.parent_tool_call_id.as_deref(), Some("parent-9"));
        assert_eq!(merged.memory_recall.as_ref().unwrap().mode, "recall");

        let deltas = m.apply_scoped_event(
            4,
            &updated(
                "dup",
                Some("Edit a.rs"),
                Some(r#"{"command":"git log"}"#),
                None,
            ),
            &crate::acp::event_sender::EventMetadata::history(),
        );
        assert!(
            matches!(deltas.as_slice(), [TranscriptDelta::Patch { id, .. }] if id == "start-dup")
        );
        assert_eq!(row(&m, "start-dup").text, "Edit a.rs");
        assert_eq!(
            tool_of(&m, "start-dup").diffs,
            [diff("a.rs")],
            "no diffs keeps the old ones"
        );
        assert_eq!(
            tool_of(&m, "start-other").args_preview,
            "{}",
            "other rows untouched"
        );
        m.apply_scoped_event(
            5,
            &updated("dup", None, None, Some(vec![diff("b.rs")])),
            &crate::acp::event_sender::EventMetadata::history(),
        );
        assert_eq!(
            tool_of(&m, "start-dup").diffs,
            [diff("b.rs")],
            "diffs replace wholesale"
        );
        assert_eq!(tool_of(&m, "start-dup").name, "Edit a.rs");
    }

    /// A new ACP message id opens a new message; one streamed message,
    /// or chunks without an id, keep joining the open one.
    #[test]
    fn agent_message_chunks_split_on_a_new_message_id() {
        let chunk = |text: &str, id: Option<&str>| Event::AgentMessageChunk {
            text: text.into(),
            message_id: id.map(Into::into),
        };
        let a = Some("m-a");
        let b = Some("m-b");
        // (events, message_start per message row, whether the rows share one group)
        let cases: [(Vec<Event>, Vec<bool>, bool); 7] = [
            (
                vec![chunk("one mes", a), chunk("sage", a)],
                vec![false, false],
                true,
            ),
            (
                vec![chunk("First.", a), chunk("", b), chunk("Second", b)],
                vec![false, true, false],
                false,
            ),
            (
                vec![chunk("x", None), chunk("y", None)],
                vec![false, false],
                true,
            ),
            (
                vec![chunk("x", a), chunk("y", None)],
                vec![false, false],
                true,
            ),
            (
                vec![chunk("x", None), chunk("y", a)],
                vec![false, false],
                true,
            ),
            // An event between the chunks of one message is not a boundary.
            (
                vec![chunk("x", a), Event::ThinkingStarted, chunk("y", a)],
                vec![false, false],
                false,
            ),
            // A new turn starts fresh: its first message is not marked.
            (
                vec![chunk("x", a), prompt("next"), chunk("y", b)],
                vec![false, false],
                false,
            ),
        ];
        for (events, starts, one_group) in cases {
            let label = format!("{events:?}");
            let m = fold(events);
            let messages: Vec<_> = m
                .rows()
                .iter()
                .filter(|r| r.kind == TranscriptRowKind::Message)
                .collect();
            let got: Vec<bool> = messages.iter().map(|r| r.message_start).collect();
            assert_eq!(got, starts, "{label}");
            let groups: HashSet<_> = messages.iter().map(|r| &r.group_id).collect();
            assert_eq!(groups.len() == 1, one_group, "{label}");
        }
    }

    /// #5088: thought chunks fold into `thinking` rows with their own groups,
    /// split by a new thought id or any other event, and blank ones render
    /// nothing but still carry a boundary.
    #[test]
    fn agent_thought_chunks_fold_into_thinking_rows() {
        let thought = |text: &str, id: Option<&str>| Event::AgentThoughtChunk {
            text: text.into(),
            message_id: id.map(Into::into),
        };
        let msg = |text: &str| Event::AgentMessageChunk {
            text: text.into(),
            message_id: None,
        };
        let (a, b) = (Some("t-a"), Some("t-b"));
        // (events, (text, message_start) per thinking row, thinking groups)
        let cases: [(Vec<Event>, Vec<(&str, bool)>, usize); 7] = [
            (
                vec![
                    thought("Weighing", a),
                    thought(" ", a),
                    thought("options", a),
                ],
                vec![("Weighing", false), (" ", false), ("options", false)],
                1,
            ),
            (
                vec![thought("one ", a), thought("two", a)],
                vec![("one ", false), ("two", false)],
                1,
            ),
            (
                vec![thought("one", a), thought(" ", b), thought("two", b)],
                vec![("one", false), ("two", true)],
                2,
            ),
            (
                vec![thought("one", a), msg("reply"), thought("two", a)],
                vec![("one", false), ("two", false)],
                2,
            ),
            (vec![thought("", None), thought("\n", a)], vec![], 0),
            (
                vec![thought("one", a), prompt("next"), thought("two", b)],
                vec![("one", false), ("two", false)],
                2,
            ),
            (
                vec![thought("x", None), thought("y", None)],
                vec![("x", false), ("y", false)],
                1,
            ),
        ];
        for (events, want, groups) in cases {
            let label = format!("{events:?}");
            let m = fold(events);
            let thinking: Vec<_> = m
                .rows()
                .iter()
                .filter(|r| r.kind == TranscriptRowKind::Thinking)
                .collect();
            let got: Vec<(&str, bool)> = thinking
                .iter()
                .map(|r| (r.text.as_str(), r.message_start))
                .collect();
            assert_eq!(got, want, "{label}");
            let distinct: HashSet<_> = thinking.iter().map(|r| &r.group_id).collect();
            assert_eq!(distinct.len(), groups, "{label}");
        }

        // A thought splits the message around it, and a partial fold that
        // observed an earlier thought id marks the next thought's start.
        let m = fold(vec![msg("a"), thought("t", a), msg("b")]);
        assert_eq!(
            kinds(&m),
            [
                TranscriptRowKind::Message,
                TranscriptRowKind::Thinking,
                TranscriptRowKind::Message
            ]
        );
        let mut m = TranscriptModel::new();
        m.observe_prior(
            1,
            &thought("earlier", a),
            &crate::acp::event_sender::EventMetadata::history(),
        );
        m.apply_scoped_event(
            2,
            &thought("later", b),
            &crate::acp::event_sender::EventMetadata::history(),
        );
        assert!(m.rows()[0].message_start);
    }

    /// A fold that starts after earlier events (a reconnect, a rows page)
    /// still marks the first chunk of a new message.
    #[test]
    fn observed_prior_chunks_carry_the_message_id_into_a_partial_fold() {
        let chunk = |text: &str, id: &str| Event::AgentMessageChunk {
            text: text.into(),
            message_id: Some(id.into()),
        };
        for (prior, expected) in [
            (vec![chunk("x", "m-a")], true),
            (vec![chunk("x", "m-b")], false),
            (vec![chunk("x", "m-a"), prompt("next")], false),
            (vec![], false),
        ] {
            let mut m = TranscriptModel::new();
            for (index, event) in prior.iter().enumerate() {
                m.observe_prior(
                    index as u64 + 1,
                    event,
                    &crate::acp::event_sender::EventMetadata::history(),
                );
            }
            m.apply_scoped_event(
                10,
                &chunk("", "m-b"),
                &crate::acp::event_sender::EventMetadata::history(),
            );
            assert!(m.rows().len() == 1, "{prior:?}");
            assert_eq!(m.rows()[0].message_start, expected, "{prior:?}");
        }
    }

    /// A fold that starts after `ElicitationRequested` still suppresses the
    /// elicitation tool's card, so a reconnect cannot leak a synthesized one.
    #[test]
    fn partial_fold_keeps_elicitation_suppression() {
        let events = [
            started(tool("q", "AskUserQuestion")),
            elicitation_requested("q"),
            completed("q", false, "Your questions have been answered"),
            // The adapter reuses the id for an ordinary tool after the answer.
            started(tool("q", "Bash")),
            completed("q", false, "ran"),
        ];
        let full = fold(events.clone());
        assert_eq!(count(&full, TranscriptRowKind::ToolStart), 1);
        assert_eq!(count(&full, TranscriptRowKind::ToolComplete), 1);
        for split in 0..events.len() {
            let mut m = TranscriptModel::new();
            for (i, e) in events.iter().enumerate() {
                if i < split {
                    m.observe_prior(
                        i as u64 + 1,
                        e,
                        &crate::acp::event_sender::EventMetadata::history(),
                    );
                } else {
                    m.apply_scoped_event(
                        i as u64 + 1,
                        e,
                        &crate::acp::event_sender::EventMetadata::history(),
                    );
                }
            }
            let expected = if split == events.len() - 1 {
                vec![TranscriptRowKind::ToolComplete]
            } else {
                kinds(&full)
            };
            assert_eq!(kinds(&m), expected, "split at {split}");
        }
    }

    #[test]
    fn prefix_tool_lifecycle_retains_output_and_order_without_rendering_old_rows() {
        use TranscriptRowKind as K;
        for (label, prefix, suffix, expected) in [
            (
                "open",
                vec![
                    started(tool("t", "T")),
                    content("t", "old"),
                    content("t", "latest"),
                ],
                vec![stopped("cancelled")],
                vec![(K::ToolStopped, "t", "latest")],
            ),
            (
                "completed",
                vec![started(tool("t", "T")), completed("t", false, "done")],
                vec![stopped("cancelled")],
                vec![],
            ),
            (
                "already stopped",
                vec![started(tool("t", "T")), stopped("cancelled")],
                vec![stopped("cancelled")],
                vec![],
            ),
            (
                "update first",
                vec![updated("t", Some("Read"), None, None)],
                vec![stopped("cancelled")],
                vec![(K::ToolStopped, "t", "")],
            ),
            (
                "content alone",
                vec![content("t", "buffered")],
                vec![stopped("cancelled")],
                vec![],
            ),
            (
                "ordered reuse",
                vec![
                    started(tool("b", "B")),
                    started(tool("a", "A")),
                    completed("b", false, ""),
                    started(tool("c", "C")),
                    started(tool("b", "B again")),
                ],
                vec![stopped("cancelled")],
                vec![
                    (K::ToolStopped, "a", ""),
                    (K::ToolStopped, "c", ""),
                    (K::ToolStopped, "b", ""),
                ],
            ),
            (
                "elicitation",
                vec![
                    started(tool("q", "AskUserQuestion")),
                    elicitation_requested("q"),
                    content("q", "not a tool result"),
                ],
                vec![
                    updated("q", Some("Question"), None, None),
                    stopped("cancelled"),
                ],
                vec![],
            ),
            (
                "elicitation id reused",
                vec![
                    started(tool("q", "AskUserQuestion")),
                    elicitation_requested("q"),
                    completed("q", false, "answered"),
                    started(tool("q", "Bash")),
                ],
                vec![stopped("cancelled")],
                vec![(K::ToolStopped, "q", "")],
            ),
            (
                "content before live start",
                vec![content("t", "prefix output")],
                vec![started(tool("t", "Bash")), stopped("cancelled")],
                vec![
                    (K::ToolStart, "t", "Bash"),
                    (K::ToolStopped, "t", "prefix output"),
                ],
            ),
            (
                "new start drops old trailing output",
                vec![
                    started(tool("t", "T")),
                    completed("t", false, "done"),
                    content("t", "old trailing output"),
                ],
                vec![started(tool("t", "Bash")), stopped("cancelled")],
                vec![(K::ToolStart, "t", "Bash"), (K::ToolStopped, "t", "")],
            ),
        ] {
            let mut model = TranscriptModel::new();
            for (index, event) in prefix.iter().enumerate() {
                model.observe_prior(
                    index as u64 + 1,
                    event,
                    &crate::acp::event_sender::EventMetadata::history(),
                );
                assert!(model.rows().is_empty(), "{label}: prefix stays unrendered");
            }
            for (index, event) in suffix.iter().enumerate() {
                let seq = 100 + index as u64;
                model.apply_scoped_event(
                    seq,
                    event,
                    &crate::acp::event_sender::EventMetadata::history(),
                );
                assert!(
                    model
                        .apply_scoped_event(
                            seq,
                            event,
                            &crate::acp::event_sender::EventMetadata::history()
                        )
                        .is_empty(),
                    "{label}: replay overlap"
                );
            }
            let actual: Vec<_> = model
                .rows()
                .iter()
                .map(|row| {
                    let id = row
                        .tool_call_id
                        .as_deref()
                        .or_else(|| row.tool.as_ref().map(|tool| tool.id.as_str()))
                        .unwrap();
                    (row.kind, id, row.text.as_str())
                })
                .collect();
            assert_eq!(actual, expected, "{label}");
            let terminals: Vec<_> = model
                .rows()
                .iter()
                .map(|row| (row.id.clone(), row.at))
                .collect();
            model.apply_scoped_event(
                1000,
                &stopped("prompt_complete"),
                &crate::acp::event_sender::EventMetadata::history(),
            );
            assert_eq!(
                model
                    .rows()
                    .iter()
                    .map(|row| (row.id.clone(), row.at))
                    .collect::<Vec<_>>(),
                terminals,
                "{label}: no second terminal or ticking end time"
            );
        }
    }

    #[test]
    fn prefix_start_completion_emits_only_its_correlated_terminal() {
        for (is_error, final_content, expected) in [
            (false, "", "latest streamed output"),
            (true, "final result", "final result"),
        ] {
            let mut model = TranscriptModel::new();
            for (index, event) in [
                started(tool("t", "Bash")),
                content("t", "old snapshot"),
                content("t", "latest streamed output"),
            ]
            .iter()
            .enumerate()
            {
                model.observe_prior(
                    index as u64 + 1,
                    event,
                    &crate::acp::event_sender::EventMetadata::history(),
                );
            }
            assert!(model.rows().is_empty());
            let output = vec![ToolOutputBlock::Text {
                text: "structured output".into(),
            }];
            let deltas = model.apply_scoped_event(
                10,
                &Event::ToolCallCompleted {
                    tool_call_id: "t".into(),
                    is_error,
                    content: final_content.into(),
                    output: output.clone(),
                    completed_at: at(200),
                    async_subagent: true,
                },
                &crate::acp::event_sender::EventMetadata::history(),
            );
            assert!(matches!(deltas.as_slice(), [TranscriptDelta::Append(_)]));
            let terminal = &model.rows()[0];
            assert_eq!(
                terminal.kind,
                if is_error {
                    TranscriptRowKind::ToolError
                } else {
                    TranscriptRowKind::ToolComplete
                }
            );
            assert_eq!(terminal.tool_call_id.as_deref(), Some("t"));
            assert_eq!(terminal.text, expected);
            assert_eq!(terminal.at, at(200));
            assert_eq!(terminal.output, output);
            assert!(terminal.async_subagent);
            assert!(model
                .apply_scoped_event(
                    11,
                    &stopped("prompt_complete"),
                    &crate::acp::event_sender::EventMetadata::history()
                )
                .is_empty());
        }
    }

    #[test]
    fn observed_prefix_advances_watermark_for_every_event() {
        let mut model = TranscriptModel::new();
        for (index, event) in [
            chunk("prior message"),
            Event::AgentThoughtChunk {
                text: "prior thought".into(),
                message_id: Some("thought".into()),
            },
            prompt("next turn"),
            Event::ThinkingStarted,
            started(tool("prior", "Prior tool")),
            updated("prior", Some("Prior title"), None, None),
            content("prior", "prior output"),
            elicitation_requested("prior"),
            completed("prior", false, "answered"),
            stopped("prompt_complete"),
        ]
        .iter()
        .enumerate()
        {
            let seq = index as u64 + 1;
            model.observe_prior(
                seq,
                event,
                &crate::acp::event_sender::EventMetadata::history(),
            );
            assert_eq!(model.last_seq(), seq, "{event:?}");
            assert!(model.rows().is_empty(), "{event:?}");
            assert!(
                model
                    .apply_scoped_event(
                        seq,
                        event,
                        &crate::acp::event_sender::EventMetadata::history()
                    )
                    .is_empty(),
                "{event:?}"
            );
        }
    }

    #[test]
    fn observed_prefix_rejects_stale_stop_before_later_tool() {
        let mut model = TranscriptModel::new();
        model.observe_prior(
            1,
            &stopped("prompt_complete"),
            &crate::acp::event_sender::EventMetadata::history(),
        );
        model.observe_prior(
            2,
            &started(tool("later", "Later tool")),
            &crate::acp::event_sender::EventMetadata::history(),
        );
        model.observe_prior(
            1,
            &stopped("prompt_complete"),
            &crate::acp::event_sender::EventMetadata::history(),
        );
        assert_eq!(model.last_seq(), 2, "stale observation keeps the watermark");
        assert!(model.rows().is_empty());
        assert!(
            model
                .apply_scoped_event(
                    1,
                    &stopped("prompt_complete"),
                    &crate::acp::event_sender::EventMetadata::history()
                )
                .is_empty(),
            "a queued prefix stop must not close a tool opened later in the prefix"
        );
        model.apply_scoped_event(
            3,
            &stopped("cancelled"),
            &crate::acp::event_sender::EventMetadata::history(),
        );
        assert_eq!(count(&model, TranscriptRowKind::ToolStopped), 1);
        assert_eq!(
            row(&model, "stopped-later-3").tool_call_id.as_deref(),
            Some("later")
        );
        assert!(model
            .apply_scoped_event(
                4,
                &stopped("cancelled"),
                &crate::acp::event_sender::EventMetadata::history()
            )
            .is_empty());
    }

    #[test]
    fn orphan_output_clears_at_observed_turn_endings_without_stopped_rows() {
        for closer in [
            stopped("cancelled"),
            Event::startup_error("boom"),
            Event::IncompatibleAgent {
                detail: crate::acp::state::StartupErrorDetail::UnsupportedProtocolVersion {
                    expected: "1".into(),
                    received: "2".into(),
                },
            },
            Event::AgentSwitched {
                from: "claude".into(),
                to: "codex".into(),
                reason: "rate_limit".into(),
            },
        ] {
            let mut model = TranscriptModel::new();
            model.observe_prior(
                1,
                &content("reused", "old orphan output"),
                &crate::acp::event_sender::EventMetadata::history(),
            );
            model.observe_prior(
                2,
                &closer,
                &crate::acp::event_sender::EventMetadata::history(),
            );
            assert!(model.rows().is_empty(), "{closer:?}: prefix stays hidden");
            model.apply_scoped_event(
                10,
                &started(tool("reused", "New call")),
                &crate::acp::event_sender::EventMetadata::history(),
            );
            model.apply_scoped_event(
                11,
                &stopped("cancelled"),
                &crate::acp::event_sender::EventMetadata::history(),
            );
            assert_eq!(
                count(&model, TranscriptRowKind::ToolStopped),
                1,
                "{closer:?}"
            );
            assert_eq!(
                row(&model, "stopped-reused-11").text,
                "",
                "{closer:?}: a reused ID must not inherit orphan output"
            );
            assert!(model
                .apply_scoped_event(
                    12,
                    &stopped("cancelled"),
                    &crate::acp::event_sender::EventMetadata::history()
                )
                .is_empty());
        }
    }

    #[test]
    fn orphan_output_clears_at_live_turn_endings_without_stopped_rows() {
        for closer in [
            stopped("cancelled"),
            Event::startup_error("boom"),
            Event::IncompatibleAgent {
                detail: crate::acp::state::StartupErrorDetail::UnsupportedProtocolVersion {
                    expected: "1".into(),
                    received: "2".into(),
                },
            },
            Event::AgentSwitched {
                from: "claude".into(),
                to: "codex".into(),
                reason: "rate_limit".into(),
            },
        ] {
            for observed_content in [false, true] {
                let mut model = TranscriptModel::new();
                if observed_content {
                    model.observe_prior(
                        1,
                        &content("reused", "old orphan output"),
                        &crate::acp::event_sender::EventMetadata::history(),
                    );
                } else {
                    model.apply_scoped_event(
                        1,
                        &content("reused", "old orphan output"),
                        &crate::acp::event_sender::EventMetadata::history(),
                    );
                }
                model.apply_scoped_event(
                    2,
                    &closer,
                    &crate::acp::event_sender::EventMetadata::history(),
                );
                assert_eq!(
                    count(&model, TranscriptRowKind::ToolStopped),
                    0,
                    "{closer:?}: content alone must not invent a stopped tool"
                );
                model.apply_scoped_event(
                    3,
                    &started(tool("reused", "New call")),
                    &crate::acp::event_sender::EventMetadata::history(),
                );
                model.apply_scoped_event(
                    4,
                    &stopped("cancelled"),
                    &crate::acp::event_sender::EventMetadata::history(),
                );
                assert_eq!(
                    count(&model, TranscriptRowKind::ToolStopped),
                    1,
                    "{closer:?}"
                );
                assert_eq!(
                    row(&model, "stopped-reused-4").text,
                    "",
                    "{closer:?}: a reused ID must not inherit orphan output"
                );
                assert!(model
                    .apply_scoped_event(
                        5,
                        &stopped("cancelled"),
                        &crate::acp::event_sender::EventMetadata::history()
                    )
                    .is_empty());
            }
        }
    }

    #[test]
    fn metadata_after_prefix_completion_does_not_create_unpaired_start() {
        for observed_start in [false, true] {
            let mut model = TranscriptModel::new();
            if observed_start {
                model.observe_prior(
                    1,
                    &started(tool("done", "Old call")),
                    &crate::acp::event_sender::EventMetadata::history(),
                );
            }
            model.observe_prior(
                2,
                &completed("done", false, "finished before cursor"),
                &crate::acp::event_sender::EventMetadata::history(),
            );
            assert!(
                model
                    .apply_scoped_event(
                        10,
                        &updated("done", Some("Late title"), Some("new args"), None),
                        &crate::acp::event_sender::EventMetadata::history(),
                    )
                    .is_empty(),
                "metadata alone must not render a running card for a terminal prefix tool"
            );
            assert!(model.rows().is_empty());
            assert!(model
                .apply_scoped_event(
                    11,
                    &stopped("cancelled"),
                    &crate::acp::event_sender::EventMetadata::history()
                )
                .is_empty());
            model.apply_scoped_event(
                12,
                &started(tool("done", "New call")),
                &crate::acp::event_sender::EventMetadata::history(),
            );
            model.apply_scoped_event(
                13,
                &stopped("cancelled"),
                &crate::acp::event_sender::EventMetadata::history(),
            );
            assert_eq!(count(&model, TranscriptRowKind::ToolStart), 1);
            assert_eq!(count(&model, TranscriptRowKind::ToolStopped), 1);
            assert_eq!(row(&model, "start-done").text, "New call");
        }
    }

    /// ACP compaction updates are upserts by id: a late, cleared or restored
    /// summary stays beside its divider, past any later turn.
    #[test]
    fn compaction_summary_row_is_keyed_by_compaction() {
        let summary = |id: &str, text: &str| Event::ConversationCompactionSummary {
            compaction_id: id.into(),
            text: text.into(),
        };
        let mut m = fold([
            Event::ConversationCompacted,
            summary("a", ""),
            summary("b", "other"),
            Event::AgentMessageChunk {
                text: "later turn".into(),
                message_id: None,
            },
        ]);
        let ids = |m: &TranscriptModel| -> Vec<String> {
            m.rows().iter().map(|r| r.id.clone()).collect()
        };
        let layout = ids(&m);
        for (seq, text) in [(5, "late"), (6, ""), (7, "restored")] {
            let deltas = m.apply_scoped_event(
                seq,
                &summary("a", text),
                &crate::acp::event_sender::EventMetadata::history(),
            );
            assert!(
                matches!(deltas.as_slice(), [TranscriptDelta::Patch { id, row }] if id == "compaction-summary-a" && row.text == text),
                "{text:?}"
            );
            assert_eq!(ids(&m), layout, "{text:?}");
        }
        assert_eq!(layout[1], "compaction-summary-a");
        assert_eq!(row(&m, "compaction-summary-b").text, "other");
    }

    #[test]
    fn keyed_session_notice_is_replaced_in_place() {
        let failed = |reason: &str| Event::SessionNotice {
            severity: "error".into(),
            title: "Compaction failed".into(),
            description: Some(reason.into()),
            key: Some("compaction-a".into()),
        };
        let mut m = fold([
            failed("aborted"),
            Event::AgentMessageChunk {
                text: "later turn".into(),
                message_id: None,
            },
        ]);
        let deltas = m.apply_scoped_event(
            3,
            &failed("out of tokens"),
            &crate::acp::event_sender::EventMetadata::history(),
        );
        assert!(
            matches!(deltas.as_slice(), [TranscriptDelta::Patch { id, row }] if id == "notice-compaction-a" && row.text == "error: Compaction failed: out of tokens")
        );
        assert_eq!(m.rows()[0].id, "notice-compaction-a");
    }

    #[test]
    fn ask_user_question_cards_are_suppressed_in_either_order() {
        let mut m = fold([started(tool("tc-ask", "Asking"))]);
        let deltas = m.apply_scoped_event(
            2,
            &elicitation_requested("tc-ask"),
            &crate::acp::event_sender::EventMetadata::history(),
        );
        assert!(matches!(deltas.as_slice(), [TranscriptDelta::Remove(id)] if id == "start-tc-ask"));
        assert!(m
            .apply_scoped_event(
                3,
                &completed("tc-ask", false, "x"),
                &crate::acp::event_sender::EventMetadata::history()
            )
            .is_empty());
        assert!(m.rows().is_empty());

        let mut m = fold([elicitation_requested("tc-ask")]);
        assert!(m
            .apply_scoped_event(
                2,
                &started(tool("tc-ask", "Asking")),
                &crate::acp::event_sender::EventMetadata::history()
            )
            .is_empty());
        assert!(m.rows().is_empty());

        let answer = |nonce: &str, answers: Vec<ElicitationAnswer>| Event::ElicitationResolved {
            nonce: Nonce(nonce.into()),
            outcome: ElicitationOutcome::Accepted,
            answers,
        };
        let yes = || {
            vec![ElicitationAnswer {
                question: "Proceed?".into(),
                answer: "Yes".into(),
            }]
        };
        let mut m = fold([answer("el-1", yes())]);
        let answered = row(&m, "elicitation-el-1");
        assert_eq!(
            (answered.kind, answered.text.as_str()),
            (TranscriptRowKind::ElicitationAnswered, "Proceed?: Yes")
        );
        assert_eq!(answered.elicitation_answers.len(), 1);
        assert!(
            m.apply_scoped_event(
                2,
                &answer("el-1", yes()),
                &crate::acp::event_sender::EventMetadata::history()
            )
            .is_empty(),
            "re-broadcast deduped"
        );
        assert!(
            m.apply_scoped_event(
                3,
                &answer("el-2", Vec::new()),
                &crate::acp::event_sender::EventMetadata::history()
            )
            .is_empty(),
            "no answers, no row"
        );
    }

    #[test]
    fn matching_local_terminal_settles_only_its_operation_and_keeps_partial_tools() {
        use crate::acp::establishment::NativeOperationCorrelation;
        use crate::acp::event_sender::{EventMetadata, EventScope};
        use crate::acp::protocol::AcpOperationMetadata;
        let metadata = |correlation| EventMetadata {
            scope: EventScope::OperationCurrent,
            operation: Some(AcpOperationMetadata {
                correlation,
                outcome: None,
            }),
            ..EventMetadata::history()
        };
        let mut first = metadata(NativeOperationCorrelation::new(None));
        let mut second = metadata(NativeOperationCorrelation::new(None));
        let mut m = fold([
            started(tool("open", "Task")),
            content("open", "partial output"),
        ]);
        m.apply_scoped_event(3, &Event::ThinkingStarted, &first);
        m.apply_scoped_event(4, &Event::ThinkingStarted, &second);
        first.make_historical();
        m.apply_scoped_event(5, &stopped("prompt_failed"), &first);
        assert_eq!(
            m.active_operation,
            second
                .operation
                .map(|operation| operation.correlation.operation_id)
        );
        second.make_historical();
        m.apply_scoped_event(6, &stopped("prompt_failed"), &second);
        assert_eq!(m.active_operation, None);
        assert_eq!(
            m.rows().len(),
            1,
            "local terminals must not synthesize tool endings or output notices"
        );
        let partial = row(&m, "start-open");
        assert_eq!(partial.kind, TranscriptRowKind::ToolStart);
        assert!(matches!(partial.output.as_slice(),
            [ToolOutputBlock::Text { text }] if text == "partial output"));
    }

    #[test]
    fn output_diagnostics_require_an_original_result_and_matching_written_window() {
        use crate::acp::establishment::NativeOperationCorrelation;
        use crate::acp::event_sender::{EventMetadata, EventScope};
        use crate::acp::protocol::{
            AcpOperationMetadata, AcpOriginalRequestResult, AcpPromptObservation, AcpPromptWindowId,
        };

        let cases = [
            ("silence", vec![]),
            ("output", vec![chunk("hi")]),
            ("thinking", vec![Event::ThinkingStarted]),
            (
                "error",
                vec![Event::PromptRuntimeError {
                    message: "boom".into(),
                    evidence: None,
                    native_subject: None,
                }],
            ),
            ("compaction", vec![Event::ConversationCompacted]),
            (
                "notice",
                vec![Event::SessionNotice {
                    severity: "error".into(),
                    title: "Compaction failed".into(),
                    description: None,
                    key: None,
                }],
            ),
        ];
        for (label, mid) in cases {
            let mut model = fold([prompt("/usage")]);
            let mut seq = 2;
            for event in mid {
                model.apply_scoped_event(seq, &event, &EventMetadata::history());
                seq += 1;
            }
            let rows_before = model.rows().len();
            let notices_before = count(&model, TranscriptRowKind::Notice);
            let deltas = model.apply_scoped_event(
                seq,
                &stopped("prompt_complete"),
                &EventMetadata::history(),
            );
            assert!(
                deltas.is_empty(),
                "{label}: history cannot invent an output diagnosis"
            );
            assert_eq!(model.rows().len(), rows_before, "{label}");
            assert_eq!(
                count(&model, TranscriptRowKind::Notice),
                notices_before,
                "{label}"
            );
        }

        let steered = fold([
            Event::PromptCapabilities {
                image: false,
                audio: false,
                embedded_context: false,
                load_session: None,
                steering: true,
            },
            prompt("read src/acp"),
            chunk("on it"),
            prompt("just acp_client.rs"),
            stopped("prompt_complete"),
        ]);
        assert_eq!(count(&steered, TranscriptRowKind::UserPrompt), 2);
        assert!(steered
            .rows()
            .iter()
            .any(|row| { row.kind == TranscriptRowKind::Message && row.text == "on it" }));
        assert_eq!(count(&steered, TranscriptRowKind::Notice), 0);

        let window = AcpPromptWindowId::new();
        let other_window = AcpPromptWindowId::new();
        let written = Some(AcpPromptObservation::Written { window_id: window });
        for (label, outcome, observation, uncertainty, expected) in [
            (
                "original succeeded",
                Some(AcpOriginalRequestResult::Succeeded),
                written,
                Some(window),
                true,
            ),
            (
                "original failed",
                Some(AcpOriginalRequestResult::Failed),
                written,
                Some(window),
                true,
            ),
            (
                "different window",
                Some(AcpOriginalRequestResult::Succeeded),
                written,
                Some(other_window),
                false,
            ),
            ("no original result", None, written, Some(window), false),
            (
                "no written observation",
                Some(AcpOriginalRequestResult::Succeeded),
                None,
                Some(window),
                false,
            ),
            (
                "ambiguous observation",
                Some(AcpOriginalRequestResult::Succeeded),
                Some(AcpPromptObservation::Ambiguous),
                Some(window),
                false,
            ),
            (
                "nothing written",
                Some(AcpOriginalRequestResult::Succeeded),
                Some(AcpPromptObservation::NoWrittenPrompt),
                Some(window),
                false,
            ),
            (
                "no uncertainty",
                Some(AcpOriginalRequestResult::Succeeded),
                written,
                None,
                false,
            ),
        ] {
            let correlation = NativeOperationCorrelation::new(None);
            let progress = EventMetadata {
                scope: EventScope::OperationCurrent,
                operation: Some(AcpOperationMetadata {
                    correlation,
                    outcome: None,
                }),
                prompt_observation: written,
                ..EventMetadata::history()
            };
            let mut model = fold([prompt("a written operation")]);
            model.apply_scoped_event(2, &Event::ThinkingStarted, &progress);
            let terminal = EventMetadata {
                operation: Some(AcpOperationMetadata {
                    correlation,
                    outcome,
                }),
                prompt_observation: observation,
                output_uncertainty: uncertainty,
                ..EventMetadata::history()
            };
            let reason = if outcome == Some(AcpOriginalRequestResult::Failed) {
                "prompt_failed"
            } else {
                "prompt_complete"
            };
            let deltas = model.apply_scoped_event(3, &stopped(reason), &terminal);
            let notices = if expected { 1 } else { 0 };
            assert_eq!(deltas.len(), notices, "{label}");
            assert_eq!(count(&model, TranscriptRowKind::Notice), notices, "{label}");
        }
    }
}
