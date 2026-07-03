//! Background async sub-agent transcript tailer.
//!
//! Claude's `Task` tool, when launched with `isAsync`, completes
//! immediately on the parent ACP stream and runs off-protocol. The
//! parent stream never reports the sub-agent's progress or completion;
//! that lives only in an on-disk JSONL transcript (the launch payload's
//! `outputFile`, a symlink into `~/.claude/projects/<proj>/subagents/`).
//!
//! For each launch the daemon spawns one [`spawn_tailer`] task that
//! follows that transcript and emits `BackgroundAgentProgress` /
//! `BackgroundAgentCompleted` events so the web "Background agents" panel
//! and the inline Task card can show live status, activity, and result.
//!
//! Design (see the design debate on this feature):
//!
//! - One task per agent, keyed by the launch. It self-terminates on
//!   completion, on a hard-idle cap, or when `event_tx` closes (the
//!   session went away), so it can never outlive its session.
//! - Completion is set ONLY on a terminal `end_turn` assistant message.
//!   Idle is reported as `Stalled`, never faked as `Completed`.
//! - Progress is a throttled, coalesced snapshot (tool count + last
//!   action), not one event per transcript line, so the SQLite event log
//!   stays bounded while a mid-run reload still sees in-flight agents.
//! - Parsing is fully defensive: the transcript is an undocumented Claude
//!   SDK format. Malformed lines are counted and skipped; a format we
//!   cannot read at all degrades to a visible warning, never a panic.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio::sync::mpsc::Sender;

use crate::acp::state::{BackgroundAgentStatus, Event};

/// How often to poll the transcript for new bytes (no inotify).
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Minimum gap between two persisted `BackgroundAgentProgress` snapshots.
const PROGRESS_THROTTLE: Duration = Duration::from_millis(1500);
/// No transcript growth for this long flips the agent to `Stalled`.
const STALL_AFTER: Duration = Duration::from_secs(90);
/// No transcript growth for this long stops tracking entirely.
const ABORT_AFTER: Duration = Duration::from_secs(300);
/// Give the transcript file this long to appear after launch.
const WAIT_FILE_FOR: Duration = Duration::from_secs(30);
/// Cap on the assistant-text preview carried in progress/result.
const TEXT_PREVIEW_CHARS: usize = 240;

/// Removes an agent from the shared in-flight set on any tailer exit
/// (terminal event, hard-idle abort, or `event_tx` close). The
/// between-prompt idle watchdog treats a non-empty set as work in flight,
/// so this drop guard is what lets the session end once the sub-agent is
/// done, on every exit path. See #2573.
struct ActiveGuard {
    active: Arc<Mutex<HashSet<String>>>,
    agent_id: String,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = self.active.lock() {
            set.remove(&self.agent_id);
        }
    }
}

/// Spawn the tailer for one async sub-agent. Returns immediately; the
/// task runs until the agent reaches a terminal state or `event_tx`
/// closes. `output_file` is the launch payload's transcript path.
/// `active` is the connection's in-flight background-agent set: the id is
/// inserted here and removed when the tailer task exits (see `ActiveGuard`).
pub fn spawn_tailer(
    agent_id: String,
    output_file: String,
    event_tx: Sender<Event>,
    active: Arc<Mutex<HashSet<String>>>,
) {
    active
        .lock()
        .expect("bg-agent active set mutex poisoned")
        .insert(agent_id.clone());
    if output_file.is_empty() {
        // No transcript path: we can never tail it. Mark it so the panel
        // doesn't show a forever-running agent.
        tokio::spawn(async move {
            let _guard = ActiveGuard {
                active,
                agent_id: agent_id.clone(),
            };
            let _ = event_tx
                .send(completed(
                    agent_id,
                    BackgroundAgentStatus::Error,
                    Vec::new(),
                    None,
                    Some("no transcript path reported for this sub-agent".into()),
                ))
                .await;
        });
        return;
    }
    tokio::spawn(async move {
        let _guard = ActiveGuard {
            active,
            agent_id: agent_id.clone(),
        };
        run_tailer(agent_id, output_file, event_tx).await;
    });
}

/// One tool call parsed from the transcript, tracked by its tool_use id
/// so a later `tool_result` can fill in the outcome.
struct ToolEntry {
    id: String,
    name: String,
    title: Option<String>,
    ok: Option<bool>,
}

/// Hard cap on per-agent tool entries carried in events, so a runaway
/// sub-agent can't bloat the snapshot payload. Excess keeps the count
/// accurate (`tool_count`) but stops growing the detailed list.
const MAX_TOOLS: usize = 250;

/// Running accumulator for one agent's parsed transcript state.
#[derive(Default)]
struct Snapshot {
    tool_count: u32,
    /// Individual tool calls in order, with outcomes filled in from
    /// matching tool_result records. Capped at `MAX_TOOLS`.
    tools: Vec<ToolEntry>,
    last_tool: Option<String>,
    last_text: Option<String>,
    /// Final assistant text seen alongside an `end_turn` stop reason.
    result: Option<String>,
    /// Set once a terminal `end_turn` assistant message is parsed.
    done: bool,
    parse_errors: u32,
    parsed_any: bool,
}

async fn run_tailer(agent_id: String, output_file: String, event_tx: Sender<Event>) {
    // Wait for the transcript to appear (the SDK writes it shortly after
    // the launch event). Bail to Error if it never shows.
    let mut waited = Duration::ZERO;
    while tokio::fs::metadata(&output_file).await.is_err() {
        if waited >= WAIT_FILE_FOR {
            let _ = event_tx
                .send(completed(
                    agent_id,
                    BackgroundAgentStatus::Error,
                    Vec::new(),
                    None,
                    Some("sub-agent transcript never appeared".into()),
                ))
                .await;
            return;
        }
        tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => waited += POLL_INTERVAL,
            _ = event_tx.closed() => return, // session gone
        }
    }

    let mut offset: u64 = 0;
    let mut line_buf = String::new();
    let mut snap = Snapshot::default();
    let mut last_progress = Utc::now() - chrono::Duration::seconds(10);
    let mut last_growth = Utc::now();
    let mut stalled_emitted = false;

    loop {
        let grew = read_new_lines(&output_file, &mut offset, &mut line_buf, &mut snap).await;
        let now = Utc::now();
        if grew {
            last_growth = now;
            stalled_emitted = false;
        }

        if snap.done {
            // A clean end_turn: the only "Completed" path.
            let warning = format_warning(&snap);
            let _ = event_tx
                .send(completed(
                    agent_id,
                    BackgroundAgentStatus::Completed,
                    snapshot_tools(&snap),
                    snap.result.clone(),
                    warning,
                ))
                .await;
            return;
        }

        let idle = (now - last_growth).to_std().unwrap_or(Duration::ZERO);
        if idle >= ABORT_AFTER {
            // Stopped tracking; never claim it finished.
            let _ = event_tx
                .send(completed(
                    agent_id,
                    BackgroundAgentStatus::Stalled,
                    snapshot_tools(&snap),
                    snap.result.clone(),
                    Some("no transcript activity; stopped tracking".into()),
                ))
                .await;
            return;
        }

        let status = if idle >= STALL_AFTER {
            BackgroundAgentStatus::Stalled
        } else {
            BackgroundAgentStatus::Running
        };

        // Emit a throttled snapshot on real growth, or once when the
        // agent first transitions to Stalled so the panel reflects it.
        let throttle_ok = (now - last_progress)
            .to_std()
            .map(|d| d >= PROGRESS_THROTTLE)
            .unwrap_or(true);
        let stall_edge = status == BackgroundAgentStatus::Stalled && !stalled_emitted;
        if (grew && throttle_ok) || stall_edge {
            if event_tx
                .send(progress(agent_id.clone(), status, &snap))
                .await
                .is_err()
            {
                return; // session gone
            }
            last_progress = now;
            if stall_edge {
                stalled_emitted = true;
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
            _ = event_tx.closed() => return,
        }
    }
}

/// Read any bytes appended since `offset`, splitting on newlines and
/// folding complete JSONL records into `snap`. Returns true if the file
/// grew. A partial trailing line stays in `line_buf` for the next poll.
async fn read_new_lines(
    path: &str,
    offset: &mut u64,
    line_buf: &mut String,
    snap: &mut Snapshot,
) -> bool {
    let Ok(mut file) = tokio::fs::File::open(path).await else {
        return false;
    };
    if file.seek(SeekFrom::Start(*offset)).await.is_err() {
        return false;
    }
    let mut chunk = Vec::new();
    if file.read_to_end(&mut chunk).await.is_err() || chunk.is_empty() {
        return false;
    }
    *offset += chunk.len() as u64;
    // Transcript is UTF-8 JSONL; lossy is fine for our previews and never
    // splits a record (we only act on whole, newline-terminated lines).
    line_buf.push_str(&String::from_utf8_lossy(&chunk));
    while let Some(nl) = line_buf.find('\n') {
        let line: String = line_buf.drain(..=nl).collect();
        let line = line.trim();
        if !line.is_empty() {
            fold_line(line, snap);
        }
    }
    true
}

/// Parse one JSONL transcript line and fold it into the snapshot. Fully
/// defensive: any shape we don't recognize is ignored, not fatal.
/// Assistant lines carry `tool_use` (a tool call) and `text`; user lines
/// carry `tool_result` (the outcome), matched back by `tool_use_id`.
fn fold_line(line: &str, snap: &mut Snapshot) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        snap.parse_errors += 1;
        return;
    };
    let kind = v.get("type").and_then(|t| t.as_str());
    let Some(msg) = v.get("message") else {
        return;
    };
    let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) else {
        return;
    };
    match kind {
        Some("assistant") => {
            snap.parsed_any = true;
            let end_turn = msg.get("stop_reason").and_then(|s| s.as_str()) == Some("end_turn");
            for block in blocks {
                match block.get("type").and_then(|t| t.as_str()) {
                    Some("tool_use") => fold_tool_use(block, snap),
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                            let preview = preview(text);
                            if !preview.is_empty() {
                                snap.last_text = Some(preview.clone());
                                if end_turn {
                                    snap.result = Some(preview);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            if end_turn {
                snap.done = true;
            }
        }
        Some("user") => {
            for block in blocks {
                if block.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
                    fold_tool_result(block, snap);
                }
            }
        }
        // attachment / system bookkeeping lines: ignore.
        _ => {}
    }
}

/// Record a tool call. Bumps the count always; appends a detailed entry
/// until the cap so a huge sub-agent can't bloat the event payload.
fn fold_tool_use(block: &serde_json::Value, snap: &mut Snapshot) {
    snap.tool_count += 1;
    let name = block
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("tool")
        .to_string();
    snap.last_tool = Some(name.clone());
    if snap.tools.len() >= MAX_TOOLS {
        return;
    }
    let title = block.get("input").and_then(tool_title);
    let id = block
        .get("id")
        .and_then(|i| i.as_str())
        .unwrap_or_default()
        .to_string();
    snap.tools.push(ToolEntry {
        id,
        name,
        title,
        ok: None,
    });
}

/// Fill in a tool's outcome from its `tool_result`, matched by id.
fn fold_tool_result(block: &serde_json::Value, snap: &mut Snapshot) {
    let Some(id) = block.get("tool_use_id").and_then(|i| i.as_str()) else {
        return;
    };
    let is_error = block
        .get("is_error")
        .and_then(|e| e.as_bool())
        .unwrap_or(false);
    if let Some(entry) = snap.tools.iter_mut().find(|t| t.id == id) {
        entry.ok = Some(!is_error);
    }
}

/// Pick a short label from a tool's input: the command, file path,
/// pattern, url, or description, whichever is present first.
fn tool_title(input: &serde_json::Value) -> Option<String> {
    for key in [
        "command",
        "file_path",
        "path",
        "pattern",
        "url",
        "query",
        "description",
    ] {
        if let Some(s) = input.get(key).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return Some(preview(s));
            }
        }
    }
    None
}

/// Convert the tracked tool entries into the wire shape (drops the
/// internal id used only for result matching).
fn snapshot_tools(snap: &Snapshot) -> Vec<crate::acp::state::BackgroundAgentTool> {
    snap.tools
        .iter()
        .map(|t| crate::acp::state::BackgroundAgentTool {
            name: t.name.clone(),
            title: t.title.clone(),
            ok: t.ok,
        })
        .collect()
}

/// First `TEXT_PREVIEW_CHARS` characters of `text`, trimmed, with an
/// ellipsis if truncated.
fn preview(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= TEXT_PREVIEW_CHARS {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(TEXT_PREVIEW_CHARS).collect();
    format!("{}…", head.trim_end())
}

/// A non-fatal note when the transcript was readable but we never parsed
/// a usable assistant record (likely an SDK format change).
fn format_warning(snap: &Snapshot) -> Option<String> {
    if !snap.parsed_any && snap.parse_errors > 0 {
        Some("sub-agent transcript format not recognized; details unavailable".into())
    } else {
        None
    }
}

fn progress(agent_id: String, status: BackgroundAgentStatus, snap: &Snapshot) -> Event {
    Event::BackgroundAgentProgress {
        agent_id,
        status,
        tool_count: snap.tool_count,
        tools: snapshot_tools(snap),
        last_tool: snap.last_tool.clone(),
        last_text: snap.last_text.clone(),
        at: Utc::now(),
    }
}

fn completed(
    agent_id: String,
    status: BackgroundAgentStatus,
    tools: Vec<crate::acp::state::BackgroundAgentTool>,
    result: Option<String>,
    warning: Option<String>,
) -> Event {
    Event::BackgroundAgentCompleted {
        agent_id,
        status,
        tools,
        result,
        warning,
        ended_at: Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_counts_tools_and_tracks_last_text() {
        let mut snap = Snapshot::default();
        fold_line(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Bash"}]}}"#,
            &mut snap,
        );
        fold_line(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"working on it"}]}}"#,
            &mut snap,
        );
        assert_eq!(snap.tool_count, 1);
        assert_eq!(snap.last_tool.as_deref(), Some("Bash"));
        assert_eq!(snap.last_text.as_deref(), Some("working on it"));
        assert!(!snap.done);
    }

    #[test]
    fn fold_captures_tool_entries_with_titles_and_results() {
        let mut snap = Snapshot::default();
        fold_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls -la","description":"list"}}]}}"#,
            &mut snap,
        );
        fold_line(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"Read","input":{"file_path":"src/main.rs"}}]}}"#,
            &mut snap,
        );
        // tool_result for t1 (success) and t2 (error) arrive on user lines.
        fold_line(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":false}]}}"#,
            &mut snap,
        );
        fold_line(
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t2","is_error":true}]}}"#,
            &mut snap,
        );
        let tools = snapshot_tools(&snap);
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "Bash");
        assert_eq!(tools[0].title.as_deref(), Some("ls -la"));
        assert_eq!(tools[0].ok, Some(true));
        assert_eq!(tools[1].name, "Read");
        assert_eq!(tools[1].title.as_deref(), Some("src/main.rs"));
        assert_eq!(tools[1].ok, Some(false));
        assert_eq!(snap.tool_count, 2);
    }

    #[test]
    fn fold_marks_done_and_result_on_end_turn() {
        let mut snap = Snapshot::default();
        fold_line(
            r#"{"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"final answer"}]}}"#,
            &mut snap,
        );
        assert!(snap.done);
        assert_eq!(snap.result.as_deref(), Some("final answer"));
    }

    #[test]
    fn fold_skips_non_assistant_and_attachment_lines() {
        let mut snap = Snapshot::default();
        fold_line(
            r#"{"type":"user","message":{"content":"prompt"}}"#,
            &mut snap,
        );
        fold_line(r#"{"attachment":{"type":"skill_listing"}}"#, &mut snap);
        assert_eq!(snap.tool_count, 0);
        assert!(!snap.done);
        assert!(!snap.parsed_any);
    }

    #[test]
    fn fold_counts_parse_errors_without_panicking() {
        let mut snap = Snapshot::default();
        fold_line("not json at all", &mut snap);
        assert_eq!(snap.parse_errors, 1);
        assert!(!snap.parsed_any);
        assert!(format_warning(&snap).is_some());
    }

    #[test]
    fn preview_truncates_long_text() {
        let long = "x".repeat(TEXT_PREVIEW_CHARS + 50);
        let p = preview(&long);
        assert!(p.ends_with('…'));
        assert!(p.chars().count() <= TEXT_PREVIEW_CHARS + 1);
    }
}
