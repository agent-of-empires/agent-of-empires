use std::{convert::Infallible, fmt};

use chrono::{DateTime, Utc};

use super::event::View;
use super::storage::{
    Backend, Changes, DeltaOf, InputTool, Memory, Outputs, Part, Positions, RowOf, Rows, Set,
    ToolOf, Tools,
};
use super::{TranscriptRow, TranscriptRowKind, UnfinishedToolOutput, LOAD_FAILED_PREFIX};
use crate::acp::state::{PayloadCollection, ToolCall};

#[derive(Debug, Clone)]
pub(crate) struct Buffer<Text> {
    pub first_seq: Option<u64>,
    pub text: Text,
}
impl<Text> Buffer<Text> {
    pub fn empty(text: Text) -> Self {
        Self {
            first_seq: None,
            text,
        }
    }
}
#[derive(Debug, Clone)]
pub(crate) enum ToolLifecycle<Text> {
    Buffered(Buffer<Text>),
    Open(Buffer<Text>),
    Terminal(Buffer<Text>),
}
impl<Text> ToolLifecycle<Text> {
    pub fn finish(&mut self, empty: Text) -> Buffer<Text> {
        match std::mem::replace(self, Self::Terminal(Buffer::empty(empty))) {
            Self::Buffered(output) | Self::Open(output) | Self::Terminal(output) => output,
        }
    }
}

pub(crate) struct Projection<B: Backend> {
    pub unfinished: B::Outputs,
}

pub(crate) struct Core<B: Backend> {
    pub(super) backend: B,
    pub(super) rows: B::Rows,
    row_ids: B::Set,
    tool_starts: B::Positions,
    tools: B::Tools,
    retained_orphans: B::Outputs,
    elicitation_tool_ids: B::Set,
    open_message_group: Option<B::Text>,
    last_message_id: Option<B::Text>,
    open_thought_group: Option<B::Text>,
    last_thought_id: Option<B::Text>,
    thought_start_pending: bool,
    group_counter: u64,
    pub(super) last_seq: u64,
    turn_active: bool,
    turn_has_output: bool,
    steering: bool,
}

impl<B: Backend> Core<B> {
    pub fn new(backend: B) -> Result<Self, B::Error> {
        Ok(Self {
            rows: backend.rows()?,
            row_ids: backend.set()?,
            tool_starts: backend.positions()?,
            tools: backend.tools()?,
            retained_orphans: backend.outputs()?,
            elicitation_tool_ids: backend.set()?,
            open_message_group: None,
            last_message_id: None,
            open_thought_group: None,
            last_thought_id: None,
            thought_start_pending: false,
            group_counter: 0,
            last_seq: 0,
            turn_active: false,
            turn_has_output: false,
            steering: false,
            backend,
        })
    }

    pub fn into_projection(self) -> Result<Projection<B>, B::Error> {
        let mut unfinished = self.retained_orphans;
        for entry in self.tools.into_iter() {
            let (id, lifecycle) = entry?;
            if self
                .elicitation_tool_ids
                .contains(self.backend.borrow(&id))?
            {
                continue;
            }
            let (ToolLifecycle::Buffered(output)
            | ToolLifecycle::Open(output)
            | ToolLifecycle::Terminal(output)) = lifecycle;
            if let Some(first_seq) = output.first_seq {
                if self.backend.text_len(&output.text) != 0 {
                    unfinished.push(UnfinishedToolOutput {
                        tool_call_id: id,
                        text: output.text,
                        first_seq,
                    })?;
                }
            }
        }
        Ok(Projection { unfinished })
    }

    pub fn observe_prior<'a>(&mut self, seq: u64, event: View<'a, B>) -> Result<(), B::Error> {
        if seq <= self.last_seq {
            return Ok(());
        }
        self.last_seq = seq;
        match event {
            View::ToolCallStarted { tool_call } => {
                if !self.elicitation_tool_ids.contains(tool_call.id)? {
                    self.track_tool_start(tool_call.id, true)?;
                }
            }
            View::ToolCallUpdated { tool_call_id, .. } => {
                if !self.elicitation_tool_ids.contains(tool_call_id)? {
                    self.track_tool_start(tool_call_id, false)?;
                }
            }
            View::ToolCallContent {
                tool_call_id,
                content,
            } => self.buffer_tool_output(tool_call_id, content)?,
            View::ElicitationRequested {
                tool_call_id: Some(id),
            } => {
                self.elicitation_tool_ids.insert(self.backend.retain(id)?)?;
                let backend = &self.backend;
                self.retained_orphans
                    .retain(|output| Ok(!backend.equal(&output.tool_call_id, id)?))?;
                self.finish_tool(id)?;
            }
            View::ToolCallCompleted { tool_call_id, .. } => {
                self.finish_tool(tool_call_id)?;
                self.elicitation_tool_ids.remove(tool_call_id)?;
            }
            View::AgentMessageChunk {
                message_id: Some(id),
                ..
            } => self.last_message_id = Some(self.backend.retain(id)?),
            View::AgentThoughtChunk {
                message_id: Some(id),
                ..
            } => self.last_thought_id = Some(self.backend.retain(id)?),
            View::UserPromptSent { .. } | View::UserDiffCommentsPrompt { .. } => {
                self.last_message_id = None;
                self.last_thought_id = None;
            }
            View::Stopped
            | View::AgentStartupError { .. }
            | View::IncompatibleAgent
            | View::AgentSwitched { .. } => {
                self.sweep_open_tools(seq, false)?;
            }
            _ => {}
        }
        Ok(())
    }

    pub fn apply<'a>(&mut self, seq: u64, event: View<'a, B>) -> Result<B::Changes, B::Error> {
        if seq <= self.last_seq {
            return Ok(self.backend.changes());
        }
        self.last_seq = seq;
        if !matches!(&event, View::AgentMessageChunk { .. }) {
            self.open_message_group = None;
        }
        if !matches!(&event, View::AgentThoughtChunk { .. }) {
            self.open_thought_group = None;
        }
        match event {
            View::AgentMessageChunk { text, message_id } => {
                let message_start = match (message_id, &self.last_message_id) {
                    (Some(id), Some(previous)) => !self.backend.equal(previous, id)?,
                    _ => false,
                };
                if let Some(id) = message_id {
                    self.last_message_id = Some(self.backend.retain(id)?);
                }
                if message_start {
                    self.open_message_group = None;
                }
                let group = self.message_group()?;
                self.turn_has_output = true;
                let id = self
                    .backend
                    .text(&[Part::Literal("msg-"), Part::Number(seq)])?;
                let mut row = self.row(
                    id,
                    group,
                    TranscriptRowKind::Message,
                    self.backend.retain(text)?,
                );
                row.message_start = message_start;
                self.append_one(row)
            }
            View::AgentThoughtChunk { text, message_id } => {
                self.turn_has_output = true;
                if let (Some(id), Some(previous)) = (message_id, &self.last_thought_id) {
                    if !self.backend.equal(previous, id)? {
                        self.open_thought_group = None;
                        self.thought_start_pending = true;
                    }
                }
                if let Some(id) = message_id {
                    self.last_thought_id = Some(self.backend.retain(id)?);
                }
                if self.backend.trim_empty(text)? && self.open_thought_group.is_none() {
                    return Ok(self.backend.changes());
                }
                let group = match &self.open_thought_group {
                    Some(group) => group.clone(),
                    None => {
                        let group = self.fresh_group()?;
                        self.open_thought_group = Some(group.clone());
                        group
                    }
                };
                let id = self
                    .backend
                    .text(&[Part::Literal("thought-"), Part::Number(seq)])?;
                let mut row = self.row(
                    id,
                    group,
                    TranscriptRowKind::Thinking,
                    self.backend.retain(text)?,
                );
                row.message_start = std::mem::take(&mut self.thought_start_pending);
                self.append_one(row)
            }
            View::UserPromptSent {
                text,
                attachments,
                prompt_id,
                merged_prompt_ids,
                synthesized,
                origin_plugin,
            } => {
                self.begin_turn();
                if synthesized {
                    return Ok(self.backend.changes());
                }
                let id = match prompt_id {
                    Some(id) if self.backend.input_len(id) != 0 => self.backend.retain(id)?,
                    _ => self
                        .backend
                        .text(&[Part::Literal("user-seq-"), Part::Number(seq)])?,
                };
                let mut row = self.grouped_row(
                    id,
                    TranscriptRowKind::UserPrompt,
                    self.backend.retain(text)?,
                )?;
                row.attachments = self.backend.retain_attachments(attachments)?;
                row.origin_plugin = origin_plugin
                    .map(|value| self.backend.retain(value))
                    .transpose()?;
                row.merged_prompt_ids = self.backend.retain_prompt_ids(merged_prompt_ids)?;
                let existing = if self.row_ids.contains(self.backend.borrow(&row.id))? {
                    self.find_row(|existing| {
                        self.backend
                            .equal(&existing.id, self.backend.borrow(&row.id))
                    })?
                } else {
                    None
                };
                match existing {
                    Some(index) => {
                        let backend = &self.backend;
                        let tool_starts = &mut self.tool_starts;
                        let delta = self.rows.update(index, |existing| {
                            if existing.kind == TranscriptRowKind::ToolStart {
                                if let Some(id) = existing.tool_call_id.as_ref() {
                                    tool_starts.remove(backend.borrow(id))?;
                                }
                            }
                            row.group_id = existing.group_id.clone();
                            *existing = row;
                            Ok(patch::<B>(existing))
                        })?;
                        self.one(delta)
                    }
                    None => self.append_one(row),
                }
            }
            View::UserDiffCommentsPrompt {
                intro,
                outro,
                is_multi_repo,
                comments,
                assembled_markdown,
            } => {
                self.begin_turn();
                let id = self
                    .backend
                    .text(&[Part::Literal("user-seq-"), Part::Number(seq)])?;
                let mut row = self.grouped_row(
                    id,
                    TranscriptRowKind::UserDiffComments,
                    self.backend.retain(assembled_markdown)?,
                )?;
                row.diff_comments =
                    Some(
                        self.backend
                            .comments(intro, outro, is_multi_repo, comments)?,
                    );
                self.append_one(row)
            }
            View::ToolCallStarted { tool_call } => self.on_tool_started(tool_call),
            View::ToolCallCompleted {
                tool_call_id,
                is_error,
                content,
                output,
                completed_at,
                async_subagent,
            } => {
                if self.elicitation_tool_ids.remove(tool_call_id)? {
                    self.finish_tool(tool_call_id)?;
                    return Ok(self.backend.changes());
                }
                let known = self.tools.get(tool_call_id)?.is_some_and(|(_, state)| {
                    matches!(&*state, ToolLifecycle::Open(_) | ToolLifecycle::Terminal(_))
                });
                let mut changes = if known {
                    self.backend.changes()
                } else {
                    self.ensure_tool_start(tool_call_id, None, None, None, completed_at, None)?
                };
                self.turn_has_output = true;
                let mut row = self.completion_row(seq, tool_call_id, is_error, content)?;
                row.at = completed_at;
                row.output = self.backend.retain_output(output)?;
                row.async_subagent = async_subagent;
                changes.push(self.append(row)?)?;
                Ok(changes)
            }
            View::ToolCallContent {
                tool_call_id,
                content,
            } => {
                self.buffer_tool_output(tool_call_id, content)?;
                Ok(self.backend.changes())
            }
            View::ToolCallUpdated {
                tool_call_id,
                title,
                args_preview,
                prompt,
                started_at,
                diffs,
            } => self.on_tool_updated(tool_call_id, title, args_preview, prompt, started_at, diffs),
            View::ElicitationRequested { tool_call_id } => {
                let Some(id) = tool_call_id else {
                    return Ok(self.backend.changes());
                };
                self.elicitation_tool_ids.insert(self.backend.retain(id)?)?;
                let backend = &self.backend;
                self.retained_orphans
                    .retain(|output| Ok(!backend.equal(&output.tool_call_id, id)?))?;
                self.finish_tool(id)?;
                self.remove_rows_for_tool(id)
            }
            View::ElicitationResolved { nonce, answers } => {
                let id = self
                    .backend
                    .text(&[Part::Literal("elicitation-"), Part::Text(nonce)])?;
                if answers.is_empty() || self.row_ids.contains(self.backend.borrow(&id))? {
                    return Ok(self.backend.changes());
                }
                let text = self.backend.answers_text(answers)?;
                let mut row = self.grouped_row(id, TranscriptRowKind::ElicitationAnswered, text)?;
                row.elicitation_answers = self.backend.retain_answers(answers)?;
                self.append_one(row)
            }
            View::Stopped => {
                let empty = self.turn_active && !self.turn_has_output;
                let mut changes = self.sweep_open_tools(seq, true)?;
                if empty {
                    let id = self
                        .backend
                        .text(&[Part::Literal("empty-"), Part::Number(seq)])?;
                    let text = self
                        .backend
                        .text(&[Part::Literal("Command produced no output.")])?;
                    changes.push(self.push(id, TranscriptRowKind::EmptyOutput, text)?)?;
                }
                self.turn_active = false;
                Ok(changes)
            }
            View::AgentStartupError { message } => {
                let mut changes = self.sweep_open_tools(seq, true)?;
                self.turn_active = false;
                self.turn_has_output = true;
                let text = self
                    .backend
                    .text(&[Part::Literal("agent startup failed: "), Part::Text(message)])?;
                changes.push(self.notice(seq, text)?)?;
                Ok(changes)
            }
            View::IncompatibleAgent => {
                let changes = self.sweep_open_tools(seq, true)?;
                self.turn_active = false;
                Ok(changes)
            }
            View::AgentSwitched { from, to, reason } => {
                let mut changes = self.sweep_open_tools(seq, true)?;
                let id = self
                    .backend
                    .text(&[Part::Literal("agent-switched-"), Part::Number(seq)])?;
                let text = self.backend.text(&[
                    Part::Literal("Switched structured view agent from "),
                    Part::Text(from),
                    Part::Literal(" to "),
                    Part::Text(to),
                    Part::Literal(" ("),
                    Part::Text(reason),
                    Part::Literal(")."),
                ])?;
                changes.push(self.push(id, TranscriptRowKind::SessionCleared, text)?)?;
                Ok(changes)
            }
            View::SessionCleared => {
                let id = self
                    .backend
                    .text(&[Part::Literal("cleared-"), Part::Number(seq)])?;
                let text = self.backend.text(&[Part::Literal(
                    "Conversation cleared, the model no longer remembers earlier turns.",
                )])?;
                self.push_one(id, TranscriptRowKind::SessionCleared, text)
            }
            View::ConversationCompacted => {
                self.turn_has_output = true;
                let id = self
                    .backend
                    .text(&[Part::Literal("compacted-"), Part::Number(seq)])?;
                let text = self.backend.text(&[Part::Literal("Conversation compacted; earlier turns above are summarised in the model's context.")])?;
                self.push_one(id, TranscriptRowKind::Compacted, text)
            }
            View::ConversationCompactionSummary {
                compaction_id,
                text,
            } => {
                let id = self.backend.text(&[
                    Part::Literal("compaction-summary-"),
                    Part::Text(compaction_id),
                ])?;
                match self.find_row(|row| self.backend.equal(&row.id, self.backend.borrow(&id)))? {
                    Some(index) => {
                        let backend = &self.backend;
                        let delta = self.rows.update(index, |row| {
                            backend.replace(&mut row.text, text)?;
                            Ok(patch::<B>(row))
                        })?;
                        self.one(delta)
                    }
                    None => self.push_one(
                        id,
                        TranscriptRowKind::CompactionSummary,
                        self.backend.retain(text)?,
                    ),
                }
            }
            View::SessionContextReset { reason } => {
                let prior = self
                    .find_row(|row| {
                        Ok(matches!(
                            row.kind,
                            TranscriptRowKind::UserPrompt | TranscriptRowKind::UserDiffComments
                        ))
                    })?
                    .is_some();
                if !prior
                    && (self.backend.input_len(reason) == 0
                        || self.backend.starts_with(reason, LOAD_FAILED_PREFIX)?)
                {
                    return Ok(self.backend.changes());
                }
                let text = if self.backend.input_len(reason) == 0 {
                    self.backend.text(&[Part::Literal(
                        "Conversation context reset; agent transcript was unavailable.",
                    )])?
                } else {
                    self.backend.retain(reason)?
                };
                self.turn_has_output = true;
                let id = self
                    .backend
                    .text(&[Part::Literal("reset-"), Part::Number(seq)])?;
                self.push_one(id, TranscriptRowKind::ContextReset, text)
            }
            View::PromptRuntimeError { message } => {
                self.turn_has_output = true;
                let text = self
                    .backend
                    .text(&[Part::Literal("prompt failed: "), Part::Text(message)])?;
                let delta = self.notice(seq, text)?;
                self.one(delta)
            }
            View::ModeSwitchFailed { mode_id, reason } => {
                let text = self.backend.text(&[
                    Part::Literal("mode switch to \""),
                    Part::Text(mode_id),
                    Part::Literal("\" failed: "),
                    Part::Text(reason),
                ])?;
                let delta = self.notice(seq, text)?;
                self.one(delta)
            }
            View::SessionNotice {
                severity,
                title,
                description,
                key,
            } => {
                self.turn_has_output = true;
                let id = match key {
                    Some(key) => self
                        .backend
                        .text(&[Part::Literal("notice-"), Part::Text(key)])?,
                    None => self
                        .backend
                        .text(&[Part::Literal("notice-"), Part::Number(seq)])?,
                };
                let description = description
                    .map(|value| self.backend.trim(value))
                    .transpose()?
                    .filter(|value| self.backend.input_len(*value) != 0);
                let text = match description {
                    Some(description) => self.backend.text(&[
                        Part::Text(severity),
                        Part::Literal(": "),
                        Part::Text(title),
                        Part::Literal(": "),
                        Part::Text(description),
                    ])?,
                    None => self.backend.text(&[
                        Part::Text(severity),
                        Part::Literal(": "),
                        Part::Text(title),
                    ])?,
                };
                match self.find_row(|row| self.backend.equal(&row.id, self.backend.borrow(&id)))? {
                    Some(index) => {
                        let delta = self.rows.update(index, |row| {
                            row.text = text;
                            Ok(patch::<B>(row))
                        })?;
                        self.one(delta)
                    }
                    None => self.push_one(id, TranscriptRowKind::Advisory, text),
                }
            }
            View::RateLimitAutoResumed { resets_at, manual } => {
                let how = if manual { "resumed" } else { "auto-resumed" };
                let date = resets_at.to_string();
                let text = self.backend.text(&[
                    Part::Literal(how),
                    Part::Literal(" at "),
                    Part::Literal(&date),
                    Part::Literal(" after rate-limit park"),
                ])?;
                let delta = self.notice(seq, text)?;
                self.one(delta)
            }
            View::ConversationSummary { text } => {
                let id = self
                    .backend
                    .text(&[Part::Literal("summary-"), Part::Number(seq)])?;
                self.push_one(id, TranscriptRowKind::Summary, self.backend.retain(text)?)
            }
            View::ThinkingStarted => {
                self.turn_active = true;
                self.turn_has_output = true;
                Ok(self.backend.changes())
            }
            View::PromptCapabilities { steering } => {
                self.steering = steering;
                Ok(self.backend.changes())
            }
            View::Other => Ok(self.backend.changes()),
        }
    }

    fn track_tool_start<'a>(
        &mut self,
        id: B::Input<'a>,
        reopen_terminal: bool,
    ) -> Result<bool, B::Error> {
        let current = self.tools.get(id)?.map(|(cursor, state)| {
            (
                cursor,
                match &*state {
                    ToolLifecycle::Buffered(_) => 1,
                    ToolLifecycle::Terminal(_) if reopen_terminal => 2,
                    _ => 0,
                },
            )
        });
        let reopened = matches!(&current, Some((_, 2)));
        match current {
            Some((cursor, 1)) => {
                let empty = self.backend.empty_text();
                self.tools.update(cursor, |state| {
                    let output = state.finish(empty);
                    *state = ToolLifecycle::Open(output);
                    Ok(())
                })?;
                self.tools.promote(cursor)?;
            }
            Some((cursor, 2)) => {
                let empty = self.backend.empty_text();
                self.tools.update(cursor, |state| {
                    *state = ToolLifecycle::Open(Buffer::empty(empty));
                    Ok(())
                })?;
                self.tools.promote(cursor)?;
            }
            Some(_) => {}
            None => self.tools.insert(
                self.backend.retain(id)?,
                ToolLifecycle::Open(Buffer::empty(self.backend.empty_text())),
            )?,
        }
        Ok(reopened)
    }

    fn buffer_tool_output<'a>(
        &mut self,
        id: B::Input<'a>,
        content: B::Input<'a>,
    ) -> Result<(), B::Error> {
        if self.elicitation_tool_ids.contains(id)? {
            return Ok(());
        }
        let cursor = self.tools.get(id)?.map(|(cursor, _)| cursor);
        match cursor {
            Some(cursor) => {
                let backend = &self.backend;
                let seq = self.last_seq;
                self.tools.update(cursor, |state| {
                    let (ToolLifecycle::Buffered(output)
                    | ToolLifecycle::Open(output)
                    | ToolLifecycle::Terminal(output)) = state;
                    output.first_seq.get_or_insert(seq);
                    backend.replace(&mut output.text, content)
                })?;
            }
            None => self.tools.insert(
                self.backend.retain(id)?,
                ToolLifecycle::Buffered(Buffer {
                    first_seq: Some(self.last_seq),
                    text: self.backend.retain(content)?,
                }),
            )?,
        }
        Ok(())
    }

    fn finish_tool<'a>(&mut self, id: B::Input<'a>) -> Result<B::Text, B::Error> {
        let cursor = self.tools.get(id)?.map(|(cursor, _)| cursor);
        let empty = self.backend.empty_text();
        match cursor {
            Some(cursor) => self
                .tools
                .update(cursor, |state| Ok(state.finish(empty).text)),
            None => {
                self.tools.insert(
                    self.backend.retain(id)?,
                    ToolLifecycle::Terminal(Buffer::empty(empty)),
                )?;
                Ok(self.backend.empty_text())
            }
        }
    }

    fn on_tool_started<'a>(&mut self, tool: InputTool<'a, B>) -> Result<B::Changes, B::Error> {
        if self.elicitation_tool_ids.contains(tool.id)? {
            return Ok(self.backend.changes());
        }
        let reopened = self.track_tool_start(tool.id, true)?;
        self.turn_has_output = true;
        let Some(index) = self.find_tool_start(tool.id)? else {
            let tool = self.backend.retain_tool(tool)?;
            let row = self.tool_start_row(tool)?;
            return self.append_one(row);
        };
        let backend = &self.backend;
        let delta = self.rows.update(index, |row| {
            let merged = match row.tool.as_ref() {
                Some(previous) if !reopened => backend.merge_tool(previous, tool)?,
                _ => backend.retain_tool(tool)?,
            };
            row.text = merged.name.clone();
            row.at = merged.started_at;
            row.tool = Some(merged);
            Ok(patch::<B>(row))
        })?;
        self.one(delta)
    }

    fn on_tool_updated<'a>(
        &mut self,
        id: B::Input<'a>,
        title: Option<B::Input<'a>>,
        args: Option<B::Input<'a>>,
        prompt: Option<B::Input<'a>>,
        started_at: Option<DateTime<Utc>>,
        diffs: Option<B::DiffsInput<'a>>,
    ) -> Result<B::Changes, B::Error> {
        let terminal = self
            .tools
            .get(id)?
            .is_some_and(|(_, state)| matches!(&*state, ToolLifecycle::Terminal(_)));
        if self.elicitation_tool_ids.contains(id)?
            || (terminal && self.find_tool_start(id)?.is_none())
        {
            return Ok(self.backend.changes());
        }
        let diffs = diffs.filter(|value| !value.is_empty());
        let synthesized = self.ensure_tool_start(
            id,
            title,
            args,
            prompt,
            started_at.unwrap_or_else(Utc::now),
            diffs,
        )?;
        if !synthesized.is_empty() {
            return Ok(synthesized);
        }
        let index = self.find_tool_start(id)?.expect("tool start exists");
        let backend = &self.backend;
        let delta = self.rows.update(index, |row| {
            if let Some(title) = title {
                backend.replace(&mut row.text, title)?;
            }
            if let Some(tool) = row.tool.as_mut() {
                if let Some(title) = title {
                    backend.replace(&mut tool.name, title)?;
                }
                if let Some(args) = args {
                    if prompt.is_some()
                        || !backend.ends_with(args, crate::acp::state::ARGS_CUT_MARKER)?
                    {
                        match prompt {
                            Some(value) => match tool.prompt.as_mut() {
                                Some(stored) => backend.replace(stored, value)?,
                                None => tool.prompt = Some(backend.retain(value)?),
                            },
                            None => tool.prompt = None,
                        }
                    }
                    backend.replace(&mut tool.args_preview, args)?;
                }
                if let Some(started_at) = started_at {
                    tool.started_at = started_at;
                }
                if let Some(diffs) = diffs {
                    tool.diffs = backend.retain_diffs(diffs)?;
                }
            }
            Ok(patch::<B>(row))
        })?;
        self.one(delta)
    }

    fn ensure_tool_start<'a>(
        &mut self,
        id: B::Input<'a>,
        name: Option<B::Input<'a>>,
        args: Option<B::Input<'a>>,
        prompt: Option<B::Input<'a>>,
        started_at: DateTime<Utc>,
        diffs: Option<B::DiffsInput<'a>>,
    ) -> Result<B::Changes, B::Error> {
        if self.find_tool_start(id)?.is_some() {
            return Ok(self.backend.changes());
        }
        self.track_tool_start(id, false)?;
        self.turn_has_output = true;
        let tool = ToolCall {
            id: self.backend.retain(id)?,
            name: match name.filter(|name| self.backend.input_len(*name) != 0) {
                Some(name) => self.backend.retain(name)?,
                None => self.backend.text(&[Part::Literal("tool call")])?,
            },
            kind: self.backend.text(&[Part::Literal("other")])?,
            args_preview: match args {
                Some(args) => self.backend.retain(args)?,
                None => self.backend.empty_text(),
            },
            prompt: prompt.map(|value| self.backend.retain(value)).transpose()?,
            started_at,
            parent_tool_call_id: None,
            memory_recall: None,
            diffs: diffs
                .map(|value| self.backend.retain_diffs(value))
                .transpose()?
                .unwrap_or_default(),
        };
        let row = self.tool_start_row(tool)?;
        self.append_one(row)
    }

    fn completion_row<'a>(
        &mut self,
        seq: u64,
        id: B::Input<'a>,
        is_error: bool,
        content: B::Input<'a>,
    ) -> Result<RowOf<B>, B::Error> {
        let buffered = self.finish_tool(id)?;
        let text = if self.backend.input_len(content) != 0 {
            self.backend.retain(content)?
        } else if self.backend.text_len(&buffered) != 0 {
            buffered
        } else {
            self.backend.text(&[Part::Literal(if is_error {
                "tool failed"
            } else {
                "completed"
            })])?
        };
        let base = self
            .backend
            .text(&[Part::Literal("done-"), Part::Text(id)])?;
        let row_id = if self.row_ids.contains(self.backend.borrow(&base))? {
            self.backend.text(&[
                Part::Text(self.backend.borrow(&base)),
                Part::Literal("-"),
                Part::Number(seq),
            ])?
        } else {
            base
        };
        let group = self
            .backend
            .text(&[Part::Literal("tool-"), Part::Text(id)])?;
        let mut row = self.row(
            row_id,
            group,
            if is_error {
                TranscriptRowKind::ToolError
            } else {
                TranscriptRowKind::ToolComplete
            },
            text,
        );
        row.tool_call_id = Some(self.backend.retain(id)?);
        Ok(row)
    }

    fn sweep_open_tools(&mut self, seq: u64, emit_rows: bool) -> Result<B::Changes, B::Error> {
        let now = emit_rows.then(Utc::now);
        let mut changes = self.backend.changes();
        let mut after = None;
        loop {
            let next = self.tools.next(after)?.map(|(cursor, id, state)| {
                let (kind, output) = match &*state {
                    ToolLifecycle::Buffered(output) => (1, output),
                    ToolLifecycle::Open(output) => (2, output),
                    ToolLifecycle::Terminal(output) => (0, output),
                };
                let need_id = (kind == 2 && emit_rows)
                    || (kind == 1
                        && output.first_seq.is_some()
                        && self.backend.text_len(&output.text) != 0);
                (cursor, kind, need_id.then(|| (*id).clone()))
            });
            let Some((cursor, kind, id)) = next else {
                break;
            };
            after = Some(cursor);
            if kind == 0 {
                continue;
            }
            let empty = self.backend.empty_text();
            let output = self.tools.update(cursor, |state| Ok(state.finish(empty)))?;
            if kind == 1 {
                if let (Some(id), Some(first_seq)) = (id, output.first_seq) {
                    self.retained_orphans.push(UnfinishedToolOutput {
                        tool_call_id: id,
                        text: output.text,
                        first_seq,
                    })?;
                }
            } else if emit_rows {
                let id = id.expect("open tool id");
                let row_id = self.backend.text(&[
                    Part::Literal("stopped-"),
                    Part::Text(self.backend.borrow(&id)),
                    Part::Literal("-"),
                    Part::Number(seq),
                ])?;
                let group = self
                    .backend
                    .text(&[Part::Literal("tool-"), Part::Text(self.backend.borrow(&id))])?;
                let mut row = self.row(row_id, group, TranscriptRowKind::ToolStopped, output.text);
                row.at = now.expect("sweep timestamp");
                row.tool_call_id = Some(id);
                changes.push(self.append(row)?)?;
            }
        }
        Ok(changes)
    }

    fn remove_rows_for_tool<'a>(&mut self, id: B::Input<'a>) -> Result<B::Changes, B::Error> {
        let mut changes = self.backend.changes();
        let backend = &self.backend;
        self.rows.retain(|row| {
            let remove = match row.tool_call_id.as_ref() {
                Some(value) => backend.equal(value, id)?,
                None => false,
            };
            if remove {
                changes.push(super::TranscriptDelta::Remove(row.id.clone()))?;
            }
            Ok(!remove)
        })?;
        if !changes.is_empty() {
            self.row_ids.clear()?;
            self.tool_starts.clear()?;
            for index in 0..self.rows.len() {
                let row = self.rows.read(index)?;
                self.row_ids.insert(row.id.clone())?;
                if row.kind == TranscriptRowKind::ToolStart {
                    if let Some(id) = row.tool_call_id.as_ref() {
                        self.tool_starts.insert(id.clone(), index)?;
                    }
                }
            }
        }
        Ok(changes)
    }

    fn row(
        &self,
        id: B::Text,
        group_id: B::Text,
        kind: TranscriptRowKind,
        text: B::Text,
    ) -> RowOf<B> {
        TranscriptRow {
            id,
            group_id,
            kind,
            at: Utc::now(),
            text,
            tool_call_id: None,
            tool: None,
            output: B::Output::default(),
            attachments: B::Attachments::default(),
            origin_plugin: None,
            merged_prompt_ids: B::PromptIds::default(),
            diff_comments: None,
            elicitation_answers: B::Answers::default(),
            async_subagent: false,
            message_start: false,
        }
    }
    fn tool_start_row(&self, tool: ToolOf<B>) -> Result<RowOf<B>, B::Error> {
        let id = self.backend.text(&[
            Part::Literal("start-"),
            Part::Text(self.backend.borrow(&tool.id)),
        ])?;
        let group = self.backend.text(&[
            Part::Literal("tool-"),
            Part::Text(self.backend.borrow(&tool.id)),
        ])?;
        let mut row = self.row(id, group, TranscriptRowKind::ToolStart, tool.name.clone());
        row.at = tool.started_at;
        row.tool_call_id = Some(tool.id.clone());
        row.tool = Some(tool);
        Ok(row)
    }
    fn append(&mut self, row: RowOf<B>) -> Result<DeltaOf<B>, B::Error> {
        self.row_ids.insert(row.id.clone())?;
        let index = self.rows.len();
        self.rows.push(&row)?;
        if row.kind == TranscriptRowKind::ToolStart {
            if let Some(id) = row.tool_call_id.as_ref() {
                self.tool_starts.insert(id.clone(), index)?;
            }
        }
        Ok(super::TranscriptDelta::Append(row))
    }
    fn one(&self, delta: DeltaOf<B>) -> Result<B::Changes, B::Error> {
        let mut changes = self.backend.changes();
        changes.push(delta)?;
        Ok(changes)
    }
    fn append_one(&mut self, row: RowOf<B>) -> Result<B::Changes, B::Error> {
        let delta = self.append(row)?;
        self.one(delta)
    }
    fn grouped_row(
        &mut self,
        id: B::Text,
        kind: TranscriptRowKind,
        text: B::Text,
    ) -> Result<RowOf<B>, B::Error> {
        let group = self.fresh_group()?;
        Ok(self.row(id, group, kind, text))
    }
    fn push(
        &mut self,
        id: B::Text,
        kind: TranscriptRowKind,
        text: B::Text,
    ) -> Result<DeltaOf<B>, B::Error> {
        let row = self.grouped_row(id, kind, text)?;
        self.append(row)
    }
    fn push_one(
        &mut self,
        id: B::Text,
        kind: TranscriptRowKind,
        text: B::Text,
    ) -> Result<B::Changes, B::Error> {
        let delta = self.push(id, kind, text)?;
        self.one(delta)
    }
    fn notice(&mut self, seq: u64, text: B::Text) -> Result<DeltaOf<B>, B::Error> {
        let id = self
            .backend
            .text(&[Part::Literal("notice-"), Part::Number(seq)])?;
        self.push(id, TranscriptRowKind::Notice, text)
    }
    fn fresh_group(&mut self) -> Result<B::Text, B::Error> {
        self.group_counter += 1;
        self.backend
            .text(&[Part::Literal("g"), Part::Number(self.group_counter)])
    }
    fn message_group(&mut self) -> Result<B::Text, B::Error> {
        if let Some(group) = &self.open_message_group {
            return Ok(group.clone());
        }
        let group = self.fresh_group()?;
        self.open_message_group = Some(group.clone());
        Ok(group)
    }
    fn begin_turn(&mut self) {
        self.last_message_id = None;
        self.last_thought_id = None;
        self.thought_start_pending = false;
        let steered = self.turn_active && self.steering;
        self.turn_active = true;
        if !steered {
            self.turn_has_output = false;
        }
    }
    fn find_row(
        &self,
        mut predicate: impl FnMut(&RowOf<B>) -> Result<bool, B::Error>,
    ) -> Result<Option<u64>, B::Error> {
        for index in 0..self.rows.len() {
            if predicate(&*self.rows.read(index)?)? {
                return Ok(Some(index));
            }
        }
        Ok(None)
    }
    fn find_tool_start<'a>(&self, id: B::Input<'a>) -> Result<Option<u64>, B::Error> {
        self.tool_starts.get(id)
    }
}

fn patch<B: Backend>(row: &RowOf<B>) -> DeltaOf<B> {
    super::TranscriptDelta::Patch {
        id: row.id.clone(),
        row: row.clone(),
    }
}

impl Default for Core<Memory> {
    fn default() -> Self {
        match Self::new(Memory) {
            Ok(value) => value,
            Err(error) => match error {},
        }
    }
}
impl Clone for Core<Memory> {
    fn clone(&self) -> Self {
        Self {
            backend: Memory,
            rows: self.rows.clone(),
            row_ids: self.row_ids.clone(),
            tool_starts: self.tool_starts.clone(),
            tools: self.tools.clone(),
            retained_orphans: self.retained_orphans.clone(),
            elicitation_tool_ids: self.elicitation_tool_ids.clone(),
            open_message_group: self.open_message_group.clone(),
            last_message_id: self.last_message_id.clone(),
            open_thought_group: self.open_thought_group.clone(),
            last_thought_id: self.last_thought_id.clone(),
            thought_start_pending: self.thought_start_pending,
            group_counter: self.group_counter,
            last_seq: self.last_seq,
            turn_active: self.turn_active,
            turn_has_output: self.turn_has_output,
            steering: self.steering,
        }
    }
}
impl fmt::Debug for Core<Memory> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TranscriptModel")
            .field("rows", &self.rows)
            .field("last_seq", &self.last_seq)
            .finish_non_exhaustive()
    }
}

pub(super) fn infallible<T>(result: Result<T, Infallible>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => match error {},
    }
}
