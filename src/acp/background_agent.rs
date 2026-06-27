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

/// Spawn the tailer for one async sub-agent. Returns immediately; the
/// task runs until the agent reaches a terminal state or `event_tx`
/// closes. `output_file` is the launch payload's transcript path.
pub fn spawn_tailer(agent_id: String, output_file: String, event_tx: Sender<Event>) {
    if output_file.is_empty() {
        // No transcript path: we can never tail it. Mark it so the panel
        // doesn't show a forever-running agent.
        tokio::spawn(async move {
            let _ = event_tx
                .send(completed(
                    agent_id,
                    BackgroundAgentStatus::Error,
                    None,
                    Some("no transcript path reported for this sub-agent".into()),
                ))
                .await;
        });
        return;
    }
    tokio::spawn(async move {
        run_tailer(agent_id, output_file, event_tx).await;
    });
}

/// Running accumulator for one agent's parsed transcript state.
#[derive(Default)]
struct Snapshot {
    tool_count: u32,
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
fn fold_line(line: &str, snap: &mut Snapshot) {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        snap.parse_errors += 1;
        return;
    };
    // `attachment` / system bookkeeping lines carry no message.
    if v.get("type").and_then(|t| t.as_str()) != Some("assistant") {
        return;
    }
    let Some(msg) = v.get("message") else {
        return;
    };
    snap.parsed_any = true;
    let end_turn = msg.get("stop_reason").and_then(|s| s.as_str()) == Some("end_turn");
    if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
        for block in blocks {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("tool_use") => {
                    snap.tool_count += 1;
                    if let Some(name) = block.get("name").and_then(|n| n.as_str()) {
                        snap.last_tool = Some(name.to_string());
                    }
                }
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
    }
    if end_turn {
        snap.done = true;
    }
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
        last_tool: snap.last_tool.clone(),
        last_text: snap.last_text.clone(),
        at: Utc::now(),
    }
}

fn completed(
    agent_id: String,
    status: BackgroundAgentStatus,
    result: Option<String>,
    warning: Option<String>,
) -> Event {
    Event::BackgroundAgentCompleted {
        agent_id,
        status,
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
