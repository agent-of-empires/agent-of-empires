//! Background async sub-agent transcript tailer.

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
const CONTAINER_EXEC_TIMEOUT: Duration = Duration::from_secs(10);
/// Cap on the assistant-text preview carried in progress/result.
const TEXT_PREVIEW_CHARS: usize = 240;

/// Where a sub-agent transcript lives, and how to read it.
#[derive(Clone)]
pub enum TranscriptSource {
    /// Read the transcript directly from the host filesystem.
    Host,
    /// Read the transcript from inside the session's container via
    /// `<runtime> exec <container> …` (`docker` / `podman`).
    Container {
        /// Container runtime binary, e.g. `docker`.
        runtime: &'static str,
        /// The session's container name for `<runtime> exec`.
        container: String,
    },
}

impl TranscriptSource {
    /// Whether the transcript file exists yet.
    async fn exists(&self, path: &str) -> bool {
        match self {
            TranscriptSource::Host => tokio::fs::metadata(path).await.is_ok(),
            TranscriptSource::Container { runtime, container } => {
                let mut cmd = tokio::process::Command::new(runtime);
                cmd.args(["exec", container, "test", "-e", path])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .kill_on_drop(true);
                matches!(
                    tokio::time::timeout(CONTAINER_EXEC_TIMEOUT, cmd.status()).await,
                    Ok(Ok(s)) if s.success()
                )
            }
        }
    }

    /// Read the bytes appended since `offset` (0-based).
    async fn read_from(&self, path: &str, offset: u64) -> Vec<u8> {
        match self {
            TranscriptSource::Host => {
                let Ok(mut file) = tokio::fs::File::open(path).await else {
                    return Vec::new();
                };
                if file.seek(SeekFrom::Start(offset)).await.is_err() {
                    return Vec::new();
                }
                let mut chunk = Vec::new();
                if file.read_to_end(&mut chunk).await.is_err() {
                    return Vec::new();
                }
                chunk
            }
            TranscriptSource::Container { runtime, container } => {
                // `tail -c +N` prints bytes from the 1-based byte offset N to
                // EOF, so a 0-based `offset` maps to `+(offset + 1)`.
                let start = format!("+{}", offset.saturating_add(1));
                let mut cmd = tokio::process::Command::new(runtime);
                cmd.args(["exec", container, "tail", "-c", &start, path])
                    .stdin(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .kill_on_drop(true);
                match tokio::time::timeout(CONTAINER_EXEC_TIMEOUT, cmd.output()).await {
                    Ok(Ok(out)) if out.status.success() => out.stdout,
                    _ => Vec::new(),
                }
            }
        }
    }
}

/// Removes an agent from the shared in-flight set on any tailer exit
/// (terminal event, hard-idle abort, or `event_tx` close).
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

/// Why a tailer was started. Decides how a transcript that never appears is
/// reported: for a live launch the SDK has just promised the file, so its
/// absence is a real failure, while a launch resumed after a daemon restart
/// may simply have outlived its transcript, which is lost tracking rather
/// than a failed sub-agent. See `BackgroundAgentStatus::Detached`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailerStart {
    /// A `BackgroundAgentLaunched` notification on the live connection.
    Live,
    /// `Supervisor::attach` re-tailing a launch the previous daemon left
    /// unresolved.
    Resumed,
}

/// Status and warning for a transcript that never appeared, per
/// [`TailerStart`].
fn missing_transcript_outcome(start: TailerStart) -> (BackgroundAgentStatus, &'static str) {
    match start {
        TailerStart::Live => (
            BackgroundAgentStatus::Error,
            "sub-agent transcript never appeared",
        ),
        TailerStart::Resumed => (
            BackgroundAgentStatus::Detached,
            "sub-agent transcript is no longer on disk; tracking stopped",
        ),
    }
}

/// Spawn the tailer for one async sub-agent.
pub fn spawn_tailer(
    agent_id: String,
    output_file: String,
    source: TranscriptSource,
    event_tx: Sender<Event>,
    active: Arc<Mutex<HashSet<String>>>,
    start: TailerStart,
) {
    // `insert` returns false when the id is already active: a second spawn
    // for the same agent is a no-op instead of racing two tailers against
    // one transcript.
    if !active
        .lock()
        .expect("bg-agent active set mutex poisoned")
        .insert(agent_id.clone())
    {
        return;
    }
    if output_file.is_empty() {
        // No transcript path: we can never tail it.
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
        run_tailer(agent_id, output_file, source, event_tx, start).await;
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
/// sub-agent can't bloat the snapshot payload.
const MAX_TOOLS: usize = 250;

/// Running accumulator for one agent's parsed transcript state.
#[derive(Default)]
struct Snapshot {
    tool_count: u32,
    /// Individual tool calls in order, with outcomes filled in from
    /// matching tool_result records.
    tools: Vec<ToolEntry>,
    last_tool: Option<String>,
    last_text: Option<String>,
    /// Final assistant text seen alongside an `end_turn` stop reason.
    result: Option<String>,
    /// Set once a terminal `end_turn` assistant message is parsed.
    done: bool,
    parse_errors: u32,
    parsed_any: bool,
    /// True when the most recently folded content block was `text`, false
    /// when it was `tool_use`.
    last_was_text: bool,
    /// Tool-use ids with no matching `tool_result` yet.
    unresolved_tools: HashSet<String>,
}

async fn run_tailer(
    agent_id: String,
    output_file: String,
    source: TranscriptSource,
    event_tx: Sender<Event>,
    start: TailerStart,
) {
    // Wait for the transcript to appear (the SDK writes it shortly after
    // the launch event). For a sandboxed session this checks inside the
    // container, not the host.
    let mut waited = Duration::ZERO;
    while !source.exists(&output_file).await {
        if waited >= WAIT_FILE_FOR {
            let (status, warning) = missing_transcript_outcome(start);
            let _ = event_tx
                .send(completed(
                    agent_id,
                    status,
                    Vec::new(),
                    None,
                    Some(warning.into()),
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
        let grew =
            read_new_lines(&source, &output_file, &mut offset, &mut line_buf, &mut snap).await;
        let now = Utc::now();
        if grew {
            last_growth = now;
            stalled_emitted = false;
        }

        if snap.done {
            // The explicit end_turn completion path.
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
            // Stopped tracking.
            let (status, result, warning) = infer_idle_outcome(&snap);
            let _ = event_tx
                .send(completed(
                    agent_id,
                    status,
                    snapshot_tools(&snap),
                    result,
                    warning,
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
/// folding complete JSONL records into `snap`.
async fn read_new_lines(
    source: &TranscriptSource,
    path: &str,
    offset: &mut u64,
    line_buf: &mut String,
    snap: &mut Snapshot,
) -> bool {
    let chunk = source.read_from(path, *offset).await;
    if chunk.is_empty() {
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

/// Parse one JSONL transcript line and fold it into the snapshot.
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
                                snap.last_was_text = true;
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

/// Record a tool call.
fn fold_tool_use(block: &serde_json::Value, snap: &mut Snapshot) {
    snap.tool_count += 1;
    snap.last_was_text = false;
    let name = block
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("tool")
        .to_string();
    snap.last_tool = Some(name.clone());
    let id = block
        .get("id")
        .and_then(|i| i.as_str())
        .unwrap_or_default()
        .to_string();
    // Tracked past the cap: `tools` is truncated to bound the event
    // payload, but completion state must stay accurate for every call.
    snap.unresolved_tools.insert(id.clone());
    if snap.tools.len() >= MAX_TOOLS {
        return;
    }
    let title = block.get("input").and_then(tool_title);
    snap.tools.push(ToolEntry {
        id,
        name,
        title,
        ok: None,
    });
}

/// Fill in a tool's outcome from its `tool_result`, matched by id, and
/// clear it from the unresolved set (which, unlike `tools`, is uncapped).
fn fold_tool_result(block: &serde_json::Value, snap: &mut Snapshot) {
    let Some(id) = block.get("tool_use_id").and_then(|i| i.as_str()) else {
        return;
    };
    let is_error = block
        .get("is_error")
        .and_then(|e| e.as_bool())
        .unwrap_or(false);
    snap.unresolved_tools.remove(id);
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

/// Decide what an idle-timeout (`ABORT_AFTER`, no `end_turn` ever seen)
/// really means: a genuine hang, or a sub-agent that finished speaking and
/// simply stopped writing.
fn infer_idle_outcome(snap: &Snapshot) -> (BackgroundAgentStatus, Option<String>, Option<String>) {
    let dangling_tool = !snap.unresolved_tools.is_empty();
    if snap.last_was_text && snap.last_text.is_some() && !dangling_tool {
        (
            BackgroundAgentStatus::Completed,
            snap.last_text.clone(),
            Some("no explicit end_turn marker; completion inferred from final text".into()),
        )
    } else {
        (
            BackgroundAgentStatus::Stalled,
            snap.result.clone(),
            Some("no transcript activity; stopped tracking".into()),
        )
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

    fn folded(lines: &[&str]) -> Snapshot {
        let mut snap = Snapshot::default();
        for line in lines {
            fold_line(line, &mut snap);
        }
        snap
    }

    /// A transcript that never appears means different things per
    /// [`TailerStart`]: the SDK failing to write one for a live launch is a
    /// real error, while a launch resumed after a daemon restart whose
    /// transcript has since been cleaned up is lost tracking, and reporting
    /// that as `Error` would show a sub-agent that very likely finished fine
    /// as failed.
    #[test]
    fn a_missing_transcript_is_an_error_only_for_a_live_launch() {
        assert_eq!(
            missing_transcript_outcome(TailerStart::Live).0,
            BackgroundAgentStatus::Error
        );
        assert_eq!(
            missing_transcript_outcome(TailerStart::Resumed).0,
            BackgroundAgentStatus::Detached
        );
    }

    /// A second `spawn_tailer` for an id already in the active set must not
    /// spawn a competing tailer against the same transcript.
    #[tokio::test]
    async fn spawn_tailer_is_a_noop_when_the_agent_id_is_already_active() {
        let active = Arc::new(Mutex::new(HashSet::from(["dup".to_string()])));
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        spawn_tailer(
            "dup".into(),
            String::new(),
            TranscriptSource::Host,
            tx,
            active.clone(),
            TailerStart::Live,
        );
        tokio::task::yield_now().await;
        assert!(
            rx.try_recv().is_err(),
            "an id already active must not get a second tailer, even the \
             untrackable-path completion the empty output_file would otherwise emit"
        );
        assert!(active.lock().unwrap().contains("dup"));
    }

    #[tokio::test]
    async fn host_read_new_lines_reads_from_offset_and_buffers_partials() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let path_str = path.to_string_lossy().to_string();
        let source = TranscriptSource::Host;

        let line =
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#;
        tokio::fs::write(&path, line).await.unwrap();
        let mut offset = 0u64;
        let mut buf = String::new();
        let mut snap = Snapshot::default();
        assert!(read_new_lines(&source, &path_str, &mut offset, &mut buf, &mut snap).await);
        assert_eq!(snap.tool_count, 0, "a partial line must not fold");
        assert_eq!(offset, line.len() as u64);

        tokio::fs::write(&path, format!("{line}\n{line}\n"))
            .await
            .unwrap();
        assert!(read_new_lines(&source, &path_str, &mut offset, &mut buf, &mut snap).await);
        assert_eq!(snap.tool_count, 2);

        let before = offset;
        assert!(!read_new_lines(&source, &path_str, &mut offset, &mut buf, &mut snap).await);
        assert_eq!(offset, before);
    }

    #[test]
    fn fold_tracks_tools_results_text_and_end_turn() {
        let snap = folded(&[
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls -la","description":"list"}}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t2","name":"Read","input":{"file_path":"src/main.rs"}}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":false}]}}"#,
            r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t2","is_error":true}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"working on it"}]}}"#,
        ]);
        let tools: Vec<_> = snapshot_tools(&snap)
            .into_iter()
            .map(|t| (t.name, t.title, t.ok))
            .collect();
        assert_eq!(
            tools,
            [
                ("Bash".into(), Some("ls -la".into()), Some(true)),
                ("Read".into(), Some("src/main.rs".into()), Some(false)),
            ]
        );
        assert_eq!(snap.tool_count, 2);
        assert_eq!(snap.last_tool.as_deref(), Some("Read"));
        assert_eq!(snap.last_text.as_deref(), Some("working on it"));
        assert!(!snap.done);

        let snap = folded(&[
            r#"{"type":"assistant","message":{"stop_reason":"end_turn","content":[{"type":"text","text":"final answer"}]}}"#,
        ]);
        assert!(snap.done);
        assert_eq!(snap.result.as_deref(), Some("final answer"));

        let snap = folded(&[
            r#"{"type":"user","message":{"content":"prompt"}}"#,
            r#"{"attachment":{"type":"skill_listing"}}"#,
        ]);
        assert!(snap.tool_count == 0 && !snap.done && !snap.parsed_any);
        assert!(format_warning(&snap).is_none());

        let snap = folded(&["not json at all"]);
        assert_eq!(snap.parse_errors, 1);
        assert!(
            format_warning(&snap).is_some(),
            "an unreadable format is surfaced"
        );

        let long = preview(&"x".repeat(TEXT_PREVIEW_CHARS + 50));
        assert!(long.ends_with('…') && long.chars().count() <= TEXT_PREVIEW_CHARS + 1);
    }

    #[test]
    fn infer_idle_outcome_distinguishes_finished_from_hung() {
        // (lines, expected status, expected result, warning substring, case)
        let cases = vec![
            (
                vec![
                    r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash"}]}}"#,
                    r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":false}]}}"#,
                    r#"{"type":"assistant","message":{"stop_reason":null,"content":[{"type":"text","text":"final report"}]}}"#,
                ],
                BackgroundAgentStatus::Completed,
                Some("final report"),
                "completion inferred from final text",
                "final text, no dangling tool, no end_turn marker: done",
            ),
            (
                vec![
                    r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working on it"}]}}"#,
                    r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash"}]}}"#,
                ],
                BackgroundAgentStatus::Stalled,
                None,
                "no transcript activity",
                "a tool call still awaiting its result: hung",
            ),
            (
                vec![
                    r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash"},{"type":"text","text":"spoke after the call"}]}}"#,
                ],
                BackgroundAgentStatus::Stalled,
                None,
                "no transcript activity",
                "text after an unresolved tool call: still mid-action",
            ),
            (
                vec!["not json at all"],
                BackgroundAgentStatus::Stalled,
                None,
                "no transcript activity",
                "nothing parsed: hung",
            ),
        ];
        for (lines, expected_status, expected_result, warn_contains, desc) in cases {
            let (status, result, warning) = infer_idle_outcome(&folded(&lines));
            assert_eq!(status, expected_status, "{desc}");
            assert_eq!(result.as_deref(), expected_result, "{desc}");
            assert!(
                warning.unwrap_or_default().contains(warn_contains),
                "{desc}"
            );
        }

        // A dangling call past the display cap still counts as unresolved.
        let uses = (0..=MAX_TOOLS).map(|i| {
            format!(r#"{{"type":"assistant","message":{{"content":[{{"type":"tool_use","id":"t{i}","name":"Bash"}}]}}}}"#)
        });
        let results = (0..MAX_TOOLS).map(|i| {
            format!(r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"t{i}","is_error":false}}]}}}}"#)
        });
        let text =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"all done"}]}}"#;
        let lines: Vec<String> = uses.chain(results).chain([text.to_string()]).collect();
        let snap = folded(&lines.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(snap.tools.len(), MAX_TOOLS, "display list stays capped");
        assert!(snap.tools.iter().all(|t| t.ok.is_some()));
        assert_eq!(snap.tool_count as usize, MAX_TOOLS + 1);
        assert_eq!(infer_idle_outcome(&snap).0, BackgroundAgentStatus::Stalled);
    }
}
