//! Agent-agnostic "summary of the conversation so far" for structured-view
//! (ACP) sessions.
//!
//! Mirrors [`crate::session::smart_rename`] in spirit (a one-shot call to the
//! session's own agent CLI, e.g. `claude -p`, gated on a one-shot-capable,
//! non-sandboxed, non-overridden agent) but differs in three ways that matter:
//!
//! - It is **recurring**, not once-per-session: it fires as the transcript
//!   grows and can also be triggered on demand.
//! - Its input is the **transcript** (reconstructed from the event store),
//!   not the first prompt, so it is capped hard and summarized **incrementally**
//!   (previous summary + new-events delta) to keep each call O(delta), not
//!   O(whole transcript).
//! - The result is published as an append-only `Event::ConversationSummary`
//!   rather than mutating the session title.
//!
//! ACP carries no conversation summary of its own (the claude-agent-acp adapter
//! drops the SDK's compaction summary), so aoe generates its own here, which is
//! also what makes it work across claude / codex / opencode / others. See #2808.

use crate::acp::state::Event;
use crate::agents;
use crate::session::smart_rename::{resolve_rename_tool, strip_ansi, truncate_bytes, SkipReason};
use std::collections::HashMap;

/// Only one summary one-shot runs at a time process-wide. A summary reads the
/// whole (capped) transcript, so it is slower and costlier than a title call;
/// a single dedicated slot keeps it from starving smart-rename's snappier pool.
pub const MAX_CONCURRENT: usize = 1;

/// Hard cap on the transcript text handed to the one-shot. Kept well under both
/// macOS `ARG_MAX` (~256 KiB total argv+env) and Linux `MAX_ARG_STRLEN`
/// (128 KiB per argument) so the prompt can travel as a single argv element,
/// exactly like smart-rename, without an `E2BIG` failure or a bespoke stdin
/// transport.
// ponytail: argv transport with a 48 KiB cap; switch to piping the prompt via
// stdin only if a future change needs a materially larger single call.
const MAX_INPUT_BYTES: usize = 48_000;

/// Per-tool-call cap so a single noisy `args_preview` (a pasted file, a long
/// shell line) cannot dominate the summary input.
const MAX_TOOL_ARG_BYTES: usize = 200;

/// Cap on the previous summary fed back in for the incremental pass.
const MAX_PREV_SUMMARY_BYTES: usize = 4_000;

/// Cap a generated summary at this many characters (truncated, not dropped).
const MAX_SUMMARY_CHARS: usize = 4_000;

/// Automatic trigger: summarize once the new-events delta since the last
/// summary reaches this many bytes.
const MIN_DELTA_BYTES: usize = 8_000;

/// Automatic trigger fallback: summarize after this many completed user turns
/// since the last summary even if the byte delta is small (long, low-output
/// conversations still deserve a recap).
const MIN_DELTA_TURNS: usize = 8;

// Summary eligibility reuses smart-rename's `SkipReason` (imported above)
// minus the rename-only `NameNotDefault` / `Disabled` gates: a summary does
// not care about the title, and the on-demand path runs even when the
// automatic setting is off.

/// Resolve the agent for the summary one-shot and gate it, mirroring
/// `smart_rename::check_eligible_resolved` but without the title / enabled
/// gates. `summary_setting` is `conversation_summary_agent` (empty = session
/// agent). Structured-only, one-shot-capable, not sandboxed, not
/// command-overridden.
pub fn resolve_summary_agent(
    structured: bool,
    session_tool: &str,
    summary_setting: &str,
    sandboxed: bool,
    session_command: &str,
    overrides: &HashMap<String, String>,
) -> Result<&'static agents::AgentDef, SkipReason> {
    if !structured {
        return Err(SkipReason::NotStructured);
    }
    if sandboxed {
        return Err(SkipReason::Sandboxed);
    }
    let summary_tool = resolve_rename_tool(session_tool, summary_setting);
    let Some(agent) = agents::get_agent(summary_tool) else {
        return Err(SkipReason::NoOneshot);
    };
    if agent.oneshot_flag.is_none() {
        return Err(SkipReason::NoOneshot);
    }
    // Only a command override of the resolved summary agent's own binary
    // disqualifies it: when the summary agent is the session's own agent, the
    // session's launch command counts; when it is a different agent, the
    // one-shot launches that binary fresh, so the session command is irrelevant
    // (same semantics as smart-rename).
    let (command, override_in_cfg) = if summary_tool == session_tool {
        (session_command, overrides.contains_key(session_tool))
    } else {
        ("", overrides.contains_key(summary_tool))
    };
    if override_in_cfg || (!command.is_empty() && command != agent.binary) {
        return Err(SkipReason::CommandOverridden);
    }
    Ok(agent)
}

/// The last `ConversationSummary` in an event list: its text and the seq it
/// covered. `(None, 0)` when the session has never been summarized.
pub fn last_summary(events: &[(u64, Event)]) -> (Option<String>, u64) {
    events
        .iter()
        .rev()
        .find_map(|(_, e)| match e {
            Event::ConversationSummary {
                text,
                summarized_until_seq,
            } => Some((Some(text.clone()), *summarized_until_seq)),
            _ => None,
        })
        .unwrap_or((None, 0))
}

/// Reconstruct the transcript for events with seq greater than `since_seq` into
/// a compact, agent-readable form. Returns `(text, snapshot_seq, new_turns)`
/// where `snapshot_seq` is the highest seq included (so the caller can record
/// exactly what the summary covers) and `new_turns` counts user prompts in the
/// delta (the turn-count trigger fallback).
///
/// Tool output and thinking are dropped; tool calls are reduced to an
/// intent-only line so a `grep` dump or a file read cannot bloat the input.
/// If the rendered delta exceeds [`MAX_INPUT_BYTES`], the oldest content is
/// dropped (the tail is the most relevant for "so far") and a marker is
/// prepended so the model knows the head was truncated.
pub fn extract_transcript_delta(events: &[(u64, Event)], since_seq: u64) -> (String, u64, usize) {
    let mut out = String::new();
    let mut snapshot_seq = since_seq;
    let mut new_turns = 0usize;
    let mut agent_open = false;

    for (seq, event) in events {
        if *seq <= since_seq {
            continue;
        }
        snapshot_seq = *seq;
        match event {
            Event::UserPromptSent { text, .. } => {
                agent_open = false;
                new_turns += 1;
                out.push_str("\n[User]\n");
                out.push_str(text.trim());
                out.push('\n');
            }
            Event::AgentMessageChunk { text } => {
                // Coalesce a run of chunks under one [Assistant] header.
                if !agent_open {
                    out.push_str("\n[Assistant]\n");
                    agent_open = true;
                }
                out.push_str(text);
            }
            Event::ToolCallStarted { tool_call } => {
                agent_open = false;
                let args = truncate_bytes(tool_call.args_preview.trim(), MAX_TOOL_ARG_BYTES);
                out.push_str(&format!("\n[Tool: {}] {}\n", tool_call.name, args));
            }
            // Everything else (thinking, tool output/results, usage, lifecycle,
            // prior summaries) is intentionally excluded from the summary input.
            _ => {}
        }
    }

    if out.len() > MAX_INPUT_BYTES {
        // Keep the tail on a char boundary and flag the drop honestly.
        let start = out.len() - MAX_INPUT_BYTES;
        let mut boundary = start;
        while boundary < out.len() && !out.is_char_boundary(boundary) {
            boundary += 1;
        }
        out = format!(
            "[... earlier turns in this segment truncated ...]\n{}",
            &out[boundary..]
        );
    }

    (out, snapshot_seq, new_turns)
}

/// Instruction for the summary one-shot. Asks for a compact recap and, when a
/// previous summary is supplied, an incremental update rather than a re-read of
/// the whole history.
const INSTRUCTION: &str = "You are summarizing an ongoing coding-agent session for a human who wants \
to see, at a glance, what has happened so far. Write a concise summary in a few short bullet points: \
what the user asked for, what the agent has done, the current state, and any open thread. \
Output only the summary: no preamble, no headings, no code fences.";

/// Build the one-shot prompt: instruction, the previous summary (if any) as the
/// base to update, then the new-events delta.
pub fn build_summary_prompt(previous: Option<&str>, delta: &str) -> String {
    let mut prompt = String::from(INSTRUCTION);
    if let Some(prev) = previous {
        let prev = truncate_bytes(prev.trim(), MAX_PREV_SUMMARY_BYTES);
        prompt.push_str("\n\nSummary so far (update it with the new activity below; do not repeat unchanged points verbatim):\n");
        prompt.push_str(prev);
    }
    prompt.push_str("\n\nNew activity:\n");
    prompt.push_str(delta.trim());
    prompt
}

/// Turn raw agent stdout into a clean summary, or `None` when the agent
/// produced nothing usable. Lighter than the title sanitizer: a summary is
/// multi-line prose, so we only strip ANSI, trim, reject empty / refusals, and
/// cap the length.
pub fn sanitize_summary(raw: &str) -> Option<String> {
    let cleaned = strip_ansi(raw);
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lc = trimmed.to_lowercase();
    if lc == "none"
        || lc.starts_with("i cannot")
        || lc.starts_with("i can't")
        || lc.starts_with("i'm unable")
        || lc.starts_with("i am unable")
        || lc.starts_with("sorry")
    {
        return None;
    }
    let capped = truncate_bytes(trimmed, MAX_SUMMARY_CHARS * 4); // chars <= bytes*4 headroom
    let capped: String = capped.chars().take(MAX_SUMMARY_CHARS).collect();
    Some(capped)
}

/// Should the automatic trigger fire for this delta? `true` when the byte delta
/// or the turn-count fallback is reached. Manual (on-demand) summaries bypass
/// this entirely.
pub fn delta_meets_threshold(delta_bytes: usize, new_turns: usize) -> bool {
    delta_bytes >= MIN_DELTA_BYTES || new_turns >= MIN_DELTA_TURNS
}

#[cfg(feature = "serve")]
pub use serve::{should_trigger_summary, try_conversation_summary, SummaryTrigger};

#[cfg(feature = "serve")]
mod serve {
    use super::*;
    use crate::server::AppState;
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Longer than smart-rename's 60s: a summary reads the whole (capped)
    /// transcript and can run a bigger model turn.
    const SUMMARY_TIMEOUT: Duration = Duration::from_secs(150);

    /// What kicked off a summary attempt.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum SummaryTrigger {
        /// Fired by the ACP listener on a clean turn boundary; honours the
        /// `conversation_summary` setting and the delta threshold.
        Auto,
        /// Fired by the on-demand endpoint; ignores the setting and threshold
        /// (an explicit user request always runs if the session is eligible).
        Manual,
    }

    /// Cheap sync predicate for the ACP listener: a clean `prompt_complete`
    /// `Stopped` for a session with no in-flight summary. The setting, the
    /// eligibility gate, and the delta threshold are all re-checked inside
    /// [`try_conversation_summary`] (they need config + the event store), so
    /// this only filters the high-volume frames before spawning a task.
    pub fn should_trigger_summary(
        event: &Event,
        session_id: &str,
        inflight: &HashSet<String>,
    ) -> bool {
        matches!(event, Event::Stopped { reason } if reason == "prompt_complete")
            && !inflight.contains(session_id)
    }

    /// Marks a session as having an in-flight summary so the auto trigger and a
    /// concurrent manual request cannot both run (and race on the last-summary
    /// seq). Removed on drop.
    struct InflightGuard<'a> {
        set: &'a Mutex<HashSet<String>>,
        id: String,
    }

    impl<'a> InflightGuard<'a> {
        fn acquire(set: &'a Mutex<HashSet<String>>, id: &str) -> Option<Self> {
            let mut guard = set.lock().expect("summary_inflight poisoned");
            if !guard.insert(id.to_string()) {
                return None;
            }
            Some(Self {
                set,
                id: id.to_string(),
            })
        }
    }

    impl Drop for InflightGuard<'_> {
        fn drop(&mut self) {
            if let Ok(mut guard) = self.set.lock() {
                guard.remove(&self.id);
            }
        }
    }

    /// Best-effort conversation summary for a structured-view session. Spawn it
    /// detached: it never returns an error and never touches the prompt flow.
    /// `Auto` honours the `conversation_summary` setting and the delta
    /// threshold; `Manual` bypasses both but still requires an eligible agent.
    pub async fn try_conversation_summary(
        state: Arc<AppState>,
        session_id: String,
        trigger: SummaryTrigger,
    ) {
        let Some((profile, tool, command, project_path, sandboxed, structured)) = ({
            let instances = state.instances.read().await;
            instances.iter().find(|i| i.id == session_id).map(|i| {
                (
                    i.source_profile.clone(),
                    i.tool.clone(),
                    i.command.clone(),
                    i.project_path.clone(),
                    i.is_sandboxed(),
                    i.is_structured(),
                )
            })
        }) else {
            return;
        };

        let resolved = crate::session::repo_config::resolve_config_with_repo_or_warn(
            &profile,
            std::path::Path::new(&project_path),
        );
        if trigger == SummaryTrigger::Auto && !resolved.session.conversation_summary {
            return;
        }
        let agent = match resolve_summary_agent(
            structured,
            &tool,
            &resolved.session.conversation_summary_agent,
            sandboxed,
            &command,
            &resolved.session.agent_command_override,
        ) {
            Ok(agent) => agent,
            Err(reason) => {
                tracing::debug!(target: "conversation_summary", session = %session_id, tool = %tool, reason = reason.as_str(), "skip");
                return;
            }
        };

        // One summary per session at a time. Held across the whole call so a
        // second turn completing mid-summary does not spawn a concurrent run.
        let Some(_guard) = InflightGuard::acquire(&state.summary_inflight, &session_id) else {
            return;
        };

        let events = state.acp_event_store.replay_from(&session_id, 0);
        let (previous, last_seq) = last_summary(&events);
        let (delta, snapshot_seq, new_turns) = extract_transcript_delta(&events, last_seq);
        if delta.trim().is_empty() {
            return;
        }
        if trigger == SummaryTrigger::Auto && !delta_meets_threshold(delta.len(), new_turns) {
            return;
        }

        let prompt = build_summary_prompt(previous.as_deref(), &delta);
        let Some(argv) = crate::session::smart_rename::build_oneshot_argv(agent, &prompt) else {
            return;
        };

        let raw = {
            let Ok(_permit) = state.summary_semaphore.acquire().await else {
                return;
            };
            crate::session::smart_rename::run_oneshot(
                &session_id,
                &argv,
                &project_path,
                SUMMARY_TIMEOUT,
            )
            .await
        };
        let Some(raw) = raw else {
            return;
        };
        let Some(text) = sanitize_summary(&raw) else {
            tracing::debug!(target: "conversation_summary", session = %session_id, "skip: agent output not a usable summary");
            return;
        };

        tracing::info!(target: "conversation_summary", session = %session_id, until = snapshot_seq, "published conversation summary");
        state
            .acp_supervisor
            .publish_conversation_summary(&session_id, text, snapshot_seq);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::state::{Event, ToolCall};
    use chrono::Utc;

    fn user(text: &str) -> Event {
        Event::UserPromptSent {
            text: text.into(),
            attachments: vec![],
        }
    }
    fn agent(text: &str) -> Event {
        Event::AgentMessageChunk { text: text.into() }
    }
    fn tool(name: &str, args: &str) -> Event {
        Event::ToolCallStarted {
            tool_call: ToolCall {
                id: "t1".into(),
                name: name.into(),
                kind: "execute".into(),
                args_preview: args.into(),
                started_at: Utc::now(),
                parent_tool_call_id: None,
                memory_recall: None,
                diffs: vec![],
            },
        }
    }

    #[test]
    fn extract_includes_prompts_and_agent_text_coalesced() {
        let events = vec![
            (1, user("fix the login bug")),
            (2, agent("Looking ")),
            (3, agent("at auth.rs")),
        ];
        let (text, snap, turns) = extract_transcript_delta(&events, 0);
        assert!(text.contains("[User]\nfix the login bug"));
        // Two agent chunks coalesce under a single [Assistant] header.
        assert_eq!(text.matches("[Assistant]").count(), 1);
        assert!(text.contains("Looking at auth.rs"));
        assert_eq!(snap, 3);
        assert_eq!(turns, 1);
    }

    #[test]
    fn extract_reduces_tool_calls_to_intent_and_caps_args() {
        let big = "x".repeat(5000);
        let events = vec![(1, tool("Bash", &big))];
        let (text, _, _) = extract_transcript_delta(&events, 0);
        assert!(text.contains("[Tool: Bash]"));
        // The 5000-byte args_preview is capped, so it cannot bloat the input.
        assert!(
            text.len() < 500,
            "tool args not capped: {} bytes",
            text.len()
        );
    }

    #[test]
    fn extract_honours_since_seq_for_the_delta() {
        let events = vec![(1, user("first")), (2, agent("done")), (3, user("second"))];
        let (text, snap, turns) = extract_transcript_delta(&events, 2);
        assert!(!text.contains("first"));
        assert!(text.contains("second"));
        assert_eq!(snap, 3);
        assert_eq!(turns, 1);
    }

    #[test]
    fn extract_truncates_oversized_delta_from_the_head() {
        let long = "y".repeat(MAX_INPUT_BYTES + 10_000);
        let events = vec![(1, user("start")), (2, agent(&long))];
        let (text, _, _) = extract_transcript_delta(&events, 0);
        assert!(text.len() <= MAX_INPUT_BYTES + 64);
        assert!(text.starts_with("[... earlier turns"));
    }

    #[test]
    fn last_summary_finds_the_most_recent() {
        let events = vec![
            (
                1,
                Event::ConversationSummary {
                    text: "old".into(),
                    summarized_until_seq: 1,
                },
            ),
            (2, user("more")),
            (
                3,
                Event::ConversationSummary {
                    text: "new".into(),
                    summarized_until_seq: 2,
                },
            ),
        ];
        let (prev, seq) = last_summary(&events);
        assert_eq!(prev.as_deref(), Some("new"));
        assert_eq!(seq, 2);
    }

    #[test]
    fn last_summary_none_when_never_summarized() {
        let events = vec![(1, user("hi"))];
        assert_eq!(last_summary(&events), (None, 0));
    }

    #[test]
    fn build_prompt_is_incremental_when_previous_present() {
        let p = build_summary_prompt(Some("prior recap"), "[User]\nnext thing");
        assert!(p.contains("Summary so far"));
        assert!(p.contains("prior recap"));
        assert!(p.contains("next thing"));
        // Without a previous summary, no incremental clause.
        let p0 = build_summary_prompt(None, "[User]\nfirst");
        assert!(!p0.contains("Summary so far"));
        assert!(p0.contains("first"));
    }

    #[test]
    fn sanitize_rejects_empty_and_refusals() {
        assert!(sanitize_summary("   \n ").is_none());
        assert!(sanitize_summary("NONE").is_none());
        assert!(sanitize_summary("Sorry, I can't do that").is_none());
        assert_eq!(
            sanitize_summary("\u{1b}[32m- did the thing\u{1b}[0m").as_deref(),
            Some("- did the thing")
        );
    }

    #[test]
    fn delta_threshold_byte_or_turn() {
        assert!(delta_meets_threshold(MIN_DELTA_BYTES, 0));
        assert!(delta_meets_threshold(0, MIN_DELTA_TURNS));
        assert!(!delta_meets_threshold(
            MIN_DELTA_BYTES - 1,
            MIN_DELTA_TURNS - 1
        ));
    }

    #[test]
    fn resolve_summary_agent_gates() {
        let overrides = HashMap::new();
        // Happy path: structured claude, no override, not sandboxed.
        assert!(resolve_summary_agent(true, "claude", "", false, "", &overrides).is_ok());
        // Not structured.
        assert!(matches!(
            resolve_summary_agent(false, "claude", "", false, "", &overrides),
            Err(SkipReason::NotStructured)
        ));
        // Sandboxed.
        assert!(matches!(
            resolve_summary_agent(true, "claude", "", true, "", &overrides),
            Err(SkipReason::Sandboxed)
        ));
        // Agent without a one-shot mode (cursor).
        assert!(matches!(
            resolve_summary_agent(true, "cursor", "", false, "", &overrides),
            Err(SkipReason::NoOneshot)
        ));
        // A distinct summary agent is used verbatim.
        assert!(resolve_summary_agent(true, "claude", "codex", false, "", &overrides).is_ok());
        // Unlike smart-rename, the title never gates a summary: a
        // custom-named session is still eligible (no NameNotDefault path).
    }
}
