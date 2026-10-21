//! Ends a background task when Claude's session transcript records its
//! `<task-notification>`.
//!
//! claude-agent-acp reports a backgrounded command's lifecycle to a client
//! only through the AIR extension, and declaring that makes the adapter strip
//! the output of every Read that names a path. So the start comes from the
//! tool result (`toolResponse.backgroundTaskId`) and the end from the
//! transcript Claude Code writes itself: a `<task-notification>` naming the
//! task id and a terminal `<status>` lands there the moment the task ends.
//! The transcript is followed through the shared file watcher, so nothing
//! polls while it is quiet.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio::sync::mpsc::WeakSender;

use crate::acp::background_agent::TailerStart;
use crate::acp::state::Event;
use crate::file_watch::{FileMatcher, FileWatchService, WatchSpec};

/// Coalesces the burst of writes one transcript entry causes.
const WATCH_DEBOUNCE: Duration = Duration::from_millis(50);
/// A kernel watcher can drop events (queue overflow), so the transcript is
/// also re-read this often. Rare enough not to be polling.
const SAFETY_REREAD: Duration = Duration::from_secs(30);
/// Used only when no watcher is available (file watching off or failed).
const FALLBACK_POLL: Duration = Duration::from_millis(500);
/// A live launch also reads this much of the transcript's tail, for a
/// notification that landed before the tailer started.
const LIVE_LOOKBACK_BYTES: u64 = 256 * 1024;
/// The transcript is read and scanned in chunks of this size, so a long
/// conversation is never held in memory.
const READ_CHUNK_BYTES: usize = 256 * 1024;
/// A single line longer than this is skipped, not buffered.
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;
/// `<status>` values after which a task will not report again.
const TERMINAL_STATUSES: &[&str] = &["completed", "failed", "stopped", "killed"];

/// Called each time a tailer has scanned everything written so far and is
/// about to wait for the next change. Tests use it as a causal checkpoint.
pub(crate) type IdleHook = Arc<dyn Fn() + Send + Sync>;

/// Whether `line` is the harness-written record of `task_id` ending: either
/// the queued notification or the user message delivering it, whose text is
/// the notification itself. The transcript also holds content the task id can
/// leak into (a tool result that read another transcript, model text, API
/// dumps), so a substring match on the raw line is not enough. Monitor event
/// notifications carry the task id without a status.
fn is_terminal_notification(line: &str, task_id: &str) -> bool {
    // Cheap filter before parsing: nearly every line is about something else.
    if !line.contains(task_id) {
        return false;
    }
    let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    let text = match entry.get("type").and_then(serde_json::Value::as_str) {
        Some("queue-operation")
            if entry.get("operation").and_then(serde_json::Value::as_str) == Some("enqueue") =>
        {
            entry.get("content")
        }
        Some("user") => entry.pointer("/message/content"),
        _ => None,
    }
    .and_then(serde_json::Value::as_str);
    text.is_some_and(|text| {
        text.trim_start().starts_with("<task-notification>")
            && text.contains(&format!("<task-id>{task_id}</task-id>"))
            && TERMINAL_STATUSES
                .iter()
                .any(|status| text.contains(&format!("<status>{status}</status>")))
    })
}

/// `<home>/projects/<encoded-cwd>/<session>.jsonl`. The encoded directory is
/// derived from the cwd the agent ran in, so this scans `projects/` the way
/// the adapter itself does instead of recomputing it.
fn find_session_transcript(claude_home: &Path, session_id: &str) -> Option<PathBuf> {
    if session_id.is_empty() || session_id.contains(['/', '\\']) || session_id == ".." {
        return None;
    }
    let leaf = format!("{session_id}.jsonl");
    std::fs::read_dir(claude_home.join("projects"))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join(&leaf))
        .find(|candidate| candidate.is_file())
}

/// The process-wide watcher the transcript tailers share. Created on first
/// use, inside the runtime; a failed backend degrades to a no-op service.
fn shared_watch() -> Arc<FileWatchService> {
    static WATCH: OnceLock<Arc<FileWatchService>> = OnceLock::new();
    WATCH
        .get_or_init(|| FileWatchService::new().unwrap_or_else(|_| FileWatchService::noop()))
        .clone()
}

/// Locks the in-flight set. It only holds plain ids, so a tailer that
/// panicked holding the lock leaves it valid; a poisoned lock must not strand
/// the task it was about to track.
fn lock_active(active: &Mutex<HashSet<String>>) -> MutexGuard<'_, HashSet<String>> {
    active.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Removes a task from the shared in-flight set on any tailer exit.
struct ActiveGuard {
    active: Arc<Mutex<HashSet<String>>>,
    task_id: String,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        lock_active(&self.active).remove(&self.task_id);
    }
}

/// The tailer of one background task: follows the session transcript until
/// the task ends, then publishes `BackgroundTaskFinished`. Every way out of
/// tracking ends with that finish (a transcript that cannot be found or a
/// Claude home that cannot be resolved included), because the start was
/// already published and the session must not stay busy for a task nothing
/// can report on. It holds only a weak sender, so it never keeps the worker's
/// event channel open: once the connection is gone it exits at its next wake
/// instead of blocking the drain's recovery.
pub(crate) struct TaskTailer {
    /// `None` when the Claude config dir could not be resolved.
    pub claude_home: Option<PathBuf>,
    pub watch: Arc<FileWatchService>,
    pub task_id: String,
    pub session_id: String,
    pub active: Arc<Mutex<HashSet<String>>>,
    pub event_tx: WeakSender<Event>,
    pub start: TailerStart,
    pub idle: IdleHook,
}

impl TaskTailer {
    /// Must run inside the tokio runtime. A task that already has a tailer
    /// keeps it, and that one finishes it.
    pub(crate) fn spawn(self) {
        if !lock_active(&self.active).insert(self.task_id.clone()) {
            return;
        }
        tokio::spawn(async move {
            let _guard = ActiveGuard {
                active: self.active.clone(),
                task_id: self.task_id.clone(),
            };
            self.follow().await;
            // Upgraded only for the send: the connection may be gone by now.
            if let Some(tx) = self.event_tx.upgrade() {
                let _ = tx
                    .send(Event::BackgroundTaskFinished {
                        task_id: self.task_id.clone(),
                    })
                    .await;
            }
        });
    }
}

/// Track `task_id` through the session transcript of the host's Claude home.
/// Must run inside the tokio runtime.
pub fn spawn_task_tailer(
    task_id: String,
    session_id: String,
    active: Arc<Mutex<HashSet<String>>>,
    event_tx: WeakSender<Event>,
    start: TailerStart,
) {
    TaskTailer {
        claude_home: crate::session::capture::claude_home_for_host_environment(None, &[]).ok(),
        watch: shared_watch(),
        task_id,
        session_id,
        active,
        event_tx,
        start,
        idle: Arc::new(|| {}),
    }
    .spawn();
}

/// Where a transcript has been read to, and the partial line after it.
struct Cursor {
    offset: u64,
    carry: Vec<u8>,
    /// Inside a line that began before the read window, or was too long.
    skipping_line: bool,
}

enum Scan {
    Ended,
    /// Nothing ended the task yet.
    Pending,
    /// The transcript cannot be read any more.
    Gone,
}

impl TaskTailer {
    /// Returns once the task ended, or tracking was given up.
    async fn follow(&self) {
        // A task start implies the session already ran a tool, so its
        // transcript exists; one that cannot be found is not trackable.
        let Some(home) = self.claude_home.clone() else {
            return;
        };
        let session = self.session_id.clone();
        let Ok(Some(path)) =
            tokio::task::spawn_blocking(move || find_session_transcript(&home, &session)).await
        else {
            return;
        };
        let path = tokio::fs::canonicalize(&path).await.unwrap_or(path);
        let Some(dir) = path.parent() else {
            return;
        };
        // Subscribe before the first read, so an append between the two still
        // wakes the loop. No watcher (noop service, failed install) means a
        // closed channel: fall back to polling.
        let (mut changes, _subscription) = match self.watch.subscribe_channel(
            WatchSpec {
                dir: dir.to_path_buf(),
                matcher: FileMatcher::Exact(path.clone()),
                debounce: Some(WATCH_DEBOUNCE),
            },
            16,
        ) {
            Ok((changes, subscription)) => (changes, Some(subscription)),
            Err(_) => (tokio::sync::mpsc::channel(1).1, None),
        };
        let size = tokio::fs::metadata(&path)
            .await
            .map(|meta| meta.len())
            .unwrap_or(0);
        let offset = match self.start {
            TailerStart::Live => size.saturating_sub(LIVE_LOOKBACK_BYTES),
            TailerStart::Resumed => 0,
        };
        let mut cursor = Cursor {
            offset,
            carry: Vec::new(),
            // Starting inside the file lands mid-line.
            skipping_line: offset > 0,
        };
        let mut watched = true;
        loop {
            if self.event_tx.upgrade().is_none() {
                return;
            }
            match scan(&path, &mut cursor, &self.task_id).await {
                Scan::Ended | Scan::Gone => return,
                Scan::Pending => {}
            }
            (self.idle)();
            if watched {
                tokio::select! {
                    change = changes.recv() => watched = change.is_some(),
                    () = tokio::time::sleep(SAFETY_REREAD) => {}
                }
            } else {
                tokio::time::sleep(FALLBACK_POLL).await;
            }
        }
    }
}

/// Reads what was appended since the cursor, one bounded chunk at a time, and
/// looks for the task's end. Stops at the first terminal notification without
/// reading the rest.
async fn scan(path: &Path, cursor: &mut Cursor, task_id: &str) -> Scan {
    let Ok(mut file) = tokio::fs::File::open(path).await else {
        return Scan::Gone;
    };
    if file.seek(SeekFrom::Start(cursor.offset)).await.is_err() {
        return Scan::Gone;
    }
    let mut buf = vec![0u8; READ_CHUNK_BYTES];
    let mut read_any = false;
    loop {
        let read = match file.read(&mut buf).await {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => return Scan::Gone,
        };
        read_any = true;
        cursor.offset += read as u64;
        if scan_chunk(cursor, &buf[..read], task_id) {
            return Scan::Ended;
        }
    }
    if !read_any {
        // Truncated or rewritten under us: start over.
        let len = tokio::fs::metadata(path)
            .await
            .map(|meta| meta.len())
            .unwrap_or(0);
        if len < cursor.offset {
            *cursor = Cursor {
                offset: 0,
                carry: Vec::new(),
                skipping_line: false,
            };
        }
    }
    Scan::Pending
}

/// Whether a line completed by `chunk` is the task's terminal notification.
/// Only the partial line at the end of the chunk is carried over.
fn scan_chunk(cursor: &mut Cursor, chunk: &[u8], task_id: &str) -> bool {
    let mut rest = chunk;
    while !rest.is_empty() {
        let Some(end) = rest.iter().position(|byte| *byte == b'\n') else {
            if !cursor.skipping_line {
                if cursor.carry.len() + rest.len() > MAX_LINE_BYTES {
                    cursor.carry.clear();
                    cursor.skipping_line = true;
                } else {
                    cursor.carry.extend_from_slice(rest);
                }
            }
            return false;
        };
        let (line, after) = (&rest[..end], &rest[end + 1..]);
        rest = after;
        if cursor.skipping_line {
            cursor.skipping_line = false;
            cursor.carry.clear();
            continue;
        }
        let ended = if cursor.carry.is_empty() {
            is_terminal_notification(&String::from_utf8_lossy(line), task_id)
        } else if cursor.carry.len() + line.len() > MAX_LINE_BYTES {
            false
        } else {
            cursor.carry.extend_from_slice(line);
            is_terminal_notification(&String::from_utf8_lossy(&cursor.carry), task_id)
        };
        cursor.carry.clear();
        if ended {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tokio::sync::mpsc::{Receiver, Sender, UnboundedReceiver};

    /// The shape of the entries Claude Code writes: a JSON line whose string
    /// holds the notification.
    fn notification_line(task_id: &str, status: Option<&str>) -> String {
        let status = status
            .map(|s| format!("<status>{s}</status>\\n"))
            .unwrap_or_default();
        format!(
            "{{\"type\":\"queue-operation\",\"operation\":\"enqueue\",\"content\":\"<task-notification>\\n<task-id>{task_id}</task-id>\\n<tool-use-id>toolu_1</tool-use-id>\\n{status}</task-notification>\"}}\n"
        )
    }

    /// A transcript entry of `kind` whose message carries `text`, the way an
    /// echo of a notification (a tool result, model text, an API dump) lands
    /// in the same file as the real record.
    fn entry_carrying(kind: &str, message: serde_json::Value) -> String {
        format!(
            "{}\n",
            serde_json::json!({"type": kind, "message": message})
        )
    }

    fn full_notification_text(task_id: &str) -> String {
        format!(
            "<task-notification>\n<task-id>{task_id}</task-id>\n<status>completed</status>\n</task-notification>"
        )
    }

    /// Only the records the harness writes itself end a task. The transcript
    /// also holds content the task id can leak into: a Read or `cat` of
    /// another transcript, a TaskOutput dump, model text.
    #[test]
    fn an_echo_of_a_notification_does_not_end_the_task() {
        let text = full_notification_text("t-1");
        let cases = [
            (
                "tool result echo",
                entry_carrying(
                    "user",
                    serde_json::json!({"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_1", "content": text}
                    ]}),
                ),
                false,
            ),
            (
                "model text",
                entry_carrying(
                    "assistant",
                    serde_json::json!({"role": "assistant", "content": [
                        {"type": "text", "text": text}
                    ]}),
                ),
                false,
            ),
            (
                "api dump",
                entry_carrying(
                    "api-request-blob",
                    serde_json::json!({"role": "user", "content": text}),
                ),
                false,
            ),
            (
                "user text that only mentions it",
                entry_carrying(
                    "user",
                    serde_json::json!({"role": "user", "content": format!("see {text}")}),
                ),
                false,
            ),
            (
                "queued user text that only mentions it",
                format!(
                    "{}\n",
                    serde_json::json!({"type": "queue-operation", "operation": "enqueue", "content": format!("what about {text}")})
                ),
                false,
            ),
            (
                "dequeue",
                format!(
                    "{}\n",
                    serde_json::json!({"type": "queue-operation", "operation": "dequeue", "content": text})
                ),
                false,
            ),
            (
                "the delivered user message",
                entry_carrying("user", serde_json::json!({"role": "user", "content": text})),
                true,
            ),
        ];
        for (name, line, want) in cases {
            assert_eq!(is_terminal_notification(&line, "t-1"), want, "{name}");
        }
    }

    #[test]
    fn only_a_terminal_notification_of_the_task_ends_it() {
        let cases = [
            ("completed", notification_line("t-1", Some("completed")), true),
            ("failed", notification_line("t-1", Some("failed")), true),
            ("stopped", notification_line("t-1", Some("stopped")), true),
            ("killed", notification_line("t-1", Some("killed")), true),
            ("still running", notification_line("t-1", Some("running")), false),
            ("monitor event, no status", notification_line("t-1", None), false),
            ("another task", notification_line("t-2", Some("completed")), false),
            (
                "not a notification",
                "{\"type\":\"user\",\"content\":\"<task-id>t-1</task-id><status>completed</status>\"}\n".to_string(),
                false,
            ),
        ];
        for (name, line, want) in cases {
            assert_eq!(is_terminal_notification(&line, "t-1"), want, "{name}");
        }
    }

    #[test]
    fn the_session_transcript_is_found_under_any_encoded_cwd() {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("projects/-some-encoded-cwd");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("sess-1.jsonl"), "").unwrap();

        assert_eq!(
            find_session_transcript(home.path(), "sess-1"),
            Some(dir.join("sess-1.jsonl"))
        );
        assert_eq!(find_session_transcript(home.path(), "sess-2"), None);
        for hostile in ["", "..", "../x", "a/b"] {
            assert_eq!(find_session_transcript(home.path(), hostile), None);
        }
    }

    struct Fixture {
        _home: tempfile::TempDir,
        home: PathBuf,
        transcript: PathBuf,
        active: Arc<Mutex<HashSet<String>>>,
        tx: Option<Sender<Event>>,
        rx: Receiver<Event>,
        idle: IdleHook,
        idle_rx: UnboundedReceiver<()>,
    }

    fn fixture(initial: &str) -> Fixture {
        let home = tempfile::tempdir().unwrap();
        let dir = home.path().join("projects/-enc");
        std::fs::create_dir_all(&dir).unwrap();
        let transcript = dir.join("sess.jsonl");
        std::fs::write(&transcript, initial).unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let (idle_tx, idle_rx) = tokio::sync::mpsc::unbounded_channel();
        Fixture {
            home: home.path().to_path_buf(),
            _home: home,
            transcript,
            active: Arc::default(),
            tx: Some(tx),
            rx,
            idle: Arc::new(move || {
                let _ = idle_tx.send(());
            }),
            idle_rx,
        }
    }

    impl Fixture {
        fn spawn_with(&self, watch: Arc<FileWatchService>, start: TailerStart) {
            TaskTailer {
                claude_home: Some(self.home.clone()),
                watch,
                task_id: "t-1".into(),
                session_id: "sess".into(),
                active: self.active.clone(),
                event_tx: self.tx.as_ref().expect("sender kept").downgrade(),
                start,
                idle: self.idle.clone(),
            }
            .spawn();
        }

        fn spawn(&self, start: TailerStart) {
            self.spawn_with(FileWatchService::new().unwrap(), start);
        }

        /// Waits until a tailer has scanned everything written so far and is
        /// waiting for the next change. A later append can then only be found
        /// by a wake, never by the initial catch-up. The deadline only bounds
        /// a failure.
        async fn parked(&mut self) {
            tokio::time::timeout(Duration::from_secs(10), self.idle_rx.recv())
                .await
                .expect("the tailer reached its wait")
                .expect("hook alive");
        }

        fn append(&self, text: &str) {
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&self.transcript)
                .unwrap();
            file.write_all(text.as_bytes()).unwrap();
        }

        /// The next event. The deadline only bounds a failure, and sits well
        /// below the safety re-read, so an event here was a wake.
        async fn next(&mut self) -> Event {
            tokio::time::timeout(Duration::from_secs(10), self.rx.recv())
                .await
                .expect("an event within the deadline")
                .expect("channel open")
        }

        /// Drops the connection's strong sender, as a lost worker does.
        fn drop_sender(&mut self) {
            self.tx = None;
        }

        async fn released(&self) {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !self.active.lock().unwrap().is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the tailer released its slot");
        }
    }

    fn finished_id(event: Event) -> String {
        match event {
            Event::BackgroundTaskFinished { task_id } => task_id,
            other => panic!("expected BackgroundTaskFinished, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_change_after_the_initial_scan_wakes_the_tailer() {
        let mut f = fixture("{\"type\":\"user\"}\n");
        f.spawn(TailerStart::Live);
        f.parked().await;
        // Another task's end first: the tailer keeps following.
        f.append(&notification_line("t-9", Some("completed")));
        f.append(&notification_line("t-1", Some("completed")));
        assert_eq!(finished_id(f.next().await), "t-1");
        f.released().await;
    }

    #[tokio::test]
    async fn without_a_watcher_a_later_append_is_found_by_the_fallback_poll() {
        let mut f = fixture("{\"type\":\"user\"}\n");
        f.spawn_with(FileWatchService::noop(), TailerStart::Live);
        // A no-op watcher hands back a closed channel, so the tailer switches
        // to polling right after its first checkpoint, and rescans once on
        // the way. Only the second checkpoint is after that switch: an append
        // here can no longer be caught by the rescan, only by a poll.
        f.parked().await;
        f.parked().await;
        f.append(&notification_line("t-1", Some("failed")));
        assert_eq!(finished_id(f.next().await), "t-1");
    }

    #[tokio::test]
    async fn a_notification_before_the_tailer_started_is_still_found() {
        let mut f = fixture(&notification_line("t-1", Some("failed")));
        f.spawn(TailerStart::Live);
        assert_eq!(finished_id(f.next().await), "t-1");
    }

    #[tokio::test]
    async fn a_resumed_tailer_scans_the_whole_transcript() {
        // The notification sits beyond the live lookback window and many
        // chunks in.
        let mut initial = "x".repeat(READ_CHUNK_BYTES * 3);
        initial.push('\n');
        initial.push_str(&notification_line("t-1", Some("killed")));
        let mut resumed = fixture(&initial);
        resumed.spawn(TailerStart::Resumed);
        assert_eq!(finished_id(resumed.next().await), "t-1");
    }

    #[tokio::test]
    async fn a_transcript_that_cannot_be_found_ends_tracking() {
        let home = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        TaskTailer {
            claude_home: Some(home.path().to_path_buf()),
            watch: FileWatchService::noop(),
            task_id: "t-1".into(),
            session_id: "missing".into(),
            active: Arc::default(),
            event_tx: tx.downgrade(),
            start: TailerStart::Live,
            idle: Arc::new(|| {}),
        }
        .spawn();
        let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("an event within the deadline")
            .expect("channel open");
        assert_eq!(finished_id(event), "t-1");
    }

    /// The start is published before the tailer exists, so no way out of
    /// tracking may skip the finish.
    #[tokio::test]
    async fn an_unresolvable_claude_home_ends_tracking() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        TaskTailer {
            claude_home: None,
            watch: FileWatchService::noop(),
            task_id: "t-1".into(),
            session_id: "sess".into(),
            active: Arc::default(),
            event_tx: tx.downgrade(),
            start: TailerStart::Live,
            idle: Arc::new(|| {}),
        }
        .spawn();
        let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("an event within the deadline")
            .expect("channel open");
        assert_eq!(finished_id(event), "t-1");
    }

    #[tokio::test]
    async fn a_poisoned_in_flight_set_does_not_strand_the_task() {
        let mut f = fixture(&notification_line("t-1", Some("completed")));
        let active = f.active.clone();
        // A tailer that panicked while holding the set's lock.
        let _ = std::thread::spawn(move || {
            let _held = active.lock().unwrap();
            panic!("poison the in-flight set");
        })
        .join();
        assert!(f.active.is_poisoned());

        f.spawn(TailerStart::Live);
        assert_eq!(finished_id(f.next().await), "t-1");
    }

    #[tokio::test]
    async fn a_task_already_tracked_is_not_tailed_twice() {
        let mut f = fixture("");
        f.spawn(TailerStart::Live);
        f.spawn(TailerStart::Live);
        f.parked().await;
        f.append(&notification_line("t-1", Some("completed")));
        assert_eq!(finished_id(f.next().await), "t-1");
        f.released().await;
        // One tailer, one finish: nothing else is queued behind it.
        f.drop_sender();
        let rest = tokio::time::timeout(Duration::from_secs(5), f.rx.recv())
            .await
            .expect("channel closes once the tailers are gone");
        assert!(rest.is_none(), "a second finish was published: {rest:?}");
    }

    /// A tailer waiting on a task that never reports must not keep the
    /// worker's event channel open after the connection is gone, or the
    /// drain never sees EOF and recovery never starts.
    #[tokio::test]
    async fn a_pending_tailer_does_not_hold_the_event_channel_open() {
        let mut f = fixture("{\"type\":\"user\"}\n");
        f.spawn(TailerStart::Live);
        f.parked().await;

        f.drop_sender();
        let closed = tokio::time::timeout(Duration::from_secs(5), f.rx.recv())
            .await
            .expect("the channel closes with the connection, tailer or not");
        assert!(closed.is_none());

        // Its next wake finds no connection and ends it.
        f.append("{\"type\":\"user\"}\n");
        f.released().await;
    }

    fn write_transcript(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sess.jsonl");
        std::fs::write(&path, contents).unwrap();
        (dir, path)
    }

    fn start_of_transcript() -> Cursor {
        Cursor {
            offset: 0,
            carry: Vec::new(),
            skipping_line: false,
        }
    }

    #[tokio::test]
    async fn an_early_notification_stops_the_read_before_the_large_suffix() {
        let mut contents = notification_line("t-1", Some("completed"));
        contents.push_str(&"filler\n".repeat(READ_CHUNK_BYTES * 4 / 7));
        let (_dir, path) = write_transcript(&contents);
        assert!(contents.len() > READ_CHUNK_BYTES * 3);

        let mut cursor = start_of_transcript();
        assert!(matches!(scan(&path, &mut cursor, "t-1").await, Scan::Ended));
        assert!(
            cursor.offset <= READ_CHUNK_BYTES as u64,
            "read {} bytes for a notification in the first line",
            cursor.offset
        );
    }

    #[tokio::test]
    async fn a_notification_straddling_a_chunk_boundary_is_found() {
        // The line starts 50 bytes before the boundary and runs past it.
        let mut contents = "x".repeat(READ_CHUNK_BYTES - 51);
        contents.push('\n');
        contents.push_str(&notification_line("t-1", Some("failed")));
        contents.push_str("{\"type\":\"user\"}\n");
        let (_dir, path) = write_transcript(&contents);

        let mut cursor = start_of_transcript();
        assert!(matches!(scan(&path, &mut cursor, "t-1").await, Scan::Ended));
        assert!(
            cursor.offset > READ_CHUNK_BYTES as u64,
            "the second chunk was needed"
        );
    }

    #[tokio::test]
    async fn an_overlong_line_is_skipped_without_being_buffered() {
        let mut contents = "y".repeat(MAX_LINE_BYTES + 10);
        contents.push('\n');
        contents.push_str(&notification_line("t-1", Some("completed")));
        let (_dir, path) = write_transcript(&contents);

        let mut cursor = start_of_transcript();
        assert!(matches!(scan(&path, &mut cursor, "t-1").await, Scan::Ended));
        assert!(cursor.carry.len() <= MAX_LINE_BYTES);
    }
}
