//! Human-readable rendering of a retained ACP event log for `aoe acp history`.

use std::collections::HashMap;

use clap::ValueEnum;

use crate::acp::transcript::{TranscriptRow, TranscriptRowKind};

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

/// Turn folded transcript `rows` into whole entries: streamed message chunks of
/// one ACP message become one agent entry and each tool call becomes one line.
pub fn entries(rows: &[TranscriptRow]) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    // Tool call id to its entry index and whether its latest completion failed.
    let mut tools: HashMap<&str, (usize, bool)> = HashMap::new();
    let mut open_message = false;
    for row in rows {
        let before = out.len();
        match row.kind {
            TranscriptRowKind::UserPrompt
            | TranscriptRowKind::UserDiffComments
            | TranscriptRowKind::ElicitationAnswered => out.push(Entry {
                role: Role::User,
                text: user_text(row),
            }),
            TranscriptRowKind::Message => {
                match out.last_mut() {
                    Some(last) if open_message && !row.message_start => {
                        last.text.push_str(&row.text)
                    }
                    _ => out.push(Entry {
                        role: Role::Agent,
                        text: row.text.clone(),
                    }),
                }
                open_message = true;
            }
            TranscriptRowKind::ToolStart => {
                if let Some(id) = row.tool_call_id.as_deref() {
                    tools.insert(id, (out.len(), false));
                }
                out.push(Entry {
                    role: Role::Tool,
                    text: tool_line(row),
                });
            }
            TranscriptRowKind::ToolComplete | TranscriptRowKind::ToolError => {
                let failed = row.kind == TranscriptRowKind::ToolError;
                if let Some((_, status)) =
                    row.tool_call_id.as_deref().and_then(|id| tools.get_mut(id))
                {
                    *status = failed;
                }
            }
            TranscriptRowKind::Notice
            | TranscriptRowKind::Advisory
            | TranscriptRowKind::SessionCleared
            | TranscriptRowKind::ContextReset
            | TranscriptRowKind::Compacted => out.push(Entry {
                role: Role::Notice,
                text: row.text.clone(),
            }),
            _ => {}
        }
        // Rows that print nothing (thoughts, tool results) leave the message open.
        if row.kind != TranscriptRowKind::Message && out.len() > before {
            open_message = false;
        }
    }
    for &(index, failed) in tools.values() {
        if failed {
            out[index].text.push_str(" (failed)");
        }
    }
    out.retain(|entry| !entry.text.is_empty());
    out
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
    use chrono::Utc;

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
                started_at: Utc::now(),
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
            completed_at: Utc::now(),
            async_subagent: false,
        }
    }

    fn fold(events: &[Event]) -> Vec<Entry> {
        let mut model = TranscriptModel::new();
        for (i, event) in events.iter().enumerate() {
            model.apply_event(i as u64 + 1, event);
        }
        entries(model.rows())
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
