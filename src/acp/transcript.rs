//! `TranscriptModel`: the server-side render model for the structured-view transcript.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::elicitations::ElicitationAnswer;
use super::state::{DiffComment, DiffPreview, Event, ToolCall, ToolOutputBlock};
use crate::daemon::PromptAttachmentRef;

/// One renderable row of the transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptRow {
    pub id: String,
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
    /// Structured payload on a `user_diff_comments` row; `text` holds the markdown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_comments: Option<DiffCommentsPayload>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub elicitation_answers: Vec<ElicitationAnswer>,
    /// A `tool_complete` row that launched an async sub-agent.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub async_subagent: bool,
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
    Summary,
    /// An error or lifecycle notice the user needs in the timeline.
    Notice,
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

/// Folds the ACP `Event` stream into an ordered [`TranscriptRow`] list.
#[derive(Debug, Clone, Default)]
pub struct TranscriptModel {
    rows: Vec<TranscriptRow>,
    row_ids: HashSet<String>,
    /// Tool calls that reached a terminal row, so nothing closes them twice.
    terminal_tools: HashSet<String>,
    /// Streamed `ToolCallContent`, keyed by tool call id.
    tool_outputs: HashMap<String, String>,
    /// Tool calls surfaced as an elicitation (AskUserQuestion); their cards are suppressed.
    elicitation_tool_ids: HashSet<String>,
    /// Group of the still-open run of consecutive message chunks.
    open_message_group: Option<String>,
    group_counter: u64,
    /// Frames at or below this seq are dropped, so replay overlap is harmless.
    last_seq: u64,
    turn_active: bool,
    /// Whether the running turn produced visible output, for the empty-output notice.
    turn_has_output: bool,
    /// Whether a mid-turn prompt steers the running turn rather than opening a new one.
    steering: bool,
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

    /// Apply one event at `seq`, returning the row changes it produced.
    pub fn apply_event(&mut self, seq: u64, event: &Event) -> Vec<TranscriptDelta> {
        if seq <= self.last_seq {
            return Vec::new();
        }
        self.last_seq = seq;
        if !matches!(event, Event::AgentMessageChunk { .. }) {
            self.open_message_group = None;
        }

        match event {
            Event::AgentMessageChunk { text } => {
                let group_id = self.message_group();
                self.turn_has_output = true;
                vec![self.append(TranscriptRow::new(
                    format!("msg-{seq}"),
                    group_id,
                    TranscriptRowKind::Message,
                    text.clone(),
                ))]
            }
            Event::UserPromptSent {
                text,
                attachments,
                prompt_id,
                synthesized,
            } => {
                self.begin_turn();
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
                self.begin_turn();
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
                if self.elicitation_tool_ids.contains(tool_call_id) {
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
                // Each content frame replaces the previous snapshot.
                self.tool_outputs
                    .insert(tool_call_id.clone(), content.clone());
                Vec::new()
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
                let empty = self.turn_active && !self.turn_has_output;
                let mut deltas = self.sweep_open_tools(seq);
                // Some slash commands produce neither a chunk nor a tool call.
                if empty {
                    deltas.push(self.push(
                        format!("empty-{seq}"),
                        TranscriptRowKind::EmptyOutput,
                        "Command produced no output.".to_string(),
                    ));
                }
                self.turn_active = false;
                deltas
            }
            // The failure itself is the turn's visible product, so no empty-output notice.
            Event::AgentStartupError { message } => {
                let mut deltas = self.sweep_open_tools(seq);
                self.turn_active = false;
                self.turn_has_output = true;
                deltas.push(self.notice(seq, format!("agent startup failed: {message}")));
                deltas
            }
            // A dedicated remediation screen renders this, so the timeline stays quiet.
            Event::IncompatibleAgent { .. } => {
                let deltas = self.sweep_open_tools(seq);
                self.turn_active = false;
                deltas
            }
            Event::AgentSwitched { from, to, reason } => {
                let mut deltas = self.sweep_open_tools(seq);
                // Clients render the handoff with the session_cleared divider.
                deltas.push(self.push(
                    format!("agent-switched-{seq}"),
                    TranscriptRowKind::SessionCleared,
                    format!("Switched structured view agent from {from} to {to} ({reason})."),
                ));
                deltas
            }
            Event::SessionCleared => vec![self.push(
                format!("cleared-{seq}"),
                TranscriptRowKind::SessionCleared,
                "Conversation cleared, the model no longer remembers earlier turns.".to_string(),
            )],
            Event::ConversationCompacted => vec![self.push(
                format!("compacted-{seq}"),
                TranscriptRowKind::Compacted,
                "Conversation compacted; earlier turns above are summarised in the model's context."
                    .to_string(),
            )],
            Event::SessionContextReset { reason } => {
                let has_prior_prompt = self.rows.iter().any(|r| {
                    matches!(
                        r.kind,
                        TranscriptRowKind::UserPrompt | TranscriptRowKind::UserDiffComments
                    )
                });
                if !has_prior_prompt
                    && (reason.is_empty() || reason.starts_with("session/load failed"))
                {
                    return Vec::new();
                }
                let text = if reason.is_empty() {
                    "Conversation context reset; agent transcript was unavailable.".to_string()
                } else {
                    reason.clone()
                };
                self.turn_has_output = true;
                vec![self.push(
                    format!("reset-{seq}"),
                    TranscriptRowKind::ContextReset,
                    text,
                )]
            }
            Event::PromptRuntimeError { message } => {
                self.turn_has_output = true;
                vec![self.notice(seq, format!("prompt failed: {message}"))]
            }
            Event::ModeSwitchFailed { mode_id, reason } => vec![self.notice(
                seq,
                format!("mode switch to \"{mode_id}\" failed: {reason}"),
            )],
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
                self.turn_active = true;
                self.turn_has_output = true;
                Vec::new()
            }
            Event::PromptCapabilities { steering, .. } => {
                self.steering = *steering;
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
            self.turn_has_output = true;
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
        self.turn_has_output = true;
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
        let buffered = self.tool_outputs.remove(tool_call_id).unwrap_or_default();
        let text = if !content.is_empty() {
            content.to_string()
        } else if !buffered.is_empty() {
            buffered
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
        self.terminal_tools.insert(tool_call_id.to_string());
        let kind = if is_error {
            TranscriptRowKind::ToolError
        } else {
            TranscriptRowKind::ToolComplete
        };
        let mut row = TranscriptRow::new(row_id, format!("tool-{tool_call_id}"), kind, text);
        row.tool_call_id = Some(tool_call_id.to_string());
        row
    }

    /// Close every `tool_start` without a terminal row with a `tool_stopped`
    /// carrying any buffered output.
    fn sweep_open_tools(&mut self, seq: u64) -> Vec<TranscriptDelta> {
        let mut seen = self.terminal_tools.clone();
        let open: Vec<String> = self
            .rows
            .iter()
            .filter(|row| row.kind == TranscriptRowKind::ToolStart)
            .filter_map(|row| row.tool_call_id.clone())
            .filter(|id| seen.insert(id.clone()))
            .collect();
        let now = Utc::now();
        open.into_iter()
            .map(|id| {
                self.terminal_tools.insert(id.clone());
                let buffered = self.tool_outputs.remove(&id).unwrap_or_default();
                let mut row = TranscriptRow::new(
                    format!("stopped-{id}-{seq}"),
                    format!("tool-{id}"),
                    TranscriptRowKind::ToolStopped,
                    buffered,
                );
                row.at = now;
                row.tool_call_id = Some(id);
                self.append(row)
            })
            .collect()
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

    fn append(&mut self, row: TranscriptRow) -> TranscriptDelta {
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

    fn begin_turn(&mut self) {
        let steered_continuation = self.turn_active && self.steering;
        self.turn_active = true;
        if !steered_continuation {
            self.turn_has_output = false;
        }
    }

    fn find_tool_start(&self, tool_call_id: &str) -> Option<usize> {
        self.rows.iter().position(|r| {
            r.kind == TranscriptRowKind::ToolStart
                && r.tool_call_id.as_deref() == Some(tool_call_id)
        })
    }
}

impl TranscriptRow {
    fn new(id: String, group_id: String, kind: TranscriptRowKind, text: String) -> Self {
        Self {
            id,
            group_id,
            kind,
            at: Utc::now(),
            text,
            tool_call_id: None,
            tool: None,
            output: Vec::new(),
            attachments: Vec::new(),
            diff_comments: None,
            elicitation_answers: Vec::new(),
            async_subagent: false,
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
            m.apply_event(i as u64 + 1, &e);
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
                Event::AgentStartupError {
                    message: "node missing".into(),
                },
                "notice-1",
                TranscriptRowKind::Notice,
                "agent startup failed: node missing".to_string(),
            ),
            (
                Event::PromptRuntimeError {
                    message: "stream died".into(),
                },
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
        assert_eq!(m.apply_event(1, &prompt("hi")).len(), 1);
        assert!(m.apply_event(1, &prompt("ignored")).is_empty());
        assert!(m.apply_event(0, &prompt("older")).is_empty());
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
        };
        let mut m = TranscriptModel::new();
        let deltas = m.apply_event(3, &with_attachment);
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
            m.apply_event(
                7,
                &Event::UserPromptSent {
                    text: "hi".into(),
                    attachments: Vec::new(),
                    prompt_id: prompt_id.map(Into::into),
                    synthesized: false,
                },
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

        // A synthesized resume prompt (#3028, #4040) renders no row but still
        // opens the turn.
        let ev = Event::UserPromptSent {
            text: "run the nightly task".into(),
            attachments: Vec::new(),
            prompt_id: None,
            synthesized: true,
        };
        let mut m = TranscriptModel::new();
        let deltas = m.apply_event(1, &ev);
        assert!(deltas.is_empty());
        assert!(m.rows().is_empty());
        assert!(m.turn_active);
        assert!(!m.turn_has_output);
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
                !m.tool_outputs.contains_key("t"),
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
        assert!(
            m.apply_event(2, &content("task-1", "streaming")).is_empty(),
            "buffering emits nothing"
        );
        m.apply_event(
            3,
            &Event::ToolCallCompleted {
                tool_call_id: "task-1".into(),
                is_error: false,
                content: "Async agent launched successfully".into(),
                output: Vec::new(),
                completed_at: at(200),
                async_subagent: true,
            },
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
        assert!(
            m.turn_has_output,
            "a synthesized card counts as turn output"
        );

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
        let deltas = m.apply_event(
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

        let deltas = m.apply_event(
            4,
            &updated(
                "dup",
                Some("Edit a.rs"),
                Some(r#"{"command":"git log"}"#),
                None,
            ),
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
        m.apply_event(5, &updated("dup", None, None, Some(vec![diff("b.rs")])));
        assert_eq!(
            tool_of(&m, "start-dup").diffs,
            [diff("b.rs")],
            "diffs replace wholesale"
        );
        assert_eq!(tool_of(&m, "start-dup").name, "Edit a.rs");
    }

    #[test]
    fn ask_user_question_cards_are_suppressed_in_either_order() {
        let mut m = fold([started(tool("tc-ask", "Asking"))]);
        let deltas = m.apply_event(2, &elicitation_requested("tc-ask"));
        assert!(matches!(deltas.as_slice(), [TranscriptDelta::Remove(id)] if id == "start-tc-ask"));
        assert!(m
            .apply_event(3, &completed("tc-ask", false, "x"))
            .is_empty());
        assert!(m.rows().is_empty());

        let mut m = fold([elicitation_requested("tc-ask")]);
        assert!(m
            .apply_event(2, &started(tool("tc-ask", "Asking")))
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
            m.apply_event(2, &answer("el-1", yes())).is_empty(),
            "re-broadcast deduped"
        );
        assert!(
            m.apply_event(3, &answer("el-2", Vec::new())).is_empty(),
            "no answers, no row"
        );
    }

    #[test]
    fn turn_endings_sweep_open_tools_once() {
        let m = fold([
            prompt("run"),
            started(tool("open-1", "LongTask")),
            content("open-1", "partial out"),
            stopped("prompt_complete"),
        ]);
        let swept = row(&m, "stopped-open-1-4");
        assert_eq!(
            (swept.kind, swept.text.as_str()),
            (TranscriptRowKind::ToolStopped, "partial out")
        );
        assert!(!m.tool_outputs.contains_key("open-1"));

        let mut m = fold([
            started(tool("a", "A")),
            started(tool("b", "B")),
            completed("a", false, ""),
            stopped("user_stopped"),
        ]);
        m.apply_event(10, &stopped("prompt_complete"));
        let swept: Vec<&str> = m
            .rows()
            .iter()
            .filter(|r| r.kind == TranscriptRowKind::ToolStopped)
            .filter_map(|r| r.tool_call_id.as_deref())
            .collect();
        assert_eq!(swept, ["b"], "only the open tool is swept, and only once");

        let closers = [
            stopped("cancelled"),
            Event::AgentStartupError {
                message: "boom".into(),
            },
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
        ];
        for closer in closers {
            let label = format!("{closer:?}");
            let m = fold([started(tool("t1", "T")), closer]);
            assert_eq!(count(&m, TranscriptRowKind::ToolStopped), 1, "{label}");
        }
    }

    #[test]
    fn empty_output_notice_needs_an_output_less_unsteered_turn() {
        let caps = Event::PromptCapabilities {
            image: false,
            audio: false,
            embedded_context: false,
            load_session: None,
            steering: true,
        };
        let runtime_error = Event::PromptRuntimeError {
            message: "boom".into(),
        };
        // (events between the prompt and its stop, notice expected)
        let cases = [
            (vec![], true),
            (vec![chunk("hi")], false),
            (vec![Event::ThinkingStarted], false),
            (vec![runtime_error], false),
        ];
        for (i, (mid, notice)) in cases.into_iter().enumerate() {
            let mut events = vec![prompt("/usage")];
            events.extend(mid);
            events.push(stopped("prompt_complete"));
            assert_eq!(
                count(&fold(events), TranscriptRowKind::EmptyOutput) == 1,
                notice,
                "case {i}"
            );
        }

        let m = fold([
            caps,
            prompt("read src/acp"),
            chunk("on it"),
            prompt("just acp_client.rs"),
            stopped("prompt_complete"),
        ]);
        assert_eq!(
            count(&m, TranscriptRowKind::EmptyOutput),
            0,
            "a steered prompt keeps the turn's output"
        );
        assert_eq!(count(&m, TranscriptRowKind::UserPrompt), 2);
    }
}
