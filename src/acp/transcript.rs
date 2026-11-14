//! `TranscriptModel`: the server-side render model for the structured-view transcript.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::elicitations::ElicitationAnswer;
use super::state::{
    payload_is_empty, DiffComment, Event, PayloadCollection, ToolCall, ToolOutputBlock,
};
use crate::daemon::PromptAttachmentRef;

pub(crate) mod event;
pub(crate) mod reducer;
pub(crate) mod storage;

/// Reasons with this prefix are hidden before the first prompt, since a never-prompted
/// session can legitimately fail `session/load`.
pub(crate) const LOAD_FAILED_PREFIX: &str = "session/load failed";

/// One renderable row of the transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound(
    deserialize = "Text: Deserialize<'de>, Tool: Deserialize<'de>, Output: Deserialize<'de> + Default, Attachments: Deserialize<'de> + Default, Comments: Deserialize<'de>, Answers: Deserialize<'de> + Default, PromptIds: Deserialize<'de> + Default"
))]
pub struct TranscriptRow<
    Text = String,
    Tool = ToolCall,
    Output: PayloadCollection = Vec<ToolOutputBlock>,
    Attachments: PayloadCollection = Vec<PromptAttachmentRef>,
    Comments = DiffCommentsPayload,
    Answers: PayloadCollection = Vec<ElicitationAnswer>,
    PromptIds: PayloadCollection = Vec<String>,
> {
    pub id: Text,
    pub group_id: Text,
    pub kind: TranscriptRowKind,
    pub at: DateTime<Utc>,
    pub text: Text,
    /// Set on tool-lifecycle rows so a client pairs a completion with its start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<Text>,
    /// Full tool payload on `tool_start` rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<Tool>,
    /// Structured completion payload on `tool_complete` / `tool_error` rows.
    #[serde(default, skip_serializing_if = "payload_is_empty")]
    pub output: Output,
    /// Attachment metadata on a `user_prompt` row; bytes are fetched lazily.
    #[serde(default, skip_serializing_if = "payload_is_empty")]
    pub attachments: Attachments,
    /// Plugin that sent a `user_prompt` row; absent when the user typed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin_plugin: Option<Text>,
    /// Ids of other queued rows folded into a `user_prompt` row, so a client that
    /// only sees this row (snapshot or replay) still settles their optimistic rows.
    #[serde(default, skip_serializing_if = "payload_is_empty")]
    pub merged_prompt_ids: PromptIds,
    /// Structured payload on a `user_diff_comments` row; `text` holds the markdown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_comments: Option<Comments>,
    #[serde(default, skip_serializing_if = "payload_is_empty")]
    pub elicitation_answers: Answers,
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
#[serde(bound(
    deserialize = "Text: Deserialize<'de>, TranscriptRow<Text, Tool, Output, Attachments, Comments, Answers, PromptIds>: Deserialize<'de>"
))]
pub enum TranscriptDelta<
    Text = String,
    Tool = ToolCall,
    Output: PayloadCollection = Vec<ToolOutputBlock>,
    Attachments: PayloadCollection = Vec<PromptAttachmentRef>,
    Comments = DiffCommentsPayload,
    Answers: PayloadCollection = Vec<ElicitationAnswer>,
    PromptIds: PayloadCollection = Vec<String>,
> {
    Append(TranscriptRow<Text, Tool, Output, Attachments, Comments, Answers, PromptIds>),
    Patch {
        id: Text,
        row: TranscriptRow<Text, Tool, Output, Attachments, Comments, Answers, PromptIds>,
    },
    /// A row was removed (an AskUserQuestion card superseded by its form).
    Remove(Text),
}

#[derive(Debug, Clone)]
pub(crate) struct UnfinishedToolOutput<Text = String> {
    pub tool_call_id: Text,
    pub text: Text,
    pub(crate) first_seq: u64,
}

/// Folds ACP events through the shared transcript reducer.
#[derive(Debug, Clone, Default)]
pub struct TranscriptModel {
    inner: reducer::Core<storage::Memory>,
}

impl TranscriptModel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn rows(&self) -> &[TranscriptRow] {
        &self.inner.rows
    }

    pub fn into_rows(self) -> Vec<TranscriptRow> {
        self.inner.rows
    }

    pub fn last_seq(&self) -> u64 {
        self.inner.last_seq
    }

    /// Observe prefix lifecycle without rendering rows.
    pub fn observe_prior(&mut self, seq: u64, event: &Event) {
        reducer::infallible(self.inner.observe_prior(seq, event::memory(event)));
    }

    pub fn apply_event(&mut self, seq: u64, event: &Event) -> Vec<TranscriptDelta> {
        reducer::infallible(self.inner.apply(seq, event::memory(event)))
    }
}

/// Merge repeated starts without replacing richer fields with sparse updates.
pub(crate) fn merge_tool_start(prev: &ToolCall, incoming: &ToolCall) -> ToolCall {
    use storage::Backend;
    reducer::infallible(storage::Memory.merge_tool(prev, storage::Memory.tool_input(incoming)))
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
    use std::collections::HashSet;

    use super::*;
    use crate::acp::approvals::Nonce;
    use crate::acp::elicitations::{Elicitation, ElicitationOutcome};
    use crate::acp::state::test_support::{chunk, prompt, stopped};
    use crate::acp::state::{DiffPreview, MemoryRecall};
    use crate::daemon::PromptAttachmentKind;
    use chrono::TimeZone;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).single().expect("valid ts")
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
                Event::startup_error("node missing"),
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
        assert_eq!(m.apply_event(1, &prompt("hi")).len(), 1);
        assert!(m.apply_event(1, &prompt("ignored")).is_empty());
        assert!(m.apply_event(0, &prompt("older")).is_empty());
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
                    merged_prompt_ids: Vec::new(),
                    text: "hi".into(),
                    attachments: Vec::new(),
                    prompt_id: prompt_id.map(Into::into),
                    synthesized: false,
                    origin_plugin: None,
                },
            );
            assert_eq!(m.rows()[0].id, want, "prompt_id={prompt_id:?}");
        }

        let mut m = TranscriptModel::new();
        m.apply_event(
            8,
            &Event::UserPromptSent {
                merged_prompt_ids: vec!["q2".into()],
                text: "a\n\nb".into(),
                attachments: Vec::new(),
                prompt_id: Some("q1".into()),
                synthesized: false,
                origin_plugin: None,
            },
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
        };
        let deltas = m.apply_event(9, &retry);
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

        // Resume prompts must not duplicate a visible prompt.
        let ev = Event::UserPromptSent {
            merged_prompt_ids: Vec::new(),
            text: "run the nightly task".into(),
            attachments: Vec::new(),
            prompt_id: None,
            synthesized: true,
            origin_plugin: None,
        };
        let mut m = TranscriptModel::new();
        let deltas = m.apply_event(1, &ev);
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
        };
        let mut m = TranscriptModel::new();
        m.apply_event(1, &ev);
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
        m.observe_prior(1, &thought("earlier", a));
        m.apply_event(2, &thought("later", b));
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
                m.observe_prior(index as u64 + 1, event);
            }
            m.apply_event(10, &chunk("", "m-b"));
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
                    m.observe_prior(i as u64 + 1, e);
                } else {
                    m.apply_event(i as u64 + 1, e);
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
                model.observe_prior(index as u64 + 1, event);
                assert!(model.rows().is_empty(), "{label}: prefix stays unrendered");
            }
            for (index, event) in suffix.iter().enumerate() {
                let seq = 100 + index as u64;
                model.apply_event(seq, event);
                assert!(
                    model.apply_event(seq, event).is_empty(),
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
            model.apply_event(1000, &stopped("prompt_complete"));
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
                model.observe_prior(index as u64 + 1, event);
            }
            assert!(model.rows().is_empty());
            let output = vec![ToolOutputBlock::Text {
                text: "structured output".into(),
            }];
            let deltas = model.apply_event(
                10,
                &Event::ToolCallCompleted {
                    tool_call_id: "t".into(),
                    is_error,
                    content: final_content.into(),
                    output: output.clone(),
                    completed_at: at(200),
                    async_subagent: true,
                },
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
                .apply_event(11, &stopped("prompt_complete"))
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
            model.observe_prior(seq, event);
            assert_eq!(model.last_seq(), seq, "{event:?}");
            assert!(model.rows().is_empty(), "{event:?}");
            assert!(model.apply_event(seq, event).is_empty(), "{event:?}");
        }
    }

    #[test]
    fn observed_prefix_rejects_stale_stop_before_later_tool() {
        let mut model = TranscriptModel::new();
        model.observe_prior(1, &stopped("prompt_complete"));
        model.observe_prior(2, &started(tool("later", "Later tool")));
        model.observe_prior(1, &stopped("prompt_complete"));
        assert_eq!(model.last_seq(), 2, "stale observation keeps the watermark");
        assert!(model.rows().is_empty());
        assert!(
            model.apply_event(1, &stopped("prompt_complete")).is_empty(),
            "a queued prefix stop must not close a tool opened later in the prefix"
        );
        model.apply_event(3, &stopped("cancelled"));
        assert_eq!(count(&model, TranscriptRowKind::ToolStopped), 1);
        assert_eq!(
            row(&model, "stopped-later-3").tool_call_id.as_deref(),
            Some("later")
        );
        assert!(model.apply_event(4, &stopped("cancelled")).is_empty());
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
            model.observe_prior(1, &content("reused", "old orphan output"));
            model.observe_prior(2, &closer);
            assert!(model.rows().is_empty(), "{closer:?}: prefix stays hidden");
            model.apply_event(10, &started(tool("reused", "New call")));
            model.apply_event(11, &stopped("cancelled"));
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
            assert!(model.apply_event(12, &stopped("cancelled")).is_empty());
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
                    model.observe_prior(1, &content("reused", "old orphan output"));
                } else {
                    model.apply_event(1, &content("reused", "old orphan output"));
                }
                model.apply_event(2, &closer);
                assert_eq!(
                    count(&model, TranscriptRowKind::ToolStopped),
                    0,
                    "{closer:?}: content alone must not invent a stopped tool"
                );
                model.apply_event(3, &started(tool("reused", "New call")));
                model.apply_event(4, &stopped("cancelled"));
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
                assert!(model.apply_event(5, &stopped("cancelled")).is_empty());
            }
        }
    }

    #[test]
    fn metadata_after_prefix_completion_does_not_create_unpaired_start() {
        for observed_start in [false, true] {
            let mut model = TranscriptModel::new();
            if observed_start {
                model.observe_prior(1, &started(tool("done", "Old call")));
            }
            model.observe_prior(2, &completed("done", false, "finished before cursor"));
            assert!(
                model
                    .apply_event(
                        10,
                        &updated("done", Some("Late title"), Some("new args"), None)
                    )
                    .is_empty(),
                "metadata alone must not render a running card for a terminal prefix tool"
            );
            assert!(model.rows().is_empty());
            assert!(model.apply_event(11, &stopped("cancelled")).is_empty());
            model.apply_event(12, &started(tool("done", "New call")));
            model.apply_event(13, &stopped("cancelled"));
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
            let deltas = m.apply_event(seq, &summary("a", text));
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
        let deltas = m.apply_event(3, &failed("out of tokens"));
        assert!(
            matches!(deltas.as_slice(), [TranscriptDelta::Patch { id, row }] if id == "notice-compaction-a" && row.text == "error: Compaction failed: out of tokens")
        );
        assert_eq!(m.rows()[0].id, "notice-compaction-a");
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
        ];
        for closer in closers {
            let label = format!("{closer:?}");
            let m = fold([started(tool("t1", "T")), closer.clone()]);
            assert_eq!(count(&m, TranscriptRowKind::ToolStopped), 1, "{label}");
            let mut partial = TranscriptModel::new();
            partial.observe_prior(1, &started(tool("t1", "T")));
            partial.observe_prior(2, &content("t1", "buffered before cursor"));
            assert!(partial.rows().is_empty(), "{label}");
            partial.apply_event(10, &closer);
            let stopped_rows = partial
                .rows()
                .iter()
                .filter(|row| row.kind == TranscriptRowKind::ToolStopped)
                .collect::<Vec<_>>();
            assert_eq!(stopped_rows.len(), 1, "{label}");
            assert_eq!(
                stopped_rows[0].tool_call_id.as_deref(),
                Some("t1"),
                "{label}"
            );
            assert_eq!(stopped_rows[0].text, "buffered before cursor", "{label}");
            let mut closed_prefix = TranscriptModel::new();
            closed_prefix.observe_prior(1, &started(tool("t1", "T")));
            closed_prefix.observe_prior(2, &content("t1", "discarded before cursor"));
            closed_prefix.observe_prior(3, &closer);
            assert!(closed_prefix.rows().is_empty(), "{label}");
            assert!(
                closed_prefix
                    .apply_event(10, &stopped("cancelled"))
                    .is_empty(),
                "{label}"
            );
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
            // A typed `/compact` streams no text; its divider is the output.
            (vec![Event::ConversationCompacted], false),
            (
                vec![Event::SessionNotice {
                    severity: "error".into(),
                    title: "Compaction failed".into(),
                    description: None,
                    key: None,
                }],
                false,
            ),
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
