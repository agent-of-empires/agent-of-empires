//! `TranscriptModel`: the server-side render model for the structured-view transcript.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::elicitations::ElicitationAnswer;
use super::state::{notice_id, DiffComment, DiffPreview, Event, ToolCall, ToolOutputBlock};
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
    pub at: DateTime<Utc>,
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
    /// Structured payload on a `user_diff_comments` row; `text` holds the markdown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_comments: Option<DiffCommentsPayload>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub elicitation_answers: Vec<ElicitationAnswer>,
    /// A `tool_complete` row that launched an async sub-agent.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub async_subagent: bool,
    /// A `message` row that opens a new agent message, so a client keeps it
    /// apart from the previous message's text.
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
    UserPrompt,
    UserDiffComments,
    ElicitationAnswered,
    EmptyOutput,
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

/// Folds the ACP `Event` stream into an ordered [`TranscriptRow`] list.
#[derive(Debug, Clone, Default)]
pub struct TranscriptModel {
    rows: Vec<TranscriptRow>,
    row_ids: HashSet<String>,
    /// Tool calls surfaced as an elicitation (AskUserQuestion); their cards are suppressed.
    elicitation_tool_ids: HashSet<String>,
    /// Group of the still-open run of consecutive message chunks.
    open_message_group: Option<String>,
    /// ACP message id of the last chunk that carried one.
    last_message_id: Option<String>,
    group_counter: u64,
    /// Frames at or below this seq are dropped, so replay overlap is harmless.
    last_seq: u64,
    active_operation: Option<crate::acp::establishment::NativeOperationId>,
}

impl TranscriptModel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn rows(&self) -> &[TranscriptRow] {
        &self.rows
    }

    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// Note an event that precedes this fold without rendering it, so a chunk
    /// that opens the fold still knows which message it follows, and a
    /// completion still finds its elicitation suppressed.
    pub fn observe_prior(&mut self, event: &Event) {
        match event {
            Event::ElicitationRequested { elicitation } => {
                if let Some(id) = &elicitation.tool_call_id {
                    self.elicitation_tool_ids.insert(id.clone());
                }
            }
            Event::ToolCallCompleted { tool_call_id, .. } => {
                self.elicitation_tool_ids.remove(tool_call_id);
            }
            Event::AgentMessageChunk {
                message_id: Some(id),
                ..
            } => self.last_message_id = Some(id.clone()),
            Event::UserPromptSent { .. } | Event::UserDiffCommentsPrompt { .. } => {
                self.last_message_id = None;
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
            Event::UserPromptSent {
                text,
                attachments,
                prompt_id,
                synthesized,
                origin_plugin,
            } => {
                self.last_message_id = None;
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
                    vec![self.append(row)]
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
                    return Vec::new();
                }
                let mut deltas =
                    self.ensure_tool_start(tool_call_id, None, None, *completed_at, None);
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
                let mut deltas = self.ensure_tool_start(tool_call_id, None, None, Utc::now(), None);
                if let Some(index) = self.find_tool_start(tool_call_id) {
                    let row = &mut self.rows[index];
                    row.output = vec![ToolOutputBlock::Text {
                        text: content.clone(),
                    }];
                    deltas.push(patch(row));
                }
                deltas
            }
            Event::ToolCallUpdated {
                tool_call_id,
                title,
                args_preview,
                started_at,
                diffs,
            } => self.on_tool_updated(
                tool_call_id,
                title.as_deref(),
                args_preview.as_deref(),
                *started_at,
                diffs.as_deref(),
            ),
            Event::ElicitationRequested { elicitation } => {
                let Some(tool_call_id) = elicitation.tool_call_id.as_ref() else {
                    return Vec::new();
                };
                self.elicitation_tool_ids.insert(tool_call_id.clone());
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
                let uncertain = matches!(
                    (metadata.prompt_observation, metadata.output_uncertainty),
                    (Some(crate::acp::protocol::AcpPromptObservation::Written { window_id }), Some(uncertain))
                        if window_id == uncertain
                );
                if original_result.is_some() && uncertain {
                    vec![self.notice(
                        seq,
                        "Output attribution is uncertain for this prompt.".into(),
                    )]
                } else {
                    Vec::new()
                }
            }
            Event::AgentStartupError { message, .. } => {
                vec![self.notice(seq, format!("agent startup failed: {message}"))]
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
            Event::IncompatibleAgent { .. } => Vec::new(),
            Event::AgentSwitched { from, to, reason } => {
                if current {
                    self.active_operation = None;
                }
                // Clients render the handoff with the session_cleared divider.
                vec![self.push(
                    format!("agent-switched-{seq}"),
                    TranscriptRowKind::SessionCleared,
                    format!("Switched structured view agent from {from} to {to} ({reason})."),
                )]
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
                if current {
                    self.active_operation = metadata
                        .operation
                        .map(|operation| operation.correlation.operation_id);
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn on_tool_started(&mut self, tool_call: &ToolCall) -> Vec<TranscriptDelta> {
        if self.elicitation_tool_ids.contains(&tool_call.id) {
            return Vec::new();
        }
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
        started_at: Option<DateTime<Utc>>,
        diffs: Option<&[DiffPreview]>,
    ) -> Vec<TranscriptDelta> {
        // A non-empty diff list replaces wholesale; none or empty keeps earlier diffs.
        let diffs = diffs.filter(|d| !d.is_empty());
        let synthesized = self.ensure_tool_start(
            tool_call_id,
            title,
            args_preview,
            started_at.unwrap_or_else(Utc::now),
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
        started_at: DateTime<Utc>,
        diffs: Option<&[DiffPreview]>,
    ) -> Vec<TranscriptDelta> {
        if self.find_tool_start(tool_call_id).is_some() {
            return Vec::new();
        }
        let tool = ToolCall {
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
        let buffered = self
            .find_tool_start(tool_call_id)
            .and_then(|index| {
                self.rows[index]
                    .output
                    .iter()
                    .find_map(|block| match block {
                        ToolOutputBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
            })
            .unwrap_or_default();
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
            at: Utc::now(),
            text,
            tool_call_id: None,
            tool: None,
            output: Vec::new(),
            attachments: Vec::new(),
            origin_plugin: None,
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
    use chrono::TimeZone;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).single().expect("valid ts")
    }

    fn tool(id: &str, name: &str) -> ToolCall {
        ToolCall {
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
                Event::AgentStartupError { message: "node missing".into(), native_subject: None },
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
                    text: "hi".into(),
                    attachments: Vec::new(),
                    prompt_id: prompt_id.map(Into::into),
                    synthesized: false,
                    origin_plugin: None,
                },
                &crate::acp::event_sender::EventMetadata::history(),
            );
            assert_eq!(m.rows()[0].id, want, "prompt_id={prompt_id:?}");
        }

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
            text: "run the nightly task".into(),
            attachments: Vec::new(),
            prompt_id: None,
            synthesized: true,
            origin_plugin: None,
        };
        let mut m = TranscriptModel::new();
        let deltas =
            m.apply_scoped_event(1, &ev, &crate::acp::event_sender::EventMetadata::history());
        assert!(deltas.is_empty());
        assert!(m.rows().is_empty());

        // A plugin-sent message keeps its sender on the row.
        let ev = Event::UserPromptSent {
            text: "PR #7 was merged".into(),
            attachments: Vec::new(),
            prompt_id: None,
            synthesized: false,
            origin_plugin: Some("cron".into()),
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
            for event in &prior {
                m.observe_prior(event);
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
                    m.observe_prior(e);
                } else {
                    m.apply_scoped_event(
                        i as u64 + 1,
                        e,
                        &crate::acp::event_sender::EventMetadata::history(),
                    );
                }
            }
            assert_eq!(kinds(&m), kinds(&full), "split at {split}");
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
}
