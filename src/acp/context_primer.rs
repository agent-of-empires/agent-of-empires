//! Synthesises a markdown "context primer" from a structured view session's
//! persisted event log. Used by `GET /api/sessions/{id}/acp/
//! context-primer` after a `session/load` failure: the agent's model
//! context is empty, but our SQLite event store still has the visible
//! transcript, so the user can opt in to sending a compact recap of the
//! prior turns as the next user message. See #1004.
//!
//! Design (see `history/plan-context-primer.md`):
//!   - Group events into turns bounded by `UserPromptSent` and `Stopped`.
//!   - Render newest-first under a global character cap (default 24k);
//!     drop whole older turns when the budget runs out, only truncate
//!     within the newest turn if it alone exceeds the budget.
//!   - Tool calls: merge `ToolCallStarted` + `ToolCallUpdated` +
//!     `ToolCallCompleted` by id, render as a single one-liner with
//!     kind-aware key extraction (`path`, `command`, ...) and bulk-key
//!     elision (`new_string`, `file_text`, `content`, ...).
//!   - Keep `PlanUpdated` + `TodoListUpdated` as compact plan lines.
//!   - Drop `ThinkingStarted`/`ThinkingEnded`/`UsageUpdated`/mode events
//!     and other ambient noise.

use super::state::{Event, ToolCall};

pub const DEFAULT_MAX_PRIMER_CHARS: usize = 24_000;
pub const DEFAULT_MAX_PRIMER_TURNS: usize = 20;
pub const MAX_TOOL_SUMMARY_CHARS: usize = 300;
pub const MAX_ASSISTANT_TAIL_CHARS: usize = 6_000;

/// Tool argument keys whose values are bulk content (file bodies,
/// patches, stdout/stderr). Always elided in the primer, both in
/// kind-aware extraction and in the generic JSON fallback.
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

/// Tool argument keys whose values are small identifiers we want to
/// surface (paths, commands, URLs, patterns). Order matters: the
/// kind-aware extractor checks the first matching key.
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

#[derive(Debug, Clone)]
pub struct PrimerOptions {
    /// Only consider events with `seq < before_seq`. Used to exclude
    /// the `SessionContextReset` event itself and any post-reset noise.
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
    /// True when older turns were dropped or the newest turn was
    /// truncated within itself to fit the budget.
    pub truncated: bool,
    pub max_chars: usize,
    /// The user's most recent `UserPromptSent` text WHEN the session
    /// ended in a non-success terminal state (rate_limit park, or
    /// `AgentStartupError`). The prompt never reached the agent, so it
    /// is excluded from the rendered transcript and returned here for
    /// the frontend to drop into the composer as the user's pending
    /// request. None when the session ended normally or the trailing
    /// turn had real agent activity. See #1281 / #1282.
    pub unprocessed_prompt: Option<String>,
}

#[derive(Debug, Default, Clone)]
struct Turn {
    user_text: String,
    assistant_text: String,
    /// Tool calls keyed by `id` so updates/completes merge with their
    /// starting event. Render order matches insertion order.
    tool_order: Vec<String>,
    tools: std::collections::HashMap<String, ToolSummary>,
    plan_lines: Vec<String>,
    event_count: usize,
}

#[derive(Debug, Clone)]
struct ToolSummary {
    name: String,
    kind: String,
    args_preview: String,
    status: ToolStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ToolStatus {
    Running,
    Completed,
    Failed,
}

/// Build a markdown primer from the given events. Events should be in
/// ascending seq order.
pub fn build_context_primer(events: &[(u64, Event)], opts: PrimerOptions) -> ContextPrimer {
    let mut turns: Vec<Turn> = Vec::new();
    let mut current: Option<Turn> = None;
    let mut included_event_count = 0usize;
    // Tracks whether the session ended in a non-success terminal state
    // (rate-limit park or AgentStartupError) so the post-loop step can
    // recover the user's unsent prompt rather than rendering it as if
    // the agent had processed it. See #1281 / #1282.
    let mut ended_non_success = false;

    for (seq, event) in events {
        if let Some(before) = opts.before_seq {
            if *seq >= before {
                break;
            }
        }

        match event {
            Event::UserPromptSent { text, .. } => {
                if let Some(t) = current.take() {
                    turns.push(t);
                }
                current = Some(Turn {
                    user_text: text.clone(),
                    event_count: 1,
                    ..Turn::default()
                });
                included_event_count += 1;
                // A new user prompt resets the terminal-error tracking:
                // if the prior turn ended in a rate-limit but the user
                // then sent and completed another turn, that prior
                // unsent prompt is no longer the trailing state.
                ended_non_success = false;
            }
            Event::UserDiffCommentsPrompt {
                assembled_markdown, ..
            } => {
                // Same turn-boundary semantics as UserPromptSent: the
                // assembled markdown is the text the agent actually saw.
                if let Some(t) = current.take() {
                    turns.push(t);
                }
                current = Some(Turn {
                    user_text: assembled_markdown.clone(),
                    event_count: 1,
                    ..Turn::default()
                });
                included_event_count += 1;
                ended_non_success = false;
            }
            Event::AgentMessageChunk { text } => {
                let turn = current.get_or_insert_with(Turn::default);
                turn.assistant_text.push_str(text);
                turn.event_count += 1;
                included_event_count += 1;
            }
            Event::ToolCallStarted { tool_call } => {
                let turn = current.get_or_insert_with(Turn::default);
                push_tool_start(turn, tool_call);
                turn.event_count += 1;
                included_event_count += 1;
            }
            Event::ToolCallUpdated {
                tool_call_id,
                title,
                args_preview,
                ..
            } => {
                if let Some(turn) = current.as_mut() {
                    if let Some(tool) = turn.tools.get_mut(tool_call_id) {
                        if let Some(t) = title {
                            if !t.is_empty() {
                                tool.name = t.clone();
                            }
                        }
                        if let Some(a) = args_preview {
                            if !a.is_empty() {
                                tool.args_preview = a.clone();
                            }
                        }
                    }
                    turn.event_count += 1;
                }
                included_event_count += 1;
            }
            Event::ToolCallCompleted {
                tool_call_id,
                is_error,
                ..
            } => {
                if let Some(turn) = current.as_mut() {
                    if let Some(tool) = turn.tools.get_mut(tool_call_id) {
                        tool.status = if *is_error {
                            ToolStatus::Failed
                        } else {
                            ToolStatus::Completed
                        };
                    }
                    turn.event_count += 1;
                }
                included_event_count += 1;
            }
            Event::PlanUpdated { plan } => {
                let turn = current.get_or_insert_with(Turn::default);
                let mut done = 0;
                let mut in_progress = 0;
                let mut pending = 0;
                for step in &plan.steps {
                    match step.status {
                        super::state::PlanStepStatus::Done => done += 1,
                        super::state::PlanStepStatus::InProgress => in_progress += 1,
                        super::state::PlanStepStatus::Pending => pending += 1,
                        super::state::PlanStepStatus::Cancelled => {}
                    }
                }
                turn.plan_lines.push(format!(
                    "Plan: {} done, {} in progress, {} pending ({} steps)",
                    done,
                    in_progress,
                    pending,
                    plan.steps.len()
                ));
                // Include the step titles so the model knows what the
                // plan actually was, not just the bucket counts.
                // Truncate per-step so a huge plan can't monopolise the
                // budget. See review feedback on #1004.
                for step in &plan.steps {
                    let marker = match step.status {
                        super::state::PlanStepStatus::Done => "[x]",
                        super::state::PlanStepStatus::InProgress => "[~]",
                        super::state::PlanStepStatus::Pending => "[ ]",
                        super::state::PlanStepStatus::Cancelled => "[/]",
                    };
                    let title = clip_chars(step.title.trim(), 120);
                    turn.plan_lines.push(format!("  {} {}", marker, title));
                }
                turn.event_count += 1;
                included_event_count += 1;
            }
            Event::TodoListUpdated { todos } => {
                let turn = current.get_or_insert_with(Turn::default);
                let done = todos.iter().filter(|t| t.completed).count();
                turn.plan_lines
                    .push(format!("Todos: {}/{} completed", done, todos.len()));
                turn.event_count += 1;
                included_event_count += 1;
            }
            Event::Stopped { reason } => {
                if let Some(t) = current.take() {
                    turns.push(t);
                }
                included_event_count += 1;
                // Track rate-limit terminal so the post-loop step can
                // recover the user's unsent prompt as
                // `unprocessed_prompt` instead of rendering it in the
                // transcript as if the agent had processed it.
                ended_non_success = reason == "rate_limited";
            }
            Event::AgentStartupError { .. } => {
                // Startup errors are also non-success terminals: the
                // user's pending prompt (if any) never reached an
                // agent. Same recovery semantics as rate_limit.
                if let Some(t) = current.take() {
                    turns.push(t);
                }
                included_event_count += 1;
                ended_non_success = true;
            }
            // Everything else (Thinking*, UsageUpdated, ModeChanged,
            // ModesAvailable, CurrentModeChanged, AvailableCommandsUpdated,
            // RawAgentUpdate, ApprovalRequested/Resolved, DiffEmitted,
            // RateLimit, AcpSessionAssigned, SessionContextReset,
            // WakeupScheduled, ToolCallContent, AgentSwitched) is
            // either ambient state or already represented elsewhere; skip.
            _ => {}
        }
    }
    if let Some(t) = current.take() {
        turns.push(t);
    }

    // If the session ended in a non-success terminal (rate-limit park
    // or AgentStartupError) and the trailing turn has only the user's
    // prompt (no assistant text, no tool calls, no plan updates), the
    // adapter never actually processed it. Pop the turn off the recap
    // and surface its text as `unprocessed_prompt` so the recovery
    // path can drop it back into the composer as the user's pending
    // request after a switch / retry. See #1281 / #1282.
    let mut unprocessed_prompt: Option<String> = None;
    if ended_non_success {
        if let Some(last) = turns.last() {
            if last.assistant_text.is_empty()
                && last.tool_order.is_empty()
                && last.plan_lines.is_empty()
                && !last.user_text.is_empty()
            {
                let popped = turns.pop().expect("just checked last()");
                unprocessed_prompt = Some(popped.user_text);
            }
        }
    }

    if turns.is_empty() {
        return ContextPrimer {
            text: String::new(),
            included_event_count,
            included_turn_count: 0,
            truncated: false,
            max_chars: opts.max_chars,
            unprocessed_prompt,
        };
    }

    // Render newest-first under the char/turn budget. We render each
    // turn into a fully-formed markdown block (including its header),
    // then walk newest-to-oldest stuffing complete blocks into the
    // budget. If a single newest turn alone overflows, we truncate
    // within it to keep at least the user prompt + assistant tail.

    let total_turns = turns.len();
    let max_take = opts.max_turns.min(total_turns);
    let start_index = total_turns.saturating_sub(max_take);

    // Build each rendered turn body (without its `### Turn N` header,
    // numbering depends on final ordering).
    let mut bodies: Vec<String> = Vec::with_capacity(max_take);
    for turn in &turns[start_index..] {
        bodies.push(render_turn_body(turn));
    }

    let header = render_primer_header();
    let footer = render_primer_footer();
    let transcript_heading = "## Transcript\n\n";
    let truncation_notice = "_Older transcript entries were omitted to fit the primer budget._\n\n";
    // Reserve space for every fixed-shape string we will write before
    // and after the variable turn bodies, including the truncation
    // notice (assume it MAY be needed, so its slot is reserved up
    // front; if no truncation happens we just don't write it and the
    // headroom turns into slack). The exact `### Turn N\n\n` header
    // length depends on the digit count, so it's accounted for
    // per-iteration below.
    let fixed_overhead =
        header.len() + transcript_heading.len() + truncation_notice.len() + footer.len();

    if fixed_overhead >= opts.max_chars {
        // Budget too small to fit even the chrome. Emit a stub that
        // still ends with the "Current request" footer (we never want
        // to drop the user-visible "send me a prompt" cue), then
        // hard-cap. The pre-allocation below is bounded by max_chars,
        // so the result always satisfies len <= max_chars.
        let mut text = String::with_capacity(opts.max_chars);
        text.push_str(&header);
        if text.chars().count() < opts.max_chars {
            text.push_str(&footer);
        }
        if text.chars().count() > opts.max_chars {
            text = clip_chars(&text, opts.max_chars);
        }
        return ContextPrimer {
            text,
            included_event_count,
            included_turn_count: 0,
            truncated: true,
            max_chars: opts.max_chars,
            unprocessed_prompt,
        };
    }

    let body_budget = opts.max_chars - fixed_overhead;

    // Walk newest first, accumulate complete turns until adding the
    // next-oldest would exceed budget. Each turn carries its own
    // `### Turn N\n\n` header; we conservatively reserve 20 chars for
    // that (covers any plausible 1-3 digit turn count).
    let mut accepted_rev: Vec<String> = Vec::new();
    let mut accepted_chars: usize = 0;
    let turn_header_reserve = 20usize;
    let mut older_dropped = false;
    let mut newest_truncated = false;

    for (i, body) in bodies.iter().enumerate().rev() {
        let estimated = body.len() + turn_header_reserve;
        if accepted_rev.is_empty() && estimated > body_budget {
            // Newest turn alone overflows. Truncate within the turn.
            let inner_budget = body_budget.saturating_sub(turn_header_reserve);
            let truncated_body = truncate_turn_body(body, inner_budget);
            accepted_chars += truncated_body.len() + turn_header_reserve;
            accepted_rev.push(truncated_body);
            newest_truncated = true;
            if i > 0 {
                older_dropped = true;
            }
            break;
        }
        if accepted_chars + estimated > body_budget {
            older_dropped = true;
            break;
        }
        accepted_chars += estimated;
        accepted_rev.push(body.clone());
    }

    // accepted_rev holds bodies newest-first; reverse for chronological.
    accepted_rev.reverse();
    let truncated = older_dropped || newest_truncated || start_index > 0;
    let included_turn_count = accepted_rev.len();

    let mut text = String::with_capacity(fixed_overhead + accepted_chars);
    text.push_str(&header);
    if truncated {
        text.push_str(truncation_notice);
    }
    text.push_str(transcript_heading);
    for (i, body) in accepted_rev.iter().enumerate() {
        text.push_str(&format!("### Turn {}\n\n", i + 1));
        text.push_str(body);
        text.push('\n');
    }
    text.push_str(&footer);

    // Final hard cap: the per-turn estimate is conservative but a
    // pathological combination of long titles + many tool lines can
    // still push us a few chars over. Clip safely on a char boundary
    // so callers can rely on `len(primer) <= max_chars`.
    if text.chars().count() > opts.max_chars {
        text = clip_chars(&text, opts.max_chars);
    }

    ContextPrimer {
        text,
        included_event_count,
        included_turn_count,
        truncated,
        max_chars: opts.max_chars,
        unprocessed_prompt,
    }
}

fn push_tool_start(turn: &mut Turn, tool: &ToolCall) {
    let summary = ToolSummary {
        name: tool.name.clone(),
        kind: tool.kind.clone(),
        args_preview: tool.args_preview.clone(),
        status: ToolStatus::Running,
    };
    if !turn.tools.contains_key(&tool.id) {
        turn.tool_order.push(tool.id.clone());
    }
    turn.tools.insert(tool.id.clone(), summary);
}

fn render_primer_header() -> String {
    String::from(
        "# Prior structured view context\n\
         \n\
         The previous ACP session could not be loaded, so you have no memory of the conversation below. \
         Use the transcript excerpt as background context for the current request. \
         Do not repeat it back unless asked.\n\
         \n",
    )
}

fn render_primer_footer() -> String {
    String::from("\n---\n\n## Current request\n\nContinue from where we left off.\n")
}

fn render_turn_body(turn: &Turn) -> String {
    let mut out = String::new();
    if !turn.user_text.is_empty() {
        out.push_str("User:\n");
        out.push_str(turn.user_text.trim());
        out.push_str("\n\n");
    }
    if !turn.assistant_text.is_empty() {
        out.push_str("Assistant:\n");
        let tail = clip_assistant_text(&turn.assistant_text);
        out.push_str(tail.trim());
        out.push_str("\n\n");
    }
    if !turn.plan_lines.is_empty() {
        out.push_str("Plan state:\n");
        for line in &turn.plan_lines {
            out.push_str("- ");
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
    }
    if !turn.tool_order.is_empty() {
        out.push_str("Tools:\n");
        for id in &turn.tool_order {
            if let Some(tool) = turn.tools.get(id) {
                let line = render_tool_line(tool);
                out.push_str("- ");
                out.push_str(&line);
                out.push('\n');
            }
        }
        out.push('\n');
    }
    out
}

fn clip_assistant_text(s: &str) -> String {
    let total_chars = s.chars().count();
    if total_chars <= MAX_ASSISTANT_TAIL_CHARS {
        return s.to_string();
    }
    // Keep the tail (most recent assistant output is what continues
    // the conversation); prepend an elision marker. Skip by chars so
    // multi-byte UTF-8 boundaries are respected.
    let skip = total_chars - MAX_ASSISTANT_TAIL_CHARS;
    let tail: String = s.chars().skip(skip).collect();
    format!("[...earlier assistant text omitted]\n{}", tail)
}

fn render_tool_line(tool: &ToolSummary) -> String {
    let status_suffix = match tool.status {
        ToolStatus::Running => "",
        ToolStatus::Completed => " → completed",
        ToolStatus::Failed => " → failed",
    };

    let descriptor = describe_tool(&tool.name, &tool.kind, &tool.args_preview);
    let combined = format!("{}{}", descriptor, status_suffix);
    if combined.chars().count() > MAX_TOOL_SUMMARY_CHARS {
        clip_chars(&combined, MAX_TOOL_SUMMARY_CHARS - 3)
    } else {
        combined
    }
}

fn describe_tool(name: &str, kind: &str, args_preview: &str) -> String {
    let trimmed_name = if name.is_empty() { "Tool" } else { name };

    // Parse args_preview as JSON if possible; fall back to literal.
    let json: Option<serde_json::Value> = serde_json::from_str(args_preview).ok();

    if let Some(value) = json {
        if let Some(obj) = value.as_object() {
            // Kind-aware short circuits.
            match kind {
                "read" | "edit" | "delete" | "move" | "write" => {
                    if let Some(path) = pick_scalar(obj, &["file_path", "path", "relative_path"]) {
                        return format!("Tool: {} {}", trimmed_name, path);
                    }
                }
                "execute" => {
                    if let Some(cmd) = pick_scalar(obj, &["command", "cmd"]) {
                        return format!("Tool: {} `{}`", trimmed_name, cmd);
                    }
                }
                "search" => {
                    let pattern = pick_scalar(obj, &["pattern", "query", "glob"]);
                    let path = pick_scalar(obj, &["path", "relative_path"]);
                    return match (pattern, path) {
                        (Some(p), Some(loc)) => {
                            format!("Tool: {} \"{}\" in {}", trimmed_name, p, loc)
                        }
                        (Some(p), None) => format!("Tool: {} \"{}\"", trimmed_name, p),
                        (None, Some(loc)) => format!("Tool: {} {}", trimmed_name, loc),
                        (None, None) => format!("Tool: {}", trimmed_name),
                    };
                }
                "fetch" => {
                    if let Some(url) = pick_scalar(obj, &["url"]) {
                        return format!("Tool: {} {}", trimmed_name, url);
                    }
                }
                _ => {}
            }

            // Generic fallback: pick the first important scalar.
            if let Some(scalar) = pick_scalar(obj, IMPORTANT_KEYS) {
                return format!("Tool: {} {}", trimmed_name, scalar);
            }

            // Final fallback: indicate bulk content was elided when the
            // args object is non-trivial.
            let has_bulk = obj.keys().any(|k| BULK_KEYS.contains(&k.as_str()));
            if has_bulk {
                return format!("Tool: {} (bulk content omitted)", trimmed_name);
            }
            return format!("Tool: {}", trimmed_name);
        }
    }

    // args_preview is not JSON-shaped, likely already a string preview.
    // Keep a short fragment if it's not obviously bulk.
    let arg_trim = args_preview.trim();
    if arg_trim.is_empty() {
        format!("Tool: {}", trimmed_name)
    } else if arg_trim.len() <= 80 && !arg_trim.contains('\n') {
        format!("Tool: {} {}", trimmed_name, arg_trim)
    } else {
        format!("Tool: {}", trimmed_name)
    }
}

fn pick_scalar(obj: &serde_json::Map<String, serde_json::Value>, keys: &[&str]) -> Option<String> {
    for key in keys {
        if BULK_KEYS.contains(key) {
            continue;
        }
        if let Some(value) = obj.get(*key) {
            if let Some(s) = scalar_to_string(value) {
                return Some(s);
            }
        }
    }
    None
}

fn scalar_to_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(clip_chars(trimmed, 200))
            }
        }
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// UTF-8-safe clip to at most `max` characters total. Appends `...`
/// when clipped, but only when there's room (max >= 3 and the marker
/// fits). Used everywhere we need to bound a user/agent-supplied
/// string by length without risking a panic on a multi-byte boundary
/// (which `String::truncate` and direct `&s[..n]` slicing both can).
/// Guarantees: output char count <= `max`.
fn clip_chars(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let total = s.chars().count();
    if total <= max {
        return s.to_string();
    }
    // Reserve 3 chars for the marker when there's room; otherwise just
    // take up to `max` chars with no marker so we stay within the cap.
    let marker = "...";
    if max <= marker.len() {
        return s.chars().take(max).collect();
    }
    let head: String = s.chars().take(max - marker.len()).collect();
    format!("{}{}", head, marker)
}

fn truncate_turn_body(body: &str, budget: usize) -> String {
    if body.len() <= budget {
        return body.to_string();
    }
    // Preserve a head slice plus the very end of the assistant text so
    // the model sees the user prompt and the latest assistant chunk.
    let head_chunk = budget.min(2_000);
    let tail_chunk = budget.saturating_sub(head_chunk).saturating_sub(80);
    let mut head_end = head_chunk.min(body.len());
    while head_end < body.len() && !body.is_char_boundary(head_end) {
        head_end += 1;
    }
    let head = &body[..head_end];
    let tail_start = body.len().saturating_sub(tail_chunk);
    let mut idx = tail_start;
    while idx < body.len() && !body.is_char_boundary(idx) {
        idx += 1;
    }
    let tail = &body[idx..];
    format!("{}\n[...turn body truncated]\n{}", head, tail)
}

#[cfg(test)]
mod tests {
    use super::super::approvals::{Approval, ApprovalDecision, Nonce};
    use super::super::state::{Plan, PlanStep, PlanStepStatus, ToolCall};
    use super::*;
    use chrono::Utc;

    fn user_event(seq: u64, text: &str) -> (u64, Event) {
        (
            seq,
            Event::UserPromptSent {
                text: text.to_string(),
                attachments: Vec::new(),
            },
        )
    }

    fn assistant_event(seq: u64, text: &str) -> (u64, Event) {
        (
            seq,
            Event::AgentMessageChunk {
                text: text.to_string(),
            },
        )
    }

    fn stopped_event(seq: u64) -> (u64, Event) {
        (
            seq,
            Event::Stopped {
                reason: "prompt_complete".into(),
            },
        )
    }

    fn tool_event(seq: u64, id: &str, name: &str, kind: &str, args: &str) -> (u64, Event) {
        (
            seq,
            Event::ToolCallStarted {
                tool_call: ToolCall {
                    id: id.to_string(),
                    name: name.to_string(),
                    kind: kind.to_string(),
                    args_preview: args.to_string(),
                    started_at: Utc::now(),
                    parent_tool_call_id: None,
                    memory_recall: None,
                    diffs: Vec::new(),
                },
            },
        )
    }

    fn completed_event(seq: u64, id: &str, error: bool) -> (u64, Event) {
        (
            seq,
            Event::ToolCallCompleted {
                tool_call_id: id.to_string(),
                is_error: error,
                content: String::new(),
                output: Vec::new(),
                completed_at: Utc::now(),
            },
        )
    }

    #[test]
    fn empty_event_log_produces_empty_primer() {
        let primer = build_context_primer(&[], PrimerOptions::default());
        assert!(primer.text.is_empty());
        assert_eq!(primer.included_turn_count, 0);
        assert!(!primer.truncated);
    }

    #[test]
    fn renders_basic_transcript_with_user_and_assistant() {
        let events = vec![
            user_event(1, "build a CLI to do X"),
            assistant_event(2, "Here is a Rust skeleton..."),
            stopped_event(3),
        ];
        let primer = build_context_primer(&events, PrimerOptions::default());
        assert!(primer.text.contains("# Prior structured view context"));
        assert!(primer.text.contains("### Turn 1"));
        assert!(primer.text.contains("User:"));
        assert!(primer.text.contains("build a CLI to do X"));
        assert!(primer.text.contains("Assistant:"));
        assert!(primer.text.contains("Here is a Rust skeleton"));
        assert!(primer.text.contains("## Current request"));
        assert_eq!(primer.included_turn_count, 1);
        assert!(!primer.truncated);
    }

    #[test]
    fn before_seq_filters_out_post_reset_events() {
        let events = vec![
            user_event(1, "first"),
            assistant_event(2, "first reply"),
            stopped_event(3),
            (
                4,
                Event::SessionContextReset {
                    reason: "load failed".into(),
                },
            ),
            user_event(5, "second"),
            assistant_event(6, "should be excluded"),
        ];
        let opts = PrimerOptions {
            before_seq: Some(4),
            ..PrimerOptions::default()
        };
        let primer = build_context_primer(&events, opts);
        assert!(primer.text.contains("first"));
        assert!(!primer.text.contains("second"));
        assert!(!primer.text.contains("should be excluded"));
        assert_eq!(primer.included_turn_count, 1);
    }

    #[test]
    fn merges_tool_lifecycle_into_one_line() {
        let events = vec![
            user_event(1, "edit foo.rs"),
            tool_event(2, "t1", "Edit", "edit", r#"{"file_path":"src/foo.rs"}"#),
            (
                3,
                Event::ToolCallUpdated {
                    tool_call_id: "t1".into(),
                    title: Some("Edit".into()),
                    args_preview: Some(
                        r#"{"file_path":"src/foo.rs","old_string":"x","new_string":"y"}"#.into(),
                    ),
                    started_at: None,
                    diffs: None,
                },
            ),
            completed_event(4, "t1", false),
            stopped_event(5),
        ];
        let primer = build_context_primer(&events, PrimerOptions::default());
        // Only one tool line, even though there are 3 events for it.
        let lines: Vec<&str> = primer
            .text
            .lines()
            .filter(|l| l.starts_with("- Tool:"))
            .collect();
        assert_eq!(lines.len(), 1, "expected one tool line, got: {:?}", lines);
        let tool_line = lines[0];
        assert!(tool_line.contains("src/foo.rs"));
        assert!(tool_line.contains("→ completed"));
        // Bulk fields must NOT appear in the primer.
        assert!(!primer.text.contains("old_string"));
        assert!(!primer.text.contains("new_string"));
    }

    #[test]
    fn tool_failure_renders_failed_status() {
        let events = vec![
            user_event(1, "run tests"),
            tool_event(2, "t1", "Bash", "execute", r#"{"command":"cargo test"}"#),
            completed_event(3, "t1", true),
            stopped_event(4),
        ];
        let primer = build_context_primer(&events, PrimerOptions::default());
        assert!(primer.text.contains("`cargo test`"));
        assert!(primer.text.contains("→ failed"));
    }

    #[test]
    fn search_tool_extracts_pattern_and_path() {
        let events = vec![
            user_event(1, "find references"),
            tool_event(
                2,
                "t1",
                "Grep",
                "search",
                r#"{"pattern":"SessionContextReset","path":"src/acp"}"#,
            ),
            completed_event(3, "t1", false),
            stopped_event(4),
        ];
        let primer = build_context_primer(&events, PrimerOptions::default());
        let tool_line = primer
            .text
            .lines()
            .find(|l| l.starts_with("- Tool:"))
            .expect("a tool line");
        assert!(tool_line.contains("\"SessionContextReset\""));
        assert!(tool_line.contains("src/acp"));
    }

    #[test]
    fn plan_updates_render_as_compact_state_line() {
        let plan = Plan {
            plan_id: "p".into(),
            version: 1,
            steps: vec![
                PlanStep {
                    id: "1".into(),
                    title: "a".into(),
                    detail: None,
                    status: PlanStepStatus::Done,
                },
                PlanStep {
                    id: "2".into(),
                    title: "b".into(),
                    detail: None,
                    status: PlanStepStatus::InProgress,
                },
                PlanStep {
                    id: "3".into(),
                    title: "c".into(),
                    detail: None,
                    status: PlanStepStatus::Pending,
                },
            ],
        };
        let events = vec![
            user_event(1, "make a plan"),
            (2, Event::PlanUpdated { plan }),
            stopped_event(3),
        ];
        let primer = build_context_primer(&events, PrimerOptions::default());
        assert!(primer
            .text
            .contains("Plan: 1 done, 1 in progress, 1 pending"));
    }

    #[test]
    fn ambient_events_are_skipped() {
        let events = vec![
            user_event(1, "hi"),
            (2, Event::ThinkingStarted),
            assistant_event(3, "hello"),
            (4, Event::ThinkingEnded),
            (
                5,
                Event::ApprovalRequested {
                    approval: Approval {
                        nonce: Nonce::new(),
                        tool_call: ToolCall {
                            id: "tc-x".into(),
                            name: "X".into(),
                            kind: "edit".into(),
                            args_preview: "{}".into(),
                            started_at: Utc::now(),
                            parent_tool_call_id: None,
                            memory_recall: None,
                            diffs: Vec::new(),
                        },
                        destructive: false,
                        requested_at: Utc::now(),
                        resolved: None,
                    },
                },
            ),
            stopped_event(6),
        ];
        let primer = build_context_primer(&events, PrimerOptions::default());
        assert!(!primer.text.contains("Thinking"));
        assert!(!primer.text.contains("Approval"));
        assert!(primer.text.contains("hi"));
        assert!(primer.text.contains("hello"));
        // ApprovalDecision isn't read, suppress unused warning.
        let _ = ApprovalDecision::Allow;
    }

    #[test]
    fn drops_oldest_turns_when_over_budget() {
        let mut events = Vec::new();
        // Build 30 small turns, each ~100 chars of assistant text. The
        // 24k char default fits roughly 20.
        let mut seq = 1u64;
        for i in 0..30 {
            events.push(user_event(seq, &format!("user prompt #{i}")));
            seq += 1;
            events.push(assistant_event(seq, &"x".repeat(800)));
            seq += 1;
            events.push(stopped_event(seq));
            seq += 1;
        }
        let primer = build_context_primer(&events, PrimerOptions::default());
        assert!(primer.truncated, "primer should be marked truncated");
        assert!(
            primer.text.chars().count() <= DEFAULT_MAX_PRIMER_CHARS,
            "primer char count {} must fit under cap {}",
            primer.text.chars().count(),
            DEFAULT_MAX_PRIMER_CHARS,
        );
        // Newest turn must be present (turn 29).
        assert!(primer.text.contains("user prompt #29"));
        // Oldest turns must be dropped (turn 0 shouldn't fit).
        assert!(!primer.text.contains("user prompt #0\n"));
    }

    #[test]
    fn newest_turn_alone_over_budget_is_truncated_in_place() {
        let huge = "z".repeat(40_000);
        let events = vec![
            user_event(1, "say a lot"),
            assistant_event(2, &huge),
            stopped_event(3),
        ];
        // The clipped-assistant tail is ~6k chars; pick a max_chars
        // below that so the newest turn alone still exceeds the budget
        // and forces the in-place truncation branch.
        let opts = PrimerOptions {
            max_chars: 4_000,
            ..PrimerOptions::default()
        };
        let primer = build_context_primer(&events, opts);
        assert!(primer.truncated);
        assert!(
            primer.text.chars().count() <= 4_000,
            "primer char count {} must fit under 4000 cap",
            primer.text.chars().count(),
        );
        assert!(primer.text.contains("# Prior structured view context"));
        assert!(primer.text.contains("## Current request"));
    }

    #[test]
    fn assistant_text_is_clipped_with_tail_preserved() {
        let mut text = String::new();
        for i in 0..1_000 {
            text.push_str(&format!("line {i}\n"));
        }
        let events = vec![
            user_event(1, "go"),
            assistant_event(2, &text),
            stopped_event(3),
        ];
        let primer = build_context_primer(&events, PrimerOptions::default());
        // Tail must be preserved (last line should appear).
        assert!(primer.text.contains("line 999"));
        // An early line should be dropped.
        assert!(!primer.text.contains("line 0\n"));
        assert!(primer.text.contains("[...earlier assistant text omitted]"));
    }

    #[test]
    fn tiny_max_chars_does_not_panic_and_respects_cap() {
        // Pathological budget smaller than the chrome alone. Builder
        // must not panic and must keep `text.len() <= max_chars`.
        let events = vec![
            user_event(1, "hi"),
            assistant_event(2, "ok"),
            stopped_event(3),
        ];
        for max in [0usize, 1, 16, 64, 200] {
            let opts = PrimerOptions {
                max_chars: max,
                ..PrimerOptions::default()
            };
            let primer = build_context_primer(&events, opts);
            assert!(
                primer.text.chars().count() <= max,
                "max_chars={} produced {} chars",
                max,
                primer.text.chars().count(),
            );
        }
    }

    #[test]
    fn handles_non_ascii_assistant_text_without_panicking() {
        // Each emoji takes 4 UTF-8 bytes; a naive byte-based slice
        // around `MAX_ASSISTANT_TAIL_CHARS` would land in the middle
        // of one and panic on `&s[idx..]`. Exercise that path with a
        // string of 4-byte emoji to force a multi-byte boundary.
        let unit = "🦀"; // 4 bytes
        let total_chars = MAX_ASSISTANT_TAIL_CHARS + 100;
        let mut text = String::with_capacity(total_chars * 4);
        for _ in 0..total_chars {
            text.push_str(unit);
        }
        let events = vec![
            user_event(1, "go"),
            assistant_event(2, &text),
            stopped_event(3),
        ];
        let primer = build_context_primer(&events, PrimerOptions::default());
        // Must succeed (no panic) and include the elision marker.
        assert!(primer.text.contains("earlier assistant text omitted"));
    }

    #[test]
    fn plan_step_titles_appear_in_primer() {
        let plan = Plan {
            plan_id: "p".into(),
            version: 1,
            steps: vec![
                PlanStep {
                    id: "1".into(),
                    title: "investigate failure mode".into(),
                    detail: None,
                    status: PlanStepStatus::Done,
                },
                PlanStep {
                    id: "2".into(),
                    title: "wire up endpoint".into(),
                    detail: None,
                    status: PlanStepStatus::InProgress,
                },
            ],
        };
        let events = vec![
            user_event(1, "plan it"),
            (2, Event::PlanUpdated { plan }),
            stopped_event(3),
        ];
        let primer = build_context_primer(&events, PrimerOptions::default());
        assert!(primer.text.contains("investigate failure mode"));
        assert!(primer.text.contains("wire up endpoint"));
        assert!(primer.text.contains("[x]"));
        assert!(primer.text.contains("[~]"));
    }

    fn rate_limited_stop(seq: u64) -> (u64, Event) {
        (
            seq,
            Event::Stopped {
                reason: "rate_limited".into(),
            },
        )
    }

    #[test]
    fn unprocessed_prompt_popped_when_session_ends_rate_limited() {
        // User typed prompt -> /acp/prompt published UserPromptSent
        // -> adapter hit rate-limit before processing it. The bare
        // prompt at the end of the transcript must NOT be rendered as
        // history (the agent never saw it), and instead surface as
        // `unprocessed_prompt` for the recovery flow to prefill into
        // the composer. See #1281 / #1282.
        let events = vec![
            user_event(1, "earlier turn"),
            assistant_event(2, "earlier reply"),
            stopped_event(3),
            user_event(4, "Refactor the auth middleware."),
            rate_limited_stop(5),
        ];
        let primer = build_context_primer(&events, PrimerOptions::default());
        assert_eq!(
            primer.unprocessed_prompt.as_deref(),
            Some("Refactor the auth middleware.")
        );
        assert!(primer.text.contains("earlier turn"));
        assert!(
            !primer.text.contains("Refactor the auth middleware."),
            "unsent prompt must be excluded from the rendered transcript"
        );
        // Only the prior successful turn remains.
        assert_eq!(primer.included_turn_count, 1);
    }

    #[test]
    fn unprocessed_prompt_popped_when_session_ends_in_startup_error() {
        // Same semantic for AgentStartupError: the user's last prompt
        // never landed because the agent failed to come online.
        let events = vec![
            user_event(1, "try this"),
            (
                2,
                Event::AgentStartupError {
                    message: "ACP connection failed".into(),
                },
            ),
        ];
        let primer = build_context_primer(&events, PrimerOptions::default());
        assert_eq!(primer.unprocessed_prompt.as_deref(), Some("try this"));
        assert!(primer.text.is_empty() || !primer.text.contains("try this"));
        assert_eq!(primer.included_turn_count, 0);
    }

    #[test]
    fn unprocessed_prompt_none_when_trailing_turn_had_agent_activity() {
        // The trailing turn had assistant text before the rate-limit
        // landed (e.g. mid-stream cutoff). Don't pop it; the agent
        // did process some of the prompt and the user wouldn't expect
        // their question to re-appear in the composer.
        let events = vec![
            user_event(1, "say hi"),
            assistant_event(2, "hi back"),
            rate_limited_stop(3),
        ];
        let primer = build_context_primer(&events, PrimerOptions::default());
        assert!(primer.unprocessed_prompt.is_none());
        assert!(primer.text.contains("say hi"));
        assert!(primer.text.contains("hi back"));
    }

    #[test]
    fn unprocessed_prompt_resets_when_followed_by_successful_turn() {
        // The transcript shows a rate-limit recovery in the past:
        // user's earlier prompt was unsent, but they then sent it
        // again and the agent did reply. The earlier failed prompt
        // shouldn't leak into unprocessed_prompt because the trailing
        // state is a successful turn.
        let events = vec![
            user_event(1, "first try"),
            rate_limited_stop(2),
            user_event(3, "second try"),
            assistant_event(4, "ok"),
            stopped_event(5),
        ];
        let primer = build_context_primer(&events, PrimerOptions::default());
        assert!(primer.unprocessed_prompt.is_none());
        assert!(primer.text.contains("second try"));
    }

    #[test]
    fn args_preview_with_only_bulk_content_renders_omission_marker() {
        let events = vec![
            user_event(1, "write a file"),
            tool_event(
                2,
                "t1",
                "Write",
                "write",
                r#"{"file_text":"...big...","other":42}"#,
            ),
            completed_event(3, "t1", false),
            stopped_event(4),
        ];
        let primer = build_context_primer(&events, PrimerOptions::default());
        let tool_line = primer
            .text
            .lines()
            .find(|l| l.starts_with("- Tool:"))
            .expect("a tool line");
        assert!(
            tool_line.contains("bulk content omitted") || tool_line.contains("42"),
            "tool line: {tool_line}"
        );
        assert!(!primer.text.contains("...big..."));
    }
}
