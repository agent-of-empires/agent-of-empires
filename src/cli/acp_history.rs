//! Human-readable rendering of a retained ACP event log for `aoe acp history`.

use std::collections::HashMap;

use clap::ValueEnum;

use crate::acp::transcript::{RetainedTranscript, TranscriptRow, TranscriptRowKind};

/// Who produced a transcript entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Role {
    User,
    Agent,
    Tool,
    /// Errors and lifecycle notices.
    Notice,
}

impl Role {
    fn label(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Agent => "agent",
            Role::Tool => "tool",
            Role::Notice => "notice",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub role: Role,
    pub text: String,
}

/// Longest tool argument summary shown on a tool line.
const TOOL_ARGS_MAX_CHARS: usize = 100;

/// Turn the retained transcript into whole entries: streamed message chunks of
/// one ACP message become one agent entry and each tool invocation one line.
///
/// Per row kind, whether it closes the open agent message (a chunk with
/// `!message_start` otherwise continues it):
/// - closes: user prompt, diff comments (new turn), session cleared, context reset;
/// - keeps it open: message chunk, thinking, tool rows, notice, advisory,
///   compacted, elicitation answer, and every other row.
///
/// Entries are ordered by the seq of the event that began them. A reused tool
/// id has one start row, patched in place, so its line takes the seq of its
/// latest start and only terminal rows from that start on (completed, failed or
/// stopped) describe it.
pub(crate) fn entries(transcript: &RetainedTranscript) -> Vec<Entry> {
    /// One tool line: where it sits in `out` and how its latest invocation ended.
    struct Invocation {
        slot: usize,
        start: u64,
        failed: bool,
    }
    let seq_of = |row: &TranscriptRow| transcript.row_seq.get(&row.id).copied().unwrap_or(0);
    let mut out: Vec<(u64, Entry)> = Vec::new();
    let mut invocations: HashMap<&str, Invocation> = HashMap::new();
    let mut open_message: Option<usize> = None;
    for row in &transcript.rows {
        let seq = seq_of(row);
        match row.kind {
            TranscriptRowKind::UserPrompt | TranscriptRowKind::UserDiffComments => {
                out.push(entry(seq, Role::User, user_text(row)));
                open_message = None;
            }
            TranscriptRowKind::ElicitationAnswered => {
                out.push(entry(seq, Role::User, user_text(row)));
            }
            TranscriptRowKind::Message => match open_message {
                Some(slot) if !row.message_start => out[slot].1.text.push_str(&row.text),
                _ => {
                    out.push(entry(seq, Role::Agent, row.text.clone()));
                    open_message = Some(out.len() - 1);
                }
            },
            TranscriptRowKind::ToolStart => {
                let start = row
                    .tool_call_id
                    .as_deref()
                    .and_then(|id| transcript.tool_start_seq.get(id))
                    .copied()
                    .unwrap_or(seq);
                if let Some(id) = row.tool_call_id.as_deref() {
                    let slot = out.len();
                    invocations.insert(
                        id,
                        Invocation {
                            slot,
                            start,
                            failed: false,
                        },
                    );
                }
                out.push(entry(start, Role::Tool, tool_line(row)));
            }
            TranscriptRowKind::ToolComplete
            | TranscriptRowKind::ToolError
            | TranscriptRowKind::ToolStopped => {
                let latest = row
                    .tool_call_id
                    .as_deref()
                    .and_then(|id| invocations.get_mut(id));
                // `>=`: a start synthesized from this terminal event shares its seq.
                if let Some(invocation) = latest.filter(|i| seq >= i.start) {
                    invocation.failed = row.kind == TranscriptRowKind::ToolError;
                }
            }
            TranscriptRowKind::SessionCleared | TranscriptRowKind::ContextReset => {
                out.push(entry(seq, Role::Notice, row.text.clone()));
                open_message = None;
            }
            TranscriptRowKind::Notice
            | TranscriptRowKind::Advisory
            | TranscriptRowKind::Compacted => {
                out.push(entry(seq, Role::Notice, row.text.clone()));
            }
            _ => {}
        }
    }
    for invocation in invocations.values().filter(|i| i.failed) {
        out[invocation.slot].1.text.push_str(" (failed)");
    }
    out.sort_by_key(|(anchor, _)| *anchor);
    out.into_iter()
        .map(|(_, entry)| entry)
        .filter(|entry| !entry.text.is_empty())
        .collect()
}

fn entry(anchor: u64, role: Role, text: String) -> (u64, Entry) {
    (anchor, Entry { role, text })
}

/// The prompt text plus one marker line per attachment, so an image-only turn is not blank.
fn user_text(row: &TranscriptRow) -> String {
    let mut text = row.text.clone();
    for attachment in &row.attachments {
        if !text.is_empty() {
            text.push('\n');
        }
        let label = attachment.name.as_deref().unwrap_or(&attachment.mime_type);
        text.push_str(&format!("[attachment: {label}]"));
    }
    text
}

fn tool_line(row: &TranscriptRow) -> String {
    let args = row
        .tool
        .as_ref()
        .and_then(|t| t.args_preview.lines().next())
        .unwrap_or_default();
    let mut line = row.text.clone();
    if args.is_empty() {
        return line;
    }
    line.push(' ');
    if args.chars().count() > TOOL_ARGS_MAX_CHARS {
        line.extend(args.chars().take(TOOL_ARGS_MAX_CHARS));
        line.push('…');
    } else {
        line.push_str(args);
    }
    line
}

/// Keep only `roles` (all when empty), then the last `last` entries.
pub fn select(mut entries: Vec<Entry>, roles: &[Role], last: Option<usize>) -> Vec<Entry> {
    if !roles.is_empty() {
        entries.retain(|e| roles.contains(&e.role));
    }
    if let Some(n) = last {
        entries.drain(..entries.len().saturating_sub(n));
    }
    entries
}

/// One block per entry: `[role] first line`, continuation lines indented under it.
pub fn render(entries: &[Entry]) -> String {
    const INDENT: &str = "         ";
    let mut out = String::new();
    for entry in entries {
        for (i, line) in entry.text.lines().enumerate() {
            if i == 0 {
                out.push_str(&format!("{:<9}", format!("[{}]", entry.role.label())));
            } else {
                out.push_str(INDENT);
            }
            for c in line.chars() {
                if c.is_control() && c != '\t' {
                    out.extend(c.escape_default());
                } else {
                    out.push(c);
                }
            }
            out.push('\n');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::state::test_support::prompt;
    use crate::acp::state::ToolCall;
    use crate::acp::transcript::TranscriptModel;
    use crate::acp::Event;

    fn chunk(text: &str, id: Option<&str>) -> Event {
        Event::AgentMessageChunk {
            text: text.into(),
            message_id: id.map(Into::into),
        }
    }

    fn tool(id: &str, name: &str, args: &str) -> Event {
        Event::ToolCallStarted {
            tool_call: ToolCall {
                id: id.into(),
                name: name.into(),
                kind: "execute".into(),
                args_preview: args.into(),
                prompt: None,
                started_at: crate::time::Timestamp::now(),
                parent_tool_call_id: None,
                memory_recall: None,
                diffs: Vec::new(),
            },
        }
    }

    fn done(id: &str, is_error: bool) -> Event {
        Event::ToolCallCompleted {
            tool_call_id: id.into(),
            is_error,
            content: "x".repeat(5000),
            output: Vec::new(),
            completed_at: crate::time::Timestamp::now(),
            async_subagent: false,
        }
    }

    fn fold(events: &[Event]) -> Vec<Entry> {
        let mut model = TranscriptModel::new();
        for (i, event) in events.iter().enumerate() {
            model.apply_event(i as u64 + 1, event);
        }
        entries(&model.into_retained())
    }

    fn e(role: Role, text: &str) -> Entry {
        Entry {
            role,
            text: text.into(),
        }
    }

    #[test]
    fn folds_chunks_by_message_id_and_labels_roles() {
        let got = fold(&[
            prompt("do it"),
            chunk("Hel", Some("m1")),
            chunk("lo", Some("m1")),
            chunk("Second", Some("m2")),
            tool("t1", "Bash", "{\"command\":\"ls\"}\nmore"),
            done("t1", false),
            tool("t2", "Read", "{}"),
            done("t2", true),
            chunk("tail", Some("m3")),
        ]);
        assert_eq!(
            got,
            vec![
                e(Role::User, "do it"),
                e(Role::Agent, "Hello"),
                e(Role::Agent, "Second"),
                e(Role::Tool, "Bash {\"command\":\"ls\"}"),
                e(Role::Tool, "Read {} (failed)"),
                e(Role::Agent, "tail"),
            ]
        );
    }

    #[test]
    fn long_tool_args_are_cut_and_tool_output_never_printed() {
        let got = fold(&[tool("t", "Bash", &"a".repeat(500)), done("t", false)]);
        assert_eq!(got.len(), 1);
        assert!(got[0].text.chars().count() <= "Bash ".len() + TOOL_ARGS_MAX_CHARS + 1);
        assert!(got[0].text.ends_with('…'));
        assert!(!got[0].text.contains('x'));
    }

    #[test]
    fn select_filters_roles_before_taking_the_last_entries() {
        let all = vec![
            e(Role::User, "a"),
            e(Role::Agent, "b"),
            e(Role::Tool, "c"),
            e(Role::Agent, "d"),
            e(Role::User, "e"),
        ];
        let texts = |v: Vec<Entry>| v.into_iter().map(|e| e.text).collect::<Vec<_>>();
        assert_eq!(
            texts(select(all.clone(), &[], None)),
            ["a", "b", "c", "d", "e"]
        );
        assert_eq!(texts(select(all.clone(), &[], Some(2))), ["d", "e"]);
        assert_eq!(
            texts(select(all.clone(), &[Role::Agent, Role::Tool], Some(2))),
            ["c", "d"]
        );
        assert_eq!(
            texts(select(all, &[Role::Notice], Some(3))),
            Vec::<String>::new()
        );
    }

    #[test]
    fn keeps_structured_answers_as_user_and_lifecycle_boundaries_as_notices() {
        use crate::acp::approvals::Nonce;
        use crate::acp::elicitations::{ElicitationAnswer, ElicitationOutcome};
        let got = fold(&[
            Event::ElicitationResolved {
                nonce: Nonce("n1".into()),
                outcome: ElicitationOutcome::Accepted,
                answers: vec![ElicitationAnswer {
                    question: "Which?".into(),
                    answer: "A".into(),
                }],
            },
            Event::SessionCleared,
            Event::ConversationCompacted,
            Event::SessionContextReset {
                reason: "model switch".into(),
            },
        ]);
        assert_eq!(got.len(), 4, "{got:?}");
        assert_eq!(got[0], e(Role::User, "Which?: A"));
        assert!(got[1..].iter().all(|x| x.role == Role::Notice), "{got:?}");
    }

    #[test]
    fn empty_chunks_leave_no_blank_entry_to_hide_the_last_message() {
        let got = fold(&[
            chunk("answer", Some("m1")),
            chunk("", Some("m2")),
            chunk("", Some("m3")),
            chunk("late", Some("m3")),
        ]);
        assert_eq!(got, vec![e(Role::Agent, "answer"), e(Role::Agent, "late")]);
        assert_eq!(
            render(&select(
                fold(&[chunk("answer", Some("m1")), chunk("", Some("m2"))]),
                &[],
                Some(1)
            )),
            "[agent]  answer\n"
        );
    }

    #[test]
    fn a_thought_between_chunks_does_not_split_the_agent_message() {
        let thought = Event::AgentThoughtChunk {
            text: "hmm".into(),
            message_id: Some("t1".into()),
        };
        let got = fold(&[
            prompt("go"),
            chunk("Hel", Some("m1")),
            thought,
            chunk("lo", Some("m1")),
        ]);
        assert_eq!(got, vec![e(Role::User, "go"), e(Role::Agent, "Hello")]);
        assert_eq!(
            select(got, &[Role::Agent], Some(1)),
            vec![e(Role::Agent, "Hello")]
        );
    }

    fn agent_texts(entries: Vec<Entry>) -> Vec<String> {
        select(entries, &[Role::Agent], None)
            .into_iter()
            .map(|e| e.text)
            .collect()
    }

    #[test]
    fn a_message_stays_whole_across_every_row_kind_that_does_not_end_the_turn() {
        use crate::acp::approvals::Nonce;
        use crate::acp::elicitations::{ElicitationAnswer, ElicitationOutcome};
        let cases: Vec<(&str, Vec<Event>)> = vec![
            (
                "thought",
                vec![Event::AgentThoughtChunk {
                    text: "hmm".into(),
                    message_id: Some("t1".into()),
                }],
            ),
            (
                "tool start and result",
                vec![tool("t", "Bash", "{}"), done("t", false)],
            ),
            ("tool error", vec![tool("t", "Bash", "{}"), done("t", true)]),
            (
                "advisory",
                vec![Event::SessionNotice {
                    severity: "info".into(),
                    title: "Heads up".into(),
                    description: None,
                    key: None,
                }],
            ),
            ("notice", vec![Event::startup_error("late failure")]),
            ("compacted", vec![Event::ConversationCompacted]),
            (
                "elicitation answer",
                vec![Event::ElicitationResolved {
                    nonce: Nonce("n1".into()),
                    outcome: ElicitationOutcome::Accepted,
                    answers: vec![ElicitationAnswer {
                        question: "Which?".into(),
                        answer: "A".into(),
                    }],
                }],
            ),
        ];
        for (label, between) in cases {
            let mut events = vec![prompt("go"), chunk("Hel", Some("m1"))];
            events.extend(between);
            events.push(chunk("lo", Some("m1")));
            let got = fold(&events);
            assert_eq!(agent_texts(got.clone()), ["Hello"], "{label}: {got:?}");
            assert_eq!(
                select(got, &[Role::Agent], Some(1)),
                vec![e(Role::Agent, "Hello")],
                "{label}"
            );
        }
    }

    #[test]
    fn a_message_ends_at_a_turn_or_context_boundary() {
        let cases: Vec<(&str, Event)> = vec![
            ("next prompt", prompt("again")),
            ("session cleared", Event::SessionCleared),
            (
                "context reset",
                Event::SessionContextReset {
                    reason: "model switch".into(),
                },
            ),
            ("new message id", chunk("b", Some("m2"))),
        ];
        for (label, boundary) in cases {
            let mut events = vec![chunk("a", Some("m1")), boundary];
            if label != "new message id" {
                events.push(chunk("b", Some("m1")));
            }
            let got = fold(&events);
            assert_eq!(agent_texts(got.clone()), ["a", "b"], "{label}: {got:?}");
        }
    }

    #[test]
    fn tool_lines_follow_each_invocations_start_order_and_own_status() {
        // s = start, o = completed, e = failed, x = turn stopped (sweeps running tools).
        let cases: &[(&str, &[&str], &[&str])] = &[
            (
                "reuse still running after another tool",
                &["s t", "e t", "s u", "o u", "s t"],
                &["u {}", "t {}"],
            ),
            (
                "reuse failed after another tool",
                &["s t", "o t", "s u", "o u", "s t", "e t"],
                &["u {}", "t {} (failed)"],
            ),
            (
                "reuse succeeded after an old failure",
                &["s t", "e t", "s u", "o u", "s t", "o t"],
                &["u {}", "t {}"],
            ),
            (
                "stopped tool restarted after another",
                &["s t", "x", "s u", "o u", "s t"],
                &["u {}", "t {}"],
            ),
            (
                "stopped tool restarted then failed",
                &["s t", "x", "s t", "e t"],
                &["t {} (failed)"],
            ),
            (
                "reuse followed by a newer start",
                &["s t", "o t", "s t", "s u"],
                &["t {}", "u {}"],
            ),
            (
                "old failure hidden while the reuse runs",
                &["s t", "e t", "s t"],
                &["t {}"],
            ),
            (
                "late failure of another tool keeps its own slot",
                &["s t", "s u", "o t", "e u"],
                &["t {}", "u {} (failed)"],
            ),
            (
                "completion-only failure, start outside the window",
                &["e t"],
                &["tool call (failed)"],
            ),
            (
                "completion-only success, start outside the window",
                &["o t"],
                &["tool call"],
            ),
        ];
        for (label, ops, want) in cases {
            let events: Vec<Event> = ops
                .iter()
                .map(|op| match op.split_once(' ') {
                    Some(("s", id)) => tool(id, id, "{}"),
                    Some(("o", id)) => done(id, false),
                    Some(("e", id)) => done(id, true),
                    _ => Event::Stopped { reason: "x".into() },
                })
                .collect();
            let got: Vec<String> = select(fold(&events), &[Role::Tool], None)
                .into_iter()
                .map(|e| e.text)
                .collect();
            assert_eq!(&got, want, "{label}");
            let tail = select(fold(&events), &[Role::Tool], Some(1));
            assert_eq!(
                tail.last().map(|e| e.text.as_str()),
                want.last().copied(),
                "{label}: --last 1"
            );
        }
    }

    #[test]
    fn a_reused_tool_id_shows_its_latest_status() {
        let got = fold(&[
            tool("t", "Bash", "{}"),
            done("t", true),
            tool("t", "Bash", "{}"),
            done("t", false),
            tool("u", "Read", "{}"),
            done("u", false),
            done("u", true),
        ]);
        assert_eq!(
            got,
            vec![e(Role::Tool, "Bash {}"), e(Role::Tool, "Read {} (failed)")]
        );
    }

    #[test]
    fn attachment_only_prompt_still_prints_a_user_line() {
        use crate::daemon::{PromptAttachmentKind, PromptAttachmentRef};
        let attachment = |name: Option<&str>| PromptAttachmentRef {
            id: "a1".into(),
            kind: PromptAttachmentKind::Image,
            mime_type: "image/png".into(),
            name: name.map(Into::into),
            size: 10,
        };
        let sent = |text: &str, attachments| Event::UserPromptSent {
            text: text.into(),
            attachments,
            prompt_id: None,
            merged_prompt_ids: Vec::new(),
            synthesized: false,
            origin_plugin: None,
            origin: crate::daemon::PromptOrigin::Unknown,
        };
        let got = fold(&[
            sent("", vec![attachment(Some("shot.png"))]),
            sent("look", vec![attachment(None)]),
        ]);
        let last_user = select(got.clone(), &[Role::User], Some(1));
        assert_eq!(
            render(&last_user),
            "[user]   look\n         [attachment: image/png]\n"
        );
        assert_eq!(
            render(&select(got, &[Role::User], Some(2))),
            "[user]   [attachment: shot.png]\n[user]   look\n         [attachment: image/png]\n"
        );
    }

    #[test]
    fn render_escapes_control_characters_so_text_cannot_drive_the_terminal() {
        let out = render(&[e(Role::Agent, "a\x1b[31mred\x07\rb\tc")]);
        assert_eq!(out, "[agent]  a\\u{1b}[31mred\\u{7}\\rb\tc\n");
    }

    #[test]
    fn render_labels_each_entry_and_indents_continuations() {
        let out = render(&[e(Role::User, "one\ntwo"), e(Role::Tool, "Bash ls")]);
        assert_eq!(out, "[user]   one\n         two\n[tool]   Bash ls\n");
    }
}
