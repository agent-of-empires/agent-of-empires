//! Synthesises a markdown "context primer" from a structured view session's
//! persisted event log, for a session whose ACP conversation could not be loaded.

use std::collections::HashMap;

use super::state::{Event, PlanStepStatus, ToolCall, RATE_LIMIT_EXHAUSTED_RETRIES_REASON};

const DEFAULT_MAX_PRIMER_CHARS: usize = 24_000;
const DEFAULT_MAX_PRIMER_TURNS: usize = 20;
const MAX_TOOL_SUMMARY_CHARS: usize = 300;
const MAX_ASSISTANT_TAIL_CHARS: usize = 6_000;

/// Tool argument keys holding bulk content (file bodies, patches, output), never rendered.
const BULK_KEYS: &[&str] = &[
    "content",
    "file_text",
    "old_string",
    "new_string",
    "output",
    "stdout",
    "stderr",
    "diff",
    "patch",
    "replacement",
    "edits",
    "result",
    "text",
    "body",
];

/// Tool argument keys holding small identifiers worth surfacing.
const IMPORTANT_KEYS: &[&str] = &[
    "file_path",
    "path",
    "relative_path",
    "command",
    "cmd",
    "pattern",
    "query",
    "url",
    "glob",
    "cwd",
];

const HEADER: &str = "# Prior structured view context\n\
    \n\
    The previous ACP session could not be loaded, so you have no memory of the conversation below. \
    Use the transcript excerpt as background context for the current request. \
    Do not repeat it back unless asked.\n\
    \n";
const FOOTER: &str = "\n---\n\n## Current request\n\nContinue from where we left off.\n";
const TRANSCRIPT_HEADING: &str = "## Transcript\n\n";
const TRUNCATION_NOTICE: &str =
    "_Older transcript entries were omitted to fit the primer budget._\n\n";
/// Room kept per turn for its `### Turn N` header.
const TURN_HEADER_RESERVE: usize = 20;

#[derive(Debug, Clone)]
pub struct PrimerOptions {
    /// Only consider events with `seq < before_seq`.
    pub before_seq: Option<u64>,
    pub max_chars: usize,
    pub max_turns: usize,
}

impl Default for PrimerOptions {
    fn default() -> Self {
        Self {
            before_seq: None,
            max_chars: DEFAULT_MAX_PRIMER_CHARS,
            max_turns: DEFAULT_MAX_PRIMER_TURNS,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ContextPrimer {
    pub text: String,
    pub included_event_count: usize,
    pub included_turn_count: usize,
    /// Older turns were dropped or the newest turn was cut to fit.
    pub truncated: bool,
    pub max_chars: usize,
    /// The last prompt, when the session ended rate-limited or failed to
    /// start before the agent processed it; it is left out of the transcript.
    pub unprocessed_prompt: Option<String>,
}

#[derive(Debug, Default)]
struct Turn {
    user_text: String,
    assistant_text: String,
    /// Tool call ids in start order; updates and completions merge into `tools`.
    tool_order: Vec<String>,
    tools: HashMap<String, ToolSummary>,
    plan_lines: Vec<String>,
}

#[derive(Debug)]
struct ToolSummary {
    name: String,
    kind: String,
    args_preview: String,
    status: &'static str,
}

/// The session's turns plus how the log ended.
struct Folded {
    turns: Vec<Turn>,
    included_event_count: usize,
    ended_non_success: bool,
}

fn fold_turns(events: &[(u64, Event)], before_seq: Option<u64>) -> Folded {
    let mut turns = Vec::new();
    let mut current: Option<Turn> = None;
    let mut included_event_count = 0;
    let mut ended_non_success = false;

    for (seq, event) in events {
        if before_seq.is_some_and(|before| *seq >= before) {
            break;
        }
        match event {
            // The diff-comments markdown is the text the agent actually saw.
            Event::UserPromptSent { text, .. }
            | Event::UserDiffCommentsPrompt {
                assembled_markdown: text,
                ..
            } => {
                turns.extend(current.replace(Turn {
                    user_text: text.clone(),
                    ..Turn::default()
                }));
                ended_non_success = false;
            }
            Event::AgentMessageChunk { text } => current
                .get_or_insert_with(Turn::default)
                .assistant_text
                .push_str(text),
            Event::ToolCallStarted { tool_call } => {
                push_tool_start(current.get_or_insert_with(Turn::default), tool_call)
            }
            Event::ToolCallUpdated {
                tool_call_id,
                title,
                args_preview,
                ..
            } => {
                if let Some(tool) = current.as_mut().and_then(|t| t.tools.get_mut(tool_call_id)) {
                    if let Some(title) = title.as_ref().filter(|t| !t.is_empty()) {
                        tool.name = title.clone();
                    }
                    if let Some(args) = args_preview.as_ref().filter(|a| !a.is_empty()) {
                        tool.args_preview = args.clone();
                    }
                }
            }
            Event::ToolCallCompleted {
                tool_call_id,
                is_error,
                ..
            } => {
                if let Some(tool) = current.as_mut().and_then(|t| t.tools.get_mut(tool_call_id)) {
                    tool.status = if *is_error {
                        " → failed"
                    } else {
                        " → completed"
                    };
                }
            }
            Event::PlanUpdated { plan } => {
                let count = |status| plan.steps.iter().filter(|s| s.status == status).count();
                let lines = &mut current.get_or_insert_with(Turn::default).plan_lines;
                lines.push(format!(
                    "Plan: {} done, {} in progress, {} pending ({} steps)",
                    count(PlanStepStatus::Done),
                    count(PlanStepStatus::InProgress),
                    count(PlanStepStatus::Pending),
                    plan.steps.len()
                ));
                lines.extend(plan.steps.iter().map(|step| {
                    let marker = match step.status {
                        PlanStepStatus::Done => "[x]",
                        PlanStepStatus::InProgress => "[~]",
                        PlanStepStatus::Pending => "[ ]",
                        PlanStepStatus::Cancelled => "[/]",
                    };
                    format!("  {marker} {}", clip_chars(step.title.trim(), 120))
                }));
            }
            Event::TodoListUpdated { todos } => {
                let done = todos.iter().filter(|t| t.completed).count();
                current
                    .get_or_insert_with(Turn::default)
                    .plan_lines
                    .push(format!("Todos: {}/{} completed", done, todos.len()));
            }
            Event::Stopped { reason } => {
                turns.extend(current.take());
                ended_non_success =
                    reason == "rate_limited" || reason == RATE_LIMIT_EXHAUSTED_RETRIES_REASON;
            }
            Event::AgentStartupError { .. } => {
                turns.extend(current.take());
                ended_non_success = true;
            }
            _ => continue,
        }
        included_event_count += 1;
    }
    turns.extend(current);
    Folded {
        turns,
        included_event_count,
        ended_non_success,
    }
}

/// Build a markdown primer from the given events.
pub fn build_context_primer(events: &[(u64, Event)], opts: PrimerOptions) -> ContextPrimer {
    let Folded {
        mut turns,
        included_event_count,
        ended_non_success,
    } = fold_turns(events, opts.before_seq);

    // A prompt the agent never processed must not read as answered history.
    let unprocessed = ended_non_success
        && turns.last().is_some_and(|last| {
            last.assistant_text.is_empty()
                && last.tool_order.is_empty()
                && last.plan_lines.is_empty()
                && !last.user_text.is_empty()
        });
    let unprocessed_prompt = unprocessed.then(|| turns.pop().expect("checked").user_text);

    let primer = |text, included_turn_count, truncated| ContextPrimer {
        text,
        included_event_count,
        included_turn_count,
        truncated,
        max_chars: opts.max_chars,
        unprocessed_prompt: unprocessed_prompt.clone(),
    };
    if turns.is_empty() {
        return primer(String::new(), 0, false);
    }

    let start_index = turns.len().saturating_sub(opts.max_turns);
    let fixed_overhead =
        HEADER.len() + TRANSCRIPT_HEADING.len() + TRUNCATION_NOTICE.len() + FOOTER.len();
    if fixed_overhead >= opts.max_chars {
        // Too small for the chrome: header, then footer if room, clipped to the cap.
        let mut text = HEADER.to_string();
        if text.chars().count() < opts.max_chars {
            text.push_str(FOOTER);
        }
        return primer(clip_chars(&text, opts.max_chars), 0, true);
    }
    let body_budget = opts.max_chars - fixed_overhead;

    // Accept whole turns newest first until the next one would overflow; a
    // newest turn that overflows alone is cut in place.
    let bodies: Vec<String> = turns[start_index..].iter().map(render_turn_body).collect();
    let mut accepted: Vec<String> = Vec::new();
    let mut accepted_chars = 0;
    let mut truncated = start_index > 0;
    for body in bodies.iter().rev() {
        let estimated = body.len() + TURN_HEADER_RESERVE;
        if accepted.is_empty() && estimated > body_budget {
            let cut = truncate_turn_body(body, body_budget.saturating_sub(TURN_HEADER_RESERVE));
            accepted_chars += cut.len() + TURN_HEADER_RESERVE;
            accepted.push(cut);
            truncated = true;
            break;
        }
        if accepted_chars + estimated > body_budget {
            truncated = true;
            break;
        }
        accepted_chars += estimated;
        accepted.push(body.clone());
    }
    accepted.reverse();

    let mut text = String::with_capacity(fixed_overhead + accepted_chars);
    text.push_str(HEADER);
    if truncated {
        text.push_str(TRUNCATION_NOTICE);
    }
    text.push_str(TRANSCRIPT_HEADING);
    for (i, body) in accepted.iter().enumerate() {
        text.push_str(&format!("### Turn {}\n\n{body}\n", i + 1));
    }
    text.push_str(FOOTER);
    primer(clip_chars(&text, opts.max_chars), accepted.len(), truncated)
}

fn push_tool_start(turn: &mut Turn, tool: &ToolCall) {
    let summary = ToolSummary {
        name: tool.name.clone(),
        kind: tool.kind.clone(),
        args_preview: tool.args_preview.clone(),
        status: "",
    };
    if turn.tools.insert(tool.id.clone(), summary).is_none() {
        turn.tool_order.push(tool.id.clone());
    }
}

fn render_turn_body(turn: &Turn) -> String {
    let mut out = String::new();
    if !turn.user_text.is_empty() {
        out.push_str(&format!("User:\n{}\n\n", turn.user_text.trim()));
    }
    if !turn.assistant_text.is_empty() {
        let tail = clip_assistant_text(&turn.assistant_text);
        out.push_str(&format!("Assistant:\n{}\n\n", tail.trim()));
    }
    let mut section = |title: &str, lines: Vec<String>| {
        if !lines.is_empty() {
            out.push_str(title);
            for line in lines {
                out.push_str(&format!("- {line}\n"));
            }
            out.push('\n');
        }
    };
    section("Plan state:\n", turn.plan_lines.clone());
    section(
        "Tools:\n",
        turn.tool_order
            .iter()
            .filter_map(|id| turn.tools.get(id))
            .map(render_tool_line)
            .collect(),
    );
    out
}

/// Keep the tail: the latest assistant output is what the conversation continues from.
fn clip_assistant_text(s: &str) -> String {
    let total_chars = s.chars().count();
    if total_chars <= MAX_ASSISTANT_TAIL_CHARS {
        return s.to_string();
    }
    let tail: String = s
        .chars()
        .skip(total_chars - MAX_ASSISTANT_TAIL_CHARS)
        .collect();
    format!("[...earlier assistant text omitted]\n{tail}")
}

fn render_tool_line(tool: &ToolSummary) -> String {
    let line = format!(
        "{}{}",
        describe_tool(&tool.name, &tool.kind, &tool.args_preview),
        tool.status
    );
    if line.chars().count() > MAX_TOOL_SUMMARY_CHARS {
        clip_chars(&line, MAX_TOOL_SUMMARY_CHARS - 3)
    } else {
        line
    }
}

fn describe_tool(name: &str, kind: &str, args_preview: &str) -> String {
    let name = if name.is_empty() { "Tool" } else { name };
    let Some(obj) = serde_json::from_str::<serde_json::Value>(args_preview)
        .ok()
        .and_then(|v| v.as_object().cloned())
    else {
        // Not a JSON object: likely already a short string preview.
        let arg = args_preview.trim();
        if !arg.is_empty() && arg.len() <= 80 && !arg.contains('\n') {
            return format!("Tool: {name} {arg}");
        }
        return format!("Tool: {name}");
    };
    let detail = match kind {
        "read" | "edit" | "delete" | "move" | "write" => {
            pick_scalar(&obj, &["file_path", "path", "relative_path"])
        }
        "execute" => pick_scalar(&obj, &["command", "cmd"]).map(|cmd| format!("`{cmd}`")),
        "search" => {
            let pattern =
                pick_scalar(&obj, &["pattern", "query", "glob"]).map(|p| format!("\"{p}\""));
            let path = pick_scalar(&obj, &["path", "relative_path"]);
            return match (pattern, path) {
                (Some(p), Some(loc)) => format!("Tool: {name} {p} in {loc}"),
                (Some(detail), None) | (None, Some(detail)) => format!("Tool: {name} {detail}"),
                (None, None) => format!("Tool: {name}"),
            };
        }
        "fetch" => pick_scalar(&obj, &["url"]),
        _ => None,
    };
    match detail.or_else(|| pick_scalar(&obj, IMPORTANT_KEYS)) {
        Some(detail) => format!("Tool: {name} {detail}"),
        None if obj.keys().any(|k| BULK_KEYS.contains(&k.as_str())) => {
            format!("Tool: {name} (bulk content omitted)")
        }
        None => format!("Tool: {name}"),
    }
}

fn pick_scalar(obj: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter(|key| !BULK_KEYS.contains(key))
        .filter_map(|key| obj.get(*key))
        .find_map(|value| match value {
            serde_json::Value::String(s) if !s.trim().is_empty() => Some(clip_chars(s.trim(), 200)),
            serde_json::Value::Number(n) => Some(n.to_string()),
            serde_json::Value::Bool(b) => Some(b.to_string()),
            _ => None,
        })
}

/// UTF-8-safe clip to at most `max` characters, ending in `...` when there is room.
fn clip_chars(s: &str, max: usize) -> String {
    const MARKER: &str = "...";
    if s.chars().count() <= max {
        return s.to_string();
    }
    if max <= MARKER.len() {
        return s.chars().take(max).collect();
    }
    let head: String = s.chars().take(max - MARKER.len()).collect();
    format!("{head}{MARKER}")
}

/// Keep a head slice (the user prompt) and the very end (the latest assistant text).
fn truncate_turn_body(body: &str, budget: usize) -> String {
    if body.len() <= budget {
        return body.to_string();
    }
    let head_chunk = budget.min(2_000);
    let tail_chunk = budget.saturating_sub(head_chunk).saturating_sub(80);
    let char_boundary_from = |mut idx: usize| {
        while idx < body.len() && !body.is_char_boundary(idx) {
            idx += 1;
        }
        idx
    };
    let head = &body[..char_boundary_from(head_chunk.min(body.len()))];
    let tail = &body[char_boundary_from(body.len().saturating_sub(tail_chunk))..];
    format!("{head}\n[...turn body truncated]\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::approvals::{Approval, Nonce};
    use crate::acp::state::test_support::{chunk as assistant, prompt as user, stopped};
    use crate::acp::state::{Plan, PlanStep};
    use chrono::Utc;

    fn done() -> Event {
        stopped("prompt_complete")
    }

    fn tool_call(id: &str, name: &str, kind: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            args_preview: args.to_string(),
            started_at: Utc::now(),
            parent_tool_call_id: None,
            memory_recall: None,
            diffs: Vec::new(),
        }
    }

    fn tool(name: &str, kind: &str, args: &str) -> Event {
        Event::ToolCallStarted {
            tool_call: tool_call("t1", name, kind, args),
        }
    }

    fn completed(is_error: bool) -> Event {
        Event::ToolCallCompleted {
            tool_call_id: "t1".to_string(),
            is_error,
            content: String::new(),
            output: Vec::new(),
            completed_at: Utc::now(),
            async_subagent: false,
        }
    }

    fn with_options(events: Vec<Event>, opts: PrimerOptions) -> ContextPrimer {
        let events: Vec<(u64, Event)> = (1..).zip(events).collect();
        build_context_primer(&events, opts)
    }

    fn primer(events: Vec<Event>) -> ContextPrimer {
        with_options(events, PrimerOptions::default())
    }

    fn plan(steps: &[(&str, PlanStepStatus)]) -> Event {
        Event::PlanUpdated {
            plan: Plan {
                plan_id: "p".into(),
                version: 1,
                steps: steps
                    .iter()
                    .map(|(title, status)| PlanStep {
                        id: title.to_string(),
                        title: title.to_string(),
                        detail: None,
                        status: status.clone(),
                    })
                    .collect(),
            },
        }
    }

    #[test]
    fn renders_turns_and_skips_ambient_events() {
        let empty = primer(vec![]);
        assert!(empty.text.is_empty() && empty.included_turn_count == 0 && !empty.truncated);

        let approval = Event::ApprovalRequested {
            approval: Approval {
                nonce: Nonce::new(),
                tool_call: tool_call("tc-x", "X", "edit", "{}"),
                destructive: false,
                options: Vec::new(),
                choice: false,
                requested_at: Utc::now(),
                resolved: None,
            },
        };
        let p = primer(vec![
            user("build a CLI to do X"),
            Event::ThinkingStarted,
            assistant("Here is a Rust skeleton..."),
            Event::ThinkingEnded,
            approval,
            done(),
        ]);
        for want in [
            "# Prior structured view context",
            "### Turn 1",
            "User:\nbuild a CLI to do X",
            "Assistant:\nHere is a Rust skeleton",
            "## Current request",
        ] {
            assert!(p.text.contains(want), "missing {want:?}");
        }
        assert!(!p.text.contains("Thinking") && !p.text.contains("Approval"));
        assert_eq!(
            (p.included_turn_count, p.included_event_count, p.truncated),
            (1, 3, false)
        );

        let reset = Event::SessionContextReset {
            reason: "load failed".into(),
        };
        let p = with_options(
            vec![
                user("first"),
                assistant("first reply"),
                done(),
                reset,
                user("second"),
                assistant("excluded"),
            ],
            PrimerOptions {
                before_seq: Some(4),
                ..PrimerOptions::default()
            },
        );
        assert!(
            p.text.contains("first") && !p.text.contains("second") && !p.text.contains("excluded")
        );
        assert_eq!(p.included_turn_count, 1);
    }

    #[test]
    fn tool_lines_merge_lifecycle_and_describe_arguments_without_bulk() {
        let updated = Event::ToolCallUpdated {
            tool_call_id: "t1".into(),
            title: Some("Edit".into()),
            args_preview: Some(
                r#"{"file_path":"src/foo.rs","old_string":"x","new_string":"y"}"#.into(),
            ),
            started_at: None,
            diffs: None,
        };
        // (tool events, expected tool line fragments)
        let cases: Vec<(Vec<Event>, &[&str])> = vec![
            (
                vec![
                    tool("Edit", "edit", r#"{"file_path":"src/foo.rs"}"#),
                    updated,
                    completed(false),
                ],
                &["Tool: Edit src/foo.rs → completed"],
            ),
            (
                vec![
                    tool("Bash", "execute", r#"{"command":"cargo test"}"#),
                    completed(true),
                ],
                &["Tool: Bash `cargo test` → failed"],
            ),
            (
                vec![tool(
                    "Grep",
                    "search",
                    r#"{"pattern":"SessionContextReset","path":"src/acp"}"#,
                )],
                &["Tool: Grep \"SessionContextReset\" in src/acp"],
            ),
            (
                vec![tool(
                    "Write",
                    "write",
                    r#"{"file_text":"...big...","other":42}"#,
                )],
                &["Tool: Write (bulk content omitted)"],
            ),
            (
                vec![tool("Shell", "other", "ls -la")],
                &["Tool: Shell ls -la"],
            ),
        ];
        for (events, wants) in cases {
            let mut all = vec![user("go")];
            all.extend(events);
            all.push(done());
            let p = primer(all);
            let lines: Vec<&str> = p
                .text
                .lines()
                .filter(|l| l.starts_with("- Tool:"))
                .collect();
            assert_eq!(lines.len(), 1, "one line per tool: {lines:?}");
            for want in wants {
                assert!(lines[0].contains(want), "{:?} lacks {want:?}", lines[0]);
            }
            for bulk in ["old_string", "new_string", "...big..."] {
                assert!(!p.text.contains(bulk));
            }
        }
    }

    #[test]
    fn plans_render_counts_and_step_titles() {
        let p = primer(vec![
            user("make a plan"),
            plan(&[
                ("investigate failure mode", PlanStepStatus::Done),
                ("wire up endpoint", PlanStepStatus::InProgress),
                ("c", PlanStepStatus::Pending),
            ]),
            done(),
        ]);
        for want in [
            "Plan: 1 done, 1 in progress, 1 pending (3 steps)",
            "[x] investigate failure mode",
            "[~] wire up endpoint",
            "[ ] c",
        ] {
            assert!(p.text.contains(want), "missing {want:?}");
        }
    }

    #[test]
    fn budget_drops_oldest_turns_and_clips_long_text() {
        let events = (0..30)
            .flat_map(|i| {
                [
                    user(&format!("user prompt #{i}")),
                    assistant(&"x".repeat(800)),
                    done(),
                ]
            })
            .collect();
        let p = primer(events);
        assert!(p.truncated);
        assert!(p.text.chars().count() <= DEFAULT_MAX_PRIMER_CHARS);
        assert!(p.text.contains("user prompt #29") && !p.text.contains("user prompt #0\n"));

        let p = with_options(
            vec![user("say a lot"), assistant(&"z".repeat(40_000)), done()],
            PrimerOptions {
                max_chars: 4_000,
                ..PrimerOptions::default()
            },
        );
        assert!(p.truncated && p.text.chars().count() <= 4_000);
        assert!(
            p.text.contains("# Prior structured view context")
                && p.text.contains("## Current request")
        );

        let lines: String = (0..1_000).map(|i| format!("line {i}\n")).collect();
        let p = primer(vec![user("go"), assistant(&lines), done()]);
        assert!(p.text.contains("line 999") && !p.text.contains("line 0\n"));
        assert!(p.text.contains("[...earlier assistant text omitted]"));

        // Multi-byte text clips on char boundaries.
        let crabs = "🦀".repeat(MAX_ASSISTANT_TAIL_CHARS + 100);
        assert!(primer(vec![user("go"), assistant(&crabs), done()])
            .text
            .contains("earlier assistant text omitted"));

        for max in [0usize, 1, 16, 64, 200] {
            let p = with_options(
                vec![user("hi"), assistant("ok"), done()],
                PrimerOptions {
                    max_chars: max,
                    ..PrimerOptions::default()
                },
            );
            assert!(p.text.chars().count() <= max, "max_chars={max}");
        }
    }

    #[test]
    fn unprocessed_prompt_is_split_out_only_when_the_agent_never_answered() {
        let startup_error = Event::AgentStartupError {
            message: "ACP connection failed".into(),
        };
        // (events, unprocessed prompt, rendered turns)
        let cases = [
            (
                vec![
                    user("earlier turn"),
                    assistant("earlier reply"),
                    done(),
                    user("Refactor it."),
                    stopped("rate_limited"),
                ],
                Some("Refactor it."),
                1,
            ),
            (
                vec![
                    user("earlier turn"),
                    assistant("earlier reply"),
                    done(),
                    user("Refactor it."),
                    stopped("rate_limited"),
                    stopped(RATE_LIMIT_EXHAUSTED_RETRIES_REASON),
                ],
                Some("Refactor it."),
                1,
            ),
            (
                vec![user("Refactor it."), startup_error],
                Some("Refactor it."),
                0,
            ),
            (
                vec![
                    user("say hi"),
                    assistant("hi back"),
                    stopped("rate_limited"),
                ],
                None,
                1,
            ),
            (
                vec![
                    user("first try"),
                    stopped("rate_limited"),
                    user("second try"),
                    assistant("ok"),
                    done(),
                ],
                None,
                2,
            ),
        ];
        for (i, (events, unprocessed, turns)) in cases.into_iter().enumerate() {
            let p = primer(events);
            assert_eq!(p.unprocessed_prompt.as_deref(), unprocessed, "case {i}");
            assert_eq!(p.included_turn_count, turns, "case {i}");
            if let Some(prompt) = unprocessed {
                assert!(
                    !p.text.contains(prompt),
                    "case {i}: rendered as answered history"
                );
            }
        }
    }
}
