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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use crate::acp::event_sender::WeakEventSender;
use std::io::{Read, Seek, SeekFrom};
use tokio_util::sync::CancellationToken;

use chrono::{DateTime, Utc};

use crate::acp::background_agent::{ActiveAgents, TailerStart};
use crate::acp::establishment::NativeResourceAuthority;
use crate::acp::state::{BackgroundTaskLaunch, Event};
use crate::acp::workflow_journal::WorkflowJournal;
use crate::file_watch::{FileMatcher, FileWatchService, WatchSpec};

/// Coalesces the burst of writes one transcript entry causes.
pub(crate) const WATCH_DEBOUNCE: Duration = Duration::from_millis(50);
/// A kernel watcher can drop events (queue overflow), so the transcript is
/// also re-read this often. Rare enough not to be polling.
pub(crate) const SAFETY_REREAD: Duration = Duration::from_secs(30);
/// Used only when no watcher is available (file watching off or failed).
pub(crate) const FALLBACK_POLL: Duration = Duration::from_millis(500);
/// A live launch also reads this much of the transcript's tail, for a
/// notification that landed before the tailer started.
const LIVE_LOOKBACK_BYTES: u64 = 256 * 1024;
/// The transcript is read and scanned in chunks of this size, so a long
/// conversation is never held in memory.
pub(crate) const READ_CHUNK_BYTES: usize = 256 * 1024;
/// A single line longer than this is skipped, not buffered.
pub(crate) const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;
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
/// Returns when it ended: the entry's own time, so a resumed read does not
/// count daemon downtime.
fn terminal_notification(line: &str, task_id: &str, tool_call_id: &str) -> Option<DateTime<Utc>> {
    if !line.contains(task_id) {
        return None;
    }
    let entry = serde_json::from_str::<serde_json::Value>(line).ok()?;
    let content = match entry.get("type").and_then(serde_json::Value::as_str) {
        Some("queue-operation")
            if entry.get("operation").and_then(serde_json::Value::as_str) == Some("enqueue") =>
        {
            entry.get("content")
        }
        Some("user") => entry.pointer("/message/content"),
        _ => None,
    };
    let content = content?;
    let matches = |text: &str| {
        let Some(body) = text.trim_start().strip_prefix("<task-notification>") else {
            return false;
        };
        let Some((body, _)) = body.split_once("</task-notification>") else {
            return false;
        };
        let header = body
            .split_once("<result>")
            .map_or(body, |(header, _)| header);
        let header = header
            .split_once("<summary>")
            .map_or(header, |(header, _)| header);
        let field = |open, close| {
            header
                .split_once(open)?
                .1
                .split_once(close)
                .map(|(value, _)| value.trim())
        };
        field("<task-id>", "</task-id>") == Some(task_id)
            && field("<tool-use-id>", "</tool-use-id>") == Some(tool_call_id)
            && field("<status>", "</status>")
                .is_some_and(|status| TERMINAL_STATUSES.contains(&status))
    };
    let ended = content.as_str().is_some_and(matches)
        || content.as_array().is_some_and(|parts| {
            parts.iter().any(|part| {
                part.get("type").and_then(serde_json::Value::as_str) == Some("text")
                    && part
                        .get("text")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(matches)
            })
        });
    ended.then(|| {
        entry
            .get("timestamp")
            .and_then(serde_json::Value::as_str)
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
            .map_or_else(Utc::now, |at| at.with_timezone(&Utc))
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

/// Shared within the application's runtime; backend failure uses polling.
#[cfg(not(test))]
fn shared_watch() -> Arc<FileWatchService> {
    static WATCH: OnceLock<Arc<FileWatchService>> = OnceLock::new();
    WATCH
        .get_or_init(|| FileWatchService::new().unwrap_or_else(|_| FileWatchService::noop()))
        .clone()
}

#[cfg(test)]
fn shared_watch() -> Arc<FileWatchService> {
    static WATCHES: OnceLock<
        Mutex<HashMap<tokio::runtime::Id, std::sync::Weak<FileWatchService>>>,
    > = OnceLock::new();
    let runtime = tokio::runtime::Handle::current().id();
    let mut watches = WATCHES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    watches.retain(|_, watch| watch.strong_count() != 0);
    if let Some(watch) = watches.get(&runtime).and_then(std::sync::Weak::upgrade) {
        return watch;
    }
    let watch = FileWatchService::new().unwrap_or_else(|_| FileWatchService::noop());
    watches.insert(runtime, Arc::downgrade(&watch));
    watch
}

#[derive(Default)]
pub(crate) struct ActiveTasks {
    owners: Mutex<HashMap<String, Arc<TaskOwner>>>,
    publication: tokio::sync::Mutex<()>,
}

struct TaskOwner {
    tool_call_id: Option<String>,
    cancellation: CancellationToken,
    authority: Arc<NativeResourceAuthority>,
}

impl ActiveTasks {
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Arc<TaskOwner>>> {
        self.owners.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }
    pub(crate) fn clear(&self) {
        let mut owners = self.lock();
        for owner in owners.values() {
            owner.cancellation.cancel();
        }
        owners.clear();
    }
}
#[derive(Clone)]
pub(crate) struct TaskReadOwner {
    active: Arc<ActiveTasks>,
    task_id: Arc<str>,
    owner: Arc<TaskOwner>,
}

impl std::fmt::Debug for TaskReadOwner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TaskReadOwner")
            .field("task_id", &self.task_id)
            .finish_non_exhaustive()
    }
}

impl TaskReadOwner {
    fn check(&self, owners: &HashMap<String, Arc<TaskOwner>>) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.owner.cancellation.is_cancelled()
                && owners
                    .get(self.task_id.as_ref())
                    .is_some_and(|current| Arc::ptr_eq(current, &self.owner)),
            crate::acp::establishment::EstablishmentRefusal::Superseded
        );
        Ok(())
    }

    pub(crate) fn with_owner<T>(
        &self,
        effect: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let owners = self.active.lock();
        self.check(&owners)?;
        effect()
    }

    pub(crate) fn authority(&self) -> &Arc<NativeResourceAuthority> {
        &self.owner.authority
    }

    pub(crate) fn effect<T>(
        &self,
        effect: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        self.child_effect(&self.owner.authority, effect)
    }

    pub(crate) fn child_effect<T>(
        &self,
        authority: &NativeResourceAuthority,
        effect: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        authority.with_read(|| self.with_owner(effect))
    }

    pub(crate) async fn blocking<T, F>(&self, effect: F) -> anyhow::Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> anyhow::Result<T> + Send + 'static,
    {
        let access = self.clone();
        tokio::task::spawn_blocking(move || access.effect(effect)).await?
    }

    async fn publish_with(
        &self,
        events: crate::acp::event_sender::EventSender,
        event: Event,
    ) -> bool {
        let Ok(permit) = events.reserve().await else {
            return false;
        };
        self.with_owner(|| {
            permit.send(event);
            Ok(())
        })
        .is_ok()
    }

    pub(crate) async fn publish(&self, events: &WeakEventSender, event: Event) -> bool {
        let Some(events) = events.upgrade() else {
            return false;
        };
        self.publish_with(self.owner.authority.progress_sender(&events), event)
            .await
    }

    pub(crate) async fn publish_resource(
        &self,
        events: &WeakEventSender,
        event: Event,
        witness: Option<&crate::acp::establishment::NativeResourceWitness>,
    ) -> bool {
        let Some(events) = events.upgrade() else {
            return false;
        };
        let events = match witness {
            Some(witness) => events.with_resource_reference(witness),
            None => events.with_metadata(crate::acp::event_sender::EventMetadata::history()),
        };
        self.publish_with(events, event).await
    }

    async fn publish_start(&self, events: &WeakEventSender, event: Event, qualified: bool) -> bool {
        let Some(events) = events.upgrade() else {
            return false;
        };
        let events = if qualified {
            self.owner.authority.start_sender(&events)
        } else {
            events.with_metadata(crate::acp::event_sender::EventMetadata::history())
        };
        self.publish_with(events, event).await
    }
}

struct ActiveGuard {
    active: Arc<ActiveTasks>,
    task_id: String,
    owner: Arc<TaskOwner>,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.owner.cancellation.cancel();
        let mut owners = self.active.lock();
        if owners
            .get(&self.task_id)
            .is_some_and(|current| Arc::ptr_eq(current, &self.owner))
        {
            owners.remove(&self.task_id);
        }
    }
}

/// Follows one exact launch without retaining the worker's event channel.
pub(crate) struct TaskTailer {
    pub claude_home: Option<PathBuf>,
    pub watch: Arc<FileWatchService>,
    pub task_id: String,
    pub tool_call_id: Option<String>,
    pub session_id: String,
    pub active: Arc<ActiveTasks>,
    pub event_tx: WeakEventSender,
    pub start: TailerStart,
    pub authority: Arc<NativeResourceAuthority>,
    pub idle: IdleHook,
    /// Set for a `Workflow` run, whose agents are followed alongside.
    pub workflow: Option<WorkflowJournal>,
}

impl TaskTailer {
    pub(crate) async fn spawn(
        mut self,
        start_event: Option<Event>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let selected = match self.authority.selector() {
            crate::acp::establishment::NativeResourceSelector::Task {
                task_id,
                tool_call_id,
                session_id,
                claude_home,
                workflow,
            } => {
                task_id == &self.task_id
                    && tool_call_id == &self.tool_call_id
                    && session_id == &self.session_id
                    && self.claude_home.as_ref() == Some(claude_home)
                    && match (workflow, self.workflow.as_ref()) {
                        (None, None) => true,
                        (Some(selected), Some(journal)) => {
                            selected.run_id == journal.launch.run_id
                                && selected.transcript_dir == journal.launch.transcript_dir
                                && journal.claude_home == *claude_home
                                && Some(journal.tool_call_id.as_str())
                                    == self.tool_call_id.as_deref()
                        }
                        _ => false,
                    }
            }
            _ => false,
        };
        if !selected {
            if let (Some(tx), Some(event)) = (self.event_tx.upgrade(), start_event) {
                let _ = tx
                    .with_metadata(crate::acp::event_sender::EventMetadata::history())
                    .send(event)
                    .await;
            }
            return None;
        }
        let publication = self.active.publication.lock().await;
        let owner = self.authority.admit_tracking(|| {
            let mut owners = self.active.lock();
            if owners.get(&self.task_id).is_some_and(|current| {
                current.tool_call_id == self.tool_call_id
                    && current.authority.same_resource(&self.authority)
                    && !current.cancellation.is_cancelled()
            }) {
                return Ok(None);
            }
            let allocation = self
                .authority
                .witness()
                .map_or_else(uuid::Uuid::new_v4, |witness| witness.allocation);
            let owner = Arc::new(TaskOwner {
                tool_call_id: self.tool_call_id.clone(),
                cancellation: CancellationToken::new(),
                authority: self.authority.for_tracking_allocation(allocation)?,
            });
            if let Some(previous) = owners.insert(self.task_id.clone(), Arc::clone(&owner)) {
                previous.cancellation.cancel();
            }
            Ok(Some(owner))
        });
        let owner = match owner {
            Ok(Some(owner)) => owner,
            _ => {
                if let (Some(tx), Some(event)) = (self.event_tx.upgrade(), start_event) {
                    let _ = tx
                        .with_metadata(crate::acp::event_sender::EventMetadata::history())
                        .send(event)
                        .await;
                }
                return None;
            }
        };
        let guard = ActiveGuard {
            active: Arc::clone(&self.active),
            task_id: self.task_id.clone(),
            owner: Arc::clone(&owner),
        };
        let access = TaskReadOwner {
            active: Arc::clone(&self.active),
            task_id: self.task_id.as_str().into(),
            owner: Arc::clone(&owner),
        };
        let discovered = match (&self.claude_home, self.tool_call_id.as_ref()) {
            (Some(home), Some(_)) => {
                let home = home.clone();
                let session = self.session_id.clone();
                access
                    .blocking(move || Ok(find_session_transcript(&home, &session)))
                    .await
            }
            _ => Err(crate::acp::establishment::EstablishmentRefusal::Unconfirmed.into()),
        };
        if let Some(event) = start_event {
            if !access
                .publish_start(&self.event_tx, event, discovered.is_ok())
                .await
            {
                return None;
            }
        }
        let Ok(path) = discovered else {
            return None;
        };
        drop(publication);
        Some(tokio::spawn(async move {
            let _guard = guard;
            let ended = CancellationToken::new();
            let journal = self.workflow.take().map(|journal| {
                tokio::spawn(journal.run(ended.clone(), owner.cancellation.clone(), access.clone()))
            });
            let ended_at = tokio::select! {
                () = owner.cancellation.cancelled() => return,
                ended_at = async {
                    match path {
                        Some(path) => self.follow(path, &access).await,
                        None => None,
                    }
                } => ended_at,
            };
            ended.cancel();
            if let Some(journal) = journal {
                let _ = journal.await;
            }
            let _publication = self.active.publication.lock().await;
            let _ = access
                .publish(
                    &self.event_tx,
                    Event::BackgroundTaskFinished {
                        task_id: self.task_id.clone(),
                        ended_at,
                    },
                )
                .await;
        }))
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn spawn_task_tailer(
    launch: BackgroundTaskLaunch,
    session_id: String,
    claude_home: Option<PathBuf>,
    active: Arc<ActiveTasks>,
    bg_agents: Arc<ActiveAgents>,
    event_tx: WeakEventSender,
    start: TailerStart,
    start_event: Option<Event>,
    authority: Arc<NativeResourceAuthority>,
) {
    let workflow = match (launch.workflow, &launch.tool_call_id, &claude_home) {
        (Some(workflow), Some(tool_call_id), Some(home)) => Some(WorkflowJournal {
            launch: workflow,
            tool_call_id: tool_call_id.clone(),
            claude_home: home.clone(),
            known: launch.workflow_agents,
            event_tx: event_tx.clone(),
            bg_agents,
            watch: shared_watch(),
        }),
        _ => None,
    };
    let _ = TaskTailer {
        claude_home,
        watch: shared_watch(),
        task_id: launch.task_id,
        tool_call_id: launch.tool_call_id,
        session_id,
        active,
        event_tx,
        start,
        authority,
        idle: Arc::new(|| {}),
        workflow,
    }
    .spawn(start_event)
    .await;
}

/// Where a transcript has been read to, and the partial line after it.
#[derive(Default)]
struct Cursor {
    offset: u64,
    carry: Vec<u8>,
    /// Inside a line that began before the read window, or was too long.
    skipping_line: bool,
}

enum Scan {
    Ended(DateTime<Utc>),
    /// Nothing ended the task yet.
    Pending,
    /// The transcript cannot be read any more.
    Gone,
}

impl TaskTailer {
    /// Returns when the task ended, or `None` once tracking was given up.
    async fn follow(&self, path: PathBuf, owner: &TaskReadOwner) -> Option<DateTime<Utc>> {
        self.tool_call_id.as_ref()?;
        let path = owner
            .blocking(move || Ok(std::fs::canonicalize(&path).unwrap_or(path)))
            .await
            .ok()?;
        let path = Arc::new(path);
        let dir = path.parent()?;
        let subscription = owner
            .effect(|| {
                Ok(self.watch.subscribe_channel(
                    WatchSpec {
                        dir: dir.to_path_buf(),
                        matcher: FileMatcher::Exact(path.as_ref().clone()),
                        debounce: Some(WATCH_DEBOUNCE),
                    },
                    16,
                ))
            })
            .ok()?;
        let (mut changes, _subscription) = match subscription {
            Ok((changes, subscription)) => (changes, Some(subscription)),
            Err(_) => (tokio::sync::mpsc::channel(1).1, None),
        };
        let metadata_path = Arc::clone(&path);
        let size = owner
            .blocking(move || {
                Ok(std::fs::metadata(metadata_path.as_ref())
                    .map(|meta| meta.len())
                    .unwrap_or(0))
            })
            .await
            .ok()?;
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
            self.event_tx.upgrade()?;
            match scan(Arc::clone(&path), &mut cursor, owner).await.ok()? {
                Scan::Ended(at) => return Some(at),
                Scan::Gone => return None,
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

async fn scan(
    path: Arc<PathBuf>,
    cursor: &mut Cursor,
    owner: &TaskReadOwner,
) -> anyhow::Result<Scan> {
    let access = owner.clone();
    let mut owned = std::mem::take(cursor);
    let (result, updated) = tokio::task::spawn_blocking(move || {
        access.effect(|| {
            let tool_call = access
                .owner
                .tool_call_id
                .as_deref()
                .ok_or(crate::acp::establishment::EstablishmentRefusal::Unconfirmed)?;
            let result = scan_file(path.as_ref(), &mut owned, &access.task_id, tool_call);
            Ok((result, owned))
        })
    })
    .await??;
    *cursor = updated;
    Ok(result)
}

fn scan_file(path: &Path, cursor: &mut Cursor, task_id: &str, tool_call_id: &str) -> Scan {
    let Ok(mut file) = std::fs::File::open(path) else {
        return Scan::Gone;
    };
    if file.seek(SeekFrom::Start(cursor.offset)).is_err() {
        return Scan::Gone;
    }
    let mut buf = vec![0u8; READ_CHUNK_BYTES];
    let mut read_any = false;
    loop {
        let read = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => read,
            Err(_) => return Scan::Gone,
        };
        read_any = true;
        cursor.offset += read as u64;
        if let Some(at) = scan_chunk(cursor, &buf[..read], task_id, tool_call_id) {
            return Scan::Ended(at);
        }
    }
    if !read_any {
        // Truncated or rewritten under us: start over.
        let len = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
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

/// When the task ended, if a line completed by `chunk` is its terminal notification.
/// Only the partial line at the end of the chunk is carried over.
fn scan_chunk(
    cursor: &mut Cursor,
    chunk: &[u8],
    task_id: &str,
    tool_call_id: &str,
) -> Option<DateTime<Utc>> {
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
            return None;
        };
        let (line, after) = (&rest[..end], &rest[end + 1..]);
        rest = after;
        if cursor.skipping_line {
            cursor.skipping_line = false;
            cursor.carry.clear();
            continue;
        }
        let ended = if cursor.carry.is_empty() {
            terminal_notification(&String::from_utf8_lossy(line), task_id, tool_call_id)
        } else if cursor.carry.len() + line.len() > MAX_LINE_BYTES {
            None
        } else {
            cursor.carry.extend_from_slice(line);
            terminal_notification(
                &String::from_utf8_lossy(&cursor.carry),
                task_id,
                tool_call_id,
            )
        };
        cursor.carry.clear();
        if ended.is_some() {
            return ended;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::event_sender::{AcpEventData, SdkEvent};
    use crate::acp::supervisor::test_support::managed::{
        with_managed, ManagedClient, ManagedScope, NativePeer,
    };
    use std::io::Write;
    use tokio::sync::mpsc::{Receiver, Sender, UnboundedReceiver};

    const NOTIFIED_AT: &str = "2026-01-01T00:00:05Z";

    /// The shape of the entries Claude Code writes: a JSON line whose string
    /// holds the notification.
    fn notification_line(task_id: &str, status: Option<&str>) -> String {
        let status = status
            .map(|s| format!("<status>{s}</status>\\n"))
            .unwrap_or_default();
        format!(
            "{{\"type\":\"queue-operation\",\"operation\":\"enqueue\",\"timestamp\":\"{NOTIFIED_AT}\",\"content\":\"<task-notification>\\n<task-id>{task_id}</task-id>\\n<tool-use-id>toolu_1</tool-use-id>\\n{status}</task-notification>\"}}\n"
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
            "<task-notification>\n<task-id>{task_id}</task-id>\n<tool-use-id>toolu_1</tool-use-id>\n<status>completed</status>\n</task-notification>"
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
            assert_eq!(
                terminal_notification(&line, "t-1", "toolu_1").is_some(),
                want,
                "{name}"
            );
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
                "earlier incarnation of the same task id",
                notification_line("t-1", Some("completed")).replace("toolu_1", "toolu_old"),
                false,
            ),
            (
                "quoted task identity in another task's result",
                entry_carrying("user", serde_json::json!({"content": format!(
                    "<task-notification><task-id>other</task-id><tool-use-id>other</tool-use-id><status>completed</status><result>{}</result></task-notification>",
                    full_notification_text("t-1")
                )})),
                false,
            ),
            (
                "quoted terminal status inside a running task's result",
                notification_line("t-1", Some("running")).replace(
                    "</task-notification>",
                    "<result><status>completed</status></result></task-notification>",
                ),
                false,
            ),
            (
                "completion without captured tool identity",
                notification_line("t-1", Some("completed")).replace("<tool-use-id>toolu_1</tool-use-id>\\n", ""),
                false,
            ),
            (
                "summary cannot supply missing launch identity",
                entry_carrying("user", serde_json::json!({"content":
                    "<task-notification><task-id>t-1</task-id><status>completed</status><summary>quoted <tool-use-id>toolu_1</tool-use-id></summary></task-notification>"
                })),
                false,
            ),
            (
                "summary cannot supply terminal status",
                notification_line("t-1", None).replace(
                    "</task-notification>",
                    "<summary><status>completed</status></summary></task-notification>",
                ),
                false,
            ),
            (
                "not a notification",
                "{\"type\":\"user\",\"content\":\"<task-id>t-1</task-id><status>completed</status>\"}\n".to_string(),
                false,
            ),
        ];
        for (name, line, want) in cases {
            assert_eq!(
                terminal_notification(&line, "t-1", "toolu_1").is_some(),
                want,
                "{name}"
            );
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
        _managed: ManagedClient,
        _peer: NativePeer,
        authority: Arc<NativeResourceAuthority>,
        prior_authority: Option<Arc<NativeResourceAuthority>>,
        workflow: Option<(
            crate::acp::state::WorkflowLaunch,
            crate::acp::state::WorkflowAgents,
        )>,
        session_id: String,
        home: PathBuf,
        transcript: PathBuf,
        active: Arc<ActiveTasks>,
        tx: Option<Sender<SdkEvent>>,
        rx: Receiver<SdkEvent>,
        idle: IdleHook,
        idle_rx: UnboundedReceiver<()>,
    }

    enum FixtureSource<'a> {
        Bash(Option<&'a str>),
        Workflow,
    }

    async fn fixture(scope: &mut ManagedScope, initial: &str) -> Fixture {
        fixture_with_prior(scope, initial, None).await
    }

    async fn fixture_with_prior(
        scope: &mut ManagedScope,
        initial: &str,
        prior_tool: Option<&str>,
    ) -> Fixture {
        fixture_for(scope, initial, FixtureSource::Bash(prior_tool)).await
    }

    async fn fixture_for(
        scope: &mut ManagedScope,
        initial: &str,
        source: FixtureSource<'_>,
    ) -> Fixture {
        let prior_tool = match &source {
            FixtureSource::Bash(prior) => *prior,
            FixtureSource::Workflow => None,
        };
        let mut native = super::resource_test_support::NativeBootstrap::new(scope).await;
        let home = native.home.clone();
        let session_id = native.session_id.clone();
        let transcript = native.transcript.clone();
        let receipt = Arc::clone(&native.receipt);
        let workflow = if matches!(source, FixtureSource::Workflow) {
            let workflow_dir = home.join("projects/-enc/subagents/workflows/wf_1");
            std::fs::create_dir_all(&workflow_dir).unwrap();
            let workflow_dir = std::fs::canonicalize(workflow_dir).unwrap();
            std::fs::write(workflow_dir.join("agent-a1.jsonl"), "").unwrap();
            std::fs::write(workflow_dir.join("journal.jsonl"),
                "{\"type\":\"started\",\"agentId\":\"a1\",\"label\":\"draft:one\",\"phase\":\"Draft\",\"key\":\"k1\"}\n").unwrap();
            native
                .tool(
                    scope,
                    "toolu_1",
                    "Workflow",
                    serde_json::json!({
                        "status":"async_launched","taskId":"t-1","runId":"wf_1",
                        "workflowName":"drafts","transcriptDir":workflow_dir
                    }),
                )
                .await;
            tokio::time::timeout(Duration::from_secs(10), async {
                let (mut parent, mut child) = (false, false);
                while !parent || !child {
                    match native
                        .managed
                        .recv()
                        .await
                        .expect("original Native observer remains open")
                    {
                        Event::BackgroundTaskStarted {
                            task_id,
                            tool_call_id: Some(tool),
                            workflow: Some(run),
                            ..
                        } if task_id == "t-1" && tool == "toolu_1" && run.run_id == "wf_1" => {
                            parent = true
                        }
                        Event::BackgroundAgentLaunched {
                            agent_id,
                            tool_call_id,
                            workflow: Some(run),
                            ..
                        } if agent_id == "a1"
                            && tool_call_id == "toolu_1"
                            && run.run_id == "wf_1" =>
                        {
                            child = true
                        }
                        Event::BackgroundTaskFinished { .. }
                        | Event::BackgroundAgentCompleted { .. } => {
                            panic!("bootstrap tracking ended before hydration")
                        }
                        _ => {}
                    }
                }
            })
            .await
            .expect("real workflow parent and child allocations are published");
            let launch = scope
                .state
                .acp_event_store
                .unresolved_background_task_launches(&native.managed.id)
                .unwrap()
                .into_iter()
                .find(|launch| {
                    launch.task_id == "t-1" && launch.tool_call_id.as_deref() == Some("toolu_1")
                })
                .unwrap();
            let parent = launch.witness.as_ref().expect("actual parent allocation");
            let child = launch
                .workflow_agents
                .resources
                .get("a1")
                .expect("actual child allocation");
            assert!(matches!(child.selector.as_ref(),
                crate::acp::establishment::NativeResourceSelector::WorkflowChild {
                    parent_allocation, run_id, task_id, agent_id, output_file,
                    source: crate::acp::establishment::NativeResourceLocation::Host,
                } if *parent_allocation == parent.allocation && run_id == "wf_1" && task_id == "t-1"
                    && agent_id == "a1" && output_file == workflow_dir.join("agent-a1.jsonl").to_str().unwrap()));
            Some((launch.workflow.unwrap(), launch.workflow_agents))
        } else {
            for tool in prior_tool.into_iter().chain(std::iter::once("toolu_1")) {
                native
                    .tool(
                        scope,
                        tool,
                        "Bash",
                        serde_json::json!({"backgroundTaskId":"t-1"}),
                    )
                    .await;
                tokio::time::timeout(Duration::from_secs(10), async {
                    while !matches!(native.managed.recv().await,
                        Some(Event::BackgroundTaskStarted { task_id, tool_call_id: Some(actual), .. })
                            if task_id == "t-1" && actual == tool)
                    {}
                }).await.expect("actual Native task allocation is published");
            }
            None
        };
        let mut witness = None;
        let mut prior_witness = None;
        for stored in scope
            .state
            .acp_event_store
            .replay_from(&native.managed.id, 0)
            .unwrap()
        {
            if let Event::BackgroundTaskStarted {
                task_id,
                tool_call_id: Some(tool),
                ..
            } = stored.event.as_ref()
            {
                if task_id != "t-1" {
                    continue;
                }
                let Some(allocated) = stored.metadata.resource_witness else {
                    assert_eq!(
                        stored.metadata.scope,
                        crate::acp::event_sender::EventScope::HistoryOnly
                    );
                    assert!(stored.metadata.resource_ref.is_none());
                    continue;
                };
                assert_eq!(stored.metadata.resource_ref, Some(allocated.allocation));
                if tool == "toolu_1" {
                    assert!(witness.replace(allocated).is_none());
                } else if Some(tool.as_str()) == prior_tool {
                    assert!(prior_witness.replace(allocated).is_none());
                }
            }
        }
        native.quiesce(scope).await;
        let managed = native.managed;
        let peer = native.peer;
        let authority = NativeResourceAuthority::adopt(
            witness.expect("original allocated witness"),
            receipt.as_ref(),
        )
        .unwrap();
        let prior_authority = prior_witness
            .map(|witness| NativeResourceAuthority::adopt(witness, receipt.as_ref()).unwrap());
        assert_eq!(prior_authority.is_some(), prior_tool.is_some());
        if let Some(prior) = &prior_authority {
            assert!(matches!(prior.selector(),
                crate::acp::establishment::NativeResourceSelector::Task {
                    task_id, tool_call_id: Some(tool), session_id: native, claude_home, workflow: None,
                } if task_id == "t-1" && Some(tool.as_str()) == prior_tool
                    && native == &session_id && claude_home == home.as_path()));
            assert_ne!(
                prior.witness().unwrap().allocation,
                authority.witness().unwrap().allocation
            );
        }
        assert!(matches!(authority.selector(),
            crate::acp::establishment::NativeResourceSelector::Task {
                task_id, tool_call_id: Some(tool), session_id: native, claude_home, workflow: selected,
            } if task_id == "t-1" && tool == "toolu_1" && native == &session_id && claude_home == home.as_path()
                && selected.as_ref().map(|run| run.run_id.as_str()) == workflow.as_ref().map(|(run, _)| run.run_id.as_str())));
        std::fs::write(&transcript, initial).unwrap();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let (idle_tx, idle_rx) = tokio::sync::mpsc::unbounded_channel();
        Fixture {
            home,
            _managed: managed,
            _peer: peer,
            authority,
            prior_authority,
            workflow,
            session_id,
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
        fn tailer(&self, watch: Arc<FileWatchService>, start: TailerStart) -> TaskTailer {
            TaskTailer {
                tool_call_id: Some("toolu_1".into()),
                claude_home: Some(self.home.clone()),
                watch: Arc::clone(&watch),
                task_id: "t-1".into(),
                session_id: self.session_id.clone(),
                active: self.active.clone(),
                event_tx: self.tx.as_ref().expect("sender kept").downgrade().into(),
                start,
                authority: Arc::clone(&self.authority),
                idle: self.idle.clone(),
                workflow: self
                    .workflow
                    .as_ref()
                    .map(|(launch, known)| WorkflowJournal {
                        launch: launch.clone(),
                        known: known.clone(),
                        tool_call_id: "toolu_1".into(),
                        claude_home: self.home.clone(),
                        bg_agents: Arc::default(),
                        watch: Arc::clone(&watch),
                        event_tx: self.tx.as_ref().expect("sender kept").downgrade().into(),
                    }),
            }
        }

        async fn spawn_with(&self, watch: Arc<FileWatchService>, start: TailerStart) {
            drop(
                super::resource_test_support::wait_for_tracking(async || {
                    self.tailer(Arc::clone(&watch), start).spawn(None).await
                })
                .await,
            );
        }

        async fn spawn(&self, start: TailerStart) {
            self.spawn_with(FileWatchService::new().unwrap(), start)
                .await;
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
        async fn next(&mut self) -> AcpEventData {
            match tokio::time::timeout(Duration::from_secs(10), self.rx.recv())
                .await
                .expect("an event within the deadline")
                .expect("channel open")
            {
                SdkEvent::Data(data) => data,
                other => panic!("unexpected control event: {other:?}"),
            }
        }

        /// Drops the connection's strong sender, as a lost worker does.
        fn drop_sender(&mut self) {
            self.tx = None;
        }

        async fn released(&self) {
            tokio::time::timeout(Duration::from_secs(5), async {
                while !self.active.is_empty() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the tailer released its slot");
        }
    }

    fn finished_id(data: &AcpEventData) -> &str {
        match data.event.as_ref() {
            Event::BackgroundTaskFinished { task_id, .. } => task_id,
            other => panic!("expected BackgroundTaskFinished, got {other:?}"),
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_change_after_the_initial_scan_wakes_the_tailer() {
        with_managed(|scope| {
            Box::pin(async move {
                let mut f = fixture(scope, "{\"type\":\"user\"}\n").await;
                f.spawn(TailerStart::Live).await;
                f.parked().await;
                // Another task's end first: the tailer keeps following.
                f.append(&notification_line("t-9", Some("completed")));
                f.append(&notification_line("t-1", Some("completed")));
                assert_eq!(finished_id(&f.next().await), "t-1");
                f.released().await;
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn without_a_watcher_a_later_append_is_found_by_the_fallback_poll() {
        with_managed(|scope| {
            Box::pin(async move {
                let mut f = fixture(scope, "{\"type\":\"user\"}\n").await;
                f.spawn_with(FileWatchService::noop(), TailerStart::Live)
                    .await;
                // A no-op watcher hands back a closed channel, so the tailer switches
                // to polling right after its first checkpoint, and rescans once on
                // the way. Only the second checkpoint is after that switch: an append
                // here can no longer be caught by the rescan, only by a poll.
                f.parked().await;
                f.parked().await;
                f.append(&notification_line("t-1", Some("failed")));
                assert_eq!(finished_id(&f.next().await), "t-1");
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_notification_before_the_tailer_started_is_still_found() {
        with_managed(|scope| {
            Box::pin(async move {
                let mut f = fixture(scope, &notification_line("t-1", Some("failed"))).await;
                f.spawn(TailerStart::Live).await;
                assert_eq!(finished_id(&f.next().await), "t-1");
            })
        })
        .await;
    }

    // Recovered children settle before their parent, under original allocation IDs.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_workflow_run_settles_its_agents_before_it_finishes() {
        with_managed(|scope| Box::pin(async move {
            let mut f = fixture_for(scope, &notification_line("t-1", Some("completed")), FixtureSource::Workflow).await;
            let (launch, known) = f.workflow.as_ref().unwrap();
            let child_allocation = known.resources["a1"].allocation;
            std::fs::remove_dir_all(&launch.transcript_dir).unwrap();
            f.spawn_with(FileWatchService::noop(), TailerStart::Resumed).await;
            let detached = f.next().await;
            assert!(matches!(detached.event.as_ref(),
                Event::BackgroundAgentCompleted { agent_id, status: crate::acp::state::BackgroundAgentStatus::Detached, .. }
                    if agent_id == "a1"));
            assert_eq!(detached.metadata.resource_ref, Some(child_allocation));
            let finished = f.next().await;
            assert_eq!(finished_id(&finished), "t-1");
            assert_eq!(finished.metadata.resource_ref, Some(f.authority.witness().unwrap().allocation));
            f.released().await;
        })).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_resumed_tailer_scans_the_whole_transcript() {
        with_managed(|scope| {
            Box::pin(async move {
                // The notification sits beyond the live lookback window and many
                // chunks in.
                let mut initial = "x".repeat(READ_CHUNK_BYTES * 3);
                initial.push('\n');
                initial.push_str(&notification_line("t-1", Some("killed")));
                let mut resumed = fixture(scope, &initial).await;
                resumed.spawn(TailerStart::Resumed).await;
                // It ended while the daemon was down: the finish keeps that time.
                match resumed.next().await.event.as_ref() {
                    Event::BackgroundTaskFinished { task_id, ended_at } => {
                        assert_eq!(task_id, "t-1");
                        assert_eq!(
                            ended_at.map(|at| at.to_rfc3339()).as_deref(),
                            Some("2026-01-01T00:00:05+00:00")
                        );
                    }
                    other => panic!("expected BackgroundTaskFinished, got {other:?}"),
                }
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_transcript_that_cannot_be_found_ends_tracking() {
        with_managed(|scope| {
            Box::pin(async move {
                let mut f = fixture(scope, "").await;
                std::fs::remove_file(&f.transcript).unwrap();
                f.spawn_with(FileWatchService::noop(), TailerStart::Live)
                    .await;
                assert_eq!(finished_id(&f.next().await), "t-1");
                f.released().await;
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_poisoned_in_flight_set_does_not_strand_the_task() {
        with_managed(|scope| {
            Box::pin(async move {
                let mut f = fixture(scope, &notification_line("t-1", Some("completed"))).await;
                let active = f.active.clone();
                // A tailer that panicked while holding the set's lock.
                let _ = std::thread::spawn(move || {
                    let _held = active.owners.lock().unwrap();
                    panic!("poison the in-flight set");
                })
                .join();
                assert!(f.active.owners.is_poisoned());

                f.spawn(TailerStart::Live).await;
                assert_eq!(finished_id(&f.next().await), "t-1");
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_task_already_tracked_is_not_tailed_twice() {
        with_managed(|scope| {
            Box::pin(async move {
                let mut f = fixture(scope, "").await;
                f.spawn(TailerStart::Live).await;
                f.parked().await;
                assert!(f
                    .tailer(FileWatchService::new().unwrap(), TailerStart::Live)
                    .spawn(None)
                    .await
                    .is_none());
                f.append(&notification_line("t-1", Some("completed")));
                assert_eq!(finished_id(&f.next().await), "t-1");
                f.released().await;
                // One tailer, one finish: nothing else is queued behind it.
                f.drop_sender();
                let rest = tokio::time::timeout(Duration::from_secs(5), f.rx.recv())
                    .await
                    .expect("channel closes once the tailers are gone");
                assert!(rest.is_none(), "a second finish was published: {rest:?}");
            })
        })
        .await;
    }

    /// A tailer waiting on a task that never reports must not keep the
    /// worker's event channel open after the connection is gone, or the
    /// drain never sees EOF and recovery never starts.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_pending_tailer_does_not_hold_the_event_channel_open() {
        with_managed(|scope| {
            Box::pin(async move {
                let mut f = fixture(scope, "{\"type\":\"user\"}\n").await;
                f.spawn(TailerStart::Live).await;
                f.parked().await;

                f.drop_sender();
                let closed = tokio::time::timeout(Duration::from_secs(5), f.rx.recv())
                    .await
                    .expect("the channel closes with the connection, tailer or not");
                assert!(closed.is_none());

                // Its next wake finds no connection and ends it.
                f.append("{\"type\":\"user\"}\n");
                f.released().await;
            })
        })
        .await;
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

    #[test]
    fn an_early_notification_stops_the_read_before_the_large_suffix() {
        let mut contents = notification_line("t-1", Some("completed"));
        contents.push_str(&"filler\n".repeat(READ_CHUNK_BYTES * 4 / 7));
        let (_dir, path) = write_transcript(&contents);
        assert!(contents.len() > READ_CHUNK_BYTES * 3);

        let mut cursor = start_of_transcript();
        assert!(matches!(
            scan_file(&path, &mut cursor, "t-1", "toolu_1"),
            Scan::Ended(_)
        ));
        assert!(
            cursor.offset <= READ_CHUNK_BYTES as u64,
            "read {} bytes for a notification in the first line",
            cursor.offset
        );
    }

    #[test]
    fn a_notification_straddling_a_chunk_boundary_is_found() {
        // The line starts 50 bytes before the boundary and runs past it.
        let mut contents = "x".repeat(READ_CHUNK_BYTES - 51);
        contents.push('\n');
        contents.push_str(&notification_line("t-1", Some("failed")));
        contents.push_str("{\"type\":\"user\"}\n");
        let (_dir, path) = write_transcript(&contents);

        let mut cursor = start_of_transcript();
        assert!(matches!(
            scan_file(&path, &mut cursor, "t-1", "toolu_1"),
            Scan::Ended(_)
        ));
        assert!(
            cursor.offset > READ_CHUNK_BYTES as u64,
            "the second chunk was needed"
        );
    }

    #[test]
    fn an_overlong_line_is_skipped_without_being_buffered() {
        let mut contents = "y".repeat(MAX_LINE_BYTES + 10);
        contents.push('\n');
        contents.push_str(&notification_line("t-1", Some("completed")));
        let (_dir, path) = write_transcript(&contents);

        let mut cursor = start_of_transcript();
        assert!(matches!(
            scan_file(&path, &mut cursor, "t-1", "toolu_1"),
            Scan::Ended(_)
        ));
        assert!(cursor.carry.len() <= MAX_LINE_BYTES);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_reused_task_id_waits_for_its_captured_launch_completion() {
        with_managed(|scope| {
            Box::pin(async move {
                for start in [TailerStart::Live, TailerStart::Resumed] {
                    let old =
                        notification_line("t-1", Some("completed")).replace("toolu_1", "toolu_old");
                    let mut fixture = fixture(scope, &old).await;
                    fixture.spawn(start).await;
                    fixture.parked().await;
                    assert!(fixture.rx.try_recv().is_err());
                    fixture.append(&notification_line("t-1", Some("completed")));
                    assert_eq!(finished_id(&fixture.next().await), "t-1");
                    fixture.released().await;
                }
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_superseded_tailer_cannot_finish_or_release_the_new_launch() {
        with_managed(|scope| {
            Box::pin(async move {
                let mut fixture = fixture_with_prior(scope, "", Some("toolu_old")).await;
                let tailer = |fixture: &Fixture, tool_call_id: &str| TaskTailer {
                    claude_home: Some(fixture.home.clone()),
                    watch: FileWatchService::new().unwrap(),
                    task_id: "t-1".into(),
                    tool_call_id: Some(tool_call_id.into()),
                    session_id: fixture.session_id.clone(),
                    active: fixture.active.clone(),
                    event_tx: fixture.tx.as_ref().unwrap().downgrade().into(),
                    start: TailerStart::Live,
                    authority: Arc::clone(if tool_call_id == "toolu_1" {
                        &fixture.authority
                    } else {
                        fixture
                            .prior_authority
                            .as_ref()
                            .expect("actual earlier allocation")
                    }),
                    idle: fixture.idle.clone(),
                    workflow: None,
                };
                let old = super::resource_test_support::wait_for_tracking(async || {
                    tailer(&fixture, "toolu_old").spawn(None).await
                })
                .await;
                fixture.parked().await;
                let new = super::resource_test_support::wait_for_tracking(async || {
                    tailer(&fixture, "toolu_1").spawn(None).await
                })
                .await;
                tokio::time::timeout(Duration::from_secs(5), old)
                    .await
                    .unwrap()
                    .unwrap();
                fixture.parked().await;
                fixture.append(
                    &notification_line("t-1", Some("completed")).replace("toolu_1", "toolu_old"),
                );
                fixture.parked().await;
                assert!(fixture.rx.try_recv().is_err());
                fixture.append(&notification_line("t-1", Some("completed")));
                assert_eq!(finished_id(&fixture.next().await), "t-1");
                tokio::time::timeout(Duration::from_secs(5), new)
                    .await
                    .unwrap()
                    .unwrap();
                fixture.released().await;
                fixture.drop_sender();
                assert!(fixture.rx.recv().await.is_none());
            })
        })
        .await;
    }
}
#[cfg(test)]
pub(crate) mod resource_test_support {
    use super::{shared_watch, ActiveTasks, TaskTailer};
    use crate::acp::background_agent::{ActiveAgents, TailerStart};
    use crate::acp::establishment::{NativeEstablishmentReceipt, NativeResourceAuthority};
    use crate::acp::event_sender::{AcpEventData, SdkEvent, WeakEventSender};
    use crate::acp::state::{BackgroundAgentStatus, Event, WorkflowAgents, WorkflowLaunch};
    use crate::acp::supervisor::test_support::managed::{ManagedClient, ManagedScope, NativePeer};
    use crate::acp::workflow_journal::WorkflowJournal;
    use crate::file_watch::FileWatchService;
    use std::collections::HashSet;
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::{mpsc, oneshot};
    use tokio::task::JoinHandle;

    pub(crate) const STARTED_A1: &str = "{\"type\":\"started\",\"agentId\":\"a1\",\"label\":\"draft:one\",\"phase\":\"Draft\",\"key\":\"k1\"}\n";
    pub(crate) const STARTED_A2: &str = "{\"type\":\"started\",\"agentId\":\"a2\",\"label\":\"verify:one\",\"phase\":\"Verify\",\"key\":\"k2\"}\n";
    pub(crate) const RESULT_A1: &str = "{\"type\":\"result\",\"agentId\":\"a1\",\"key\":\"k1\",\"result\":{\"path\":\"/tmp/x.md\",\"summary\":\"drafted\"}}\n";

    pub(crate) fn append(path: &Path, bytes: &str) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        file.write_all(bytes.as_bytes()).unwrap();
    }

    pub(crate) async fn next(rx: &mut mpsc::Receiver<SdkEvent>) -> AcpEventData {
        match tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("resource event deadline")
            .expect("resource channel remains open")
        {
            SdkEvent::Data(data) => data,
            other => panic!("unexpected private barrier: {other:?}"),
        }
    }

    pub(crate) async fn join(task: JoinHandle<()>) {
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("resource reader terminates")
            .unwrap();
    }

    pub(crate) async fn closed(rx: &mut mpsc::Receiver<SdkEvent>) -> Vec<AcpEventData> {
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut data = Vec::new();
            while let Some(event) = rx.recv().await {
                match event {
                    SdkEvent::Data(event) => data.push(event),
                    other => panic!("unexpected private barrier: {other:?}"),
                }
            }
            data
        })
        .await
        .expect("weak followers release the resource channel")
    }

    pub(crate) async fn retry_registry_busy<T>(
        mut attempt: impl std::ops::AsyncFnMut() -> anyhow::Result<T>,
    ) -> T {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match attempt().await {
                    Err(error)
                        if error
                            .downcast_ref::<crate::process::worker_registry::RegistryBusy>()
                            .is_some() =>
                    {
                        tokio::task::yield_now().await;
                    }
                    result => return result.expect("captured resource operation"),
                }
            }
        })
        .await
        .expect("captured resource registry admission deadline")
    }

    pub(crate) async fn wait_for_tracking<T>(
        mut attempt: impl std::ops::AsyncFnMut() -> Option<T>,
    ) -> T {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Some(tracking) = attempt().await {
                    return tracking;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("actual captured tracking admission deadline")
    }

    pub(crate) struct NativeBootstrap {
        pub(crate) home: PathBuf,
        pub(crate) managed: ManagedClient,
        pub(crate) peer: NativePeer,
        pub(crate) receipt: Arc<NativeEstablishmentReceipt>,
        pub(crate) session_id: String,
        pub(crate) transcript: PathBuf,
        queue: Option<oneshot::Receiver<WeakEventSender>>,
        tracking: Option<oneshot::Receiver<crate::acp::acp_client::TrackingObservation>>,
    }

    impl NativeBootstrap {
        pub(crate) async fn new(scope: &mut ManagedScope) -> Self {
            let workdir = scope.tempdir();
            let (queue_tx, queue_rx) = oneshot::channel();
            let (tracking_tx, tracking_rx) = oneshot::channel();
            scope.configure_sdk(crate::acp::acp_client::SdkTestOptions {
                queue: Some(queue_tx),
                tracking: Some(tracking_tx),
                ..Default::default()
            });
            let (config, peer) = scope.native_peer(
                workdir.path().to_path_buf(),
                "claude",
                "claude",
                |request| {
                    let result = match request["method"].as_str()? {
                        "initialize" => {
                            serde_json::json!({"protocolVersion":1,"agentCapabilities":{}})
                        }
                        "session/new" => serde_json::json!({"sessionId":"task-native"}),
                        _ => return None,
                    };
                    Some(serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":result}))
                },
            );
            let managed = scope
                .spawn(config, format!("resource-fixture-{}", uuid::Uuid::new_v4()))
                .await
                .unwrap();
            managed.ready().await;
            let receipt = managed
                .actual()
                .establishment_projection()
                .unwrap()
                .receipt_snapshot()
                .unwrap();
            let session_id = receipt.session_id().unwrap().to_owned();
            let home = managed
                .actual()
                .native_store
                .as_ref()
                .filter(|binding| binding.agent == "claude" && binding.filesystem == "host")
                .and_then(|binding| binding.stores.first())
                .expect("actual host Claude store")
                .clone();
            assert!(home.starts_with(PathBuf::from(
                std::env::var_os("HOME").expect("isolated HOME")
            )));
            std::fs::create_dir_all(home.join("projects/-enc")).unwrap();
            let transcript = home
                .join("projects/-enc")
                .join(format!("{session_id}.jsonl"));
            std::fs::write(&transcript, "").unwrap();
            Self {
                home,
                managed,
                peer,
                receipt,
                session_id,
                transcript,
                queue: Some(queue_rx),
                tracking: Some(tracking_rx),
            }
        }

        pub(crate) async fn tool(
            &self,
            scope: &ManagedScope,
            tool: &str,
            name: &str,
            response: serde_json::Value,
        ) {
            let frame = serde_json::json!({
                "jsonrpc":"2.0","method":"session/update","params":{
                    "sessionId":self.session_id,"update":{
                        "sessionUpdate":"tool_call_update","toolCallId":tool,
                        "_meta":{"claudeCode":{"toolName":name,"toolResponse":response}}
                    }
                }
            });
            // A fresh Native frame may be history-only; never turn it into IO evidence.
            tokio::time::timeout(Duration::from_secs(10), async {
                let mut seen = 0;
                loop {
                    self.peer.send(frame.clone()).await;
                    loop {
                        let mut count = 0;
                        for stored in scope
                            .state
                            .acp_event_store
                            .replay_from(&self.managed.id, 0)
                            .unwrap()
                        {
                            let matching = match stored.event.as_ref() {
                                Event::BackgroundAgentLaunched { tool_call_id, .. } => {
                                    name == "Agent" && tool_call_id == tool
                                }
                                Event::BackgroundTaskStarted {
                                    tool_call_id: Some(actual),
                                    ..
                                } => name != "Agent" && actual == tool,
                                _ => false,
                            };
                            if matching {
                                count += 1;
                                if stored.metadata.resource_witness.is_some() {
                                    return;
                                }
                                assert_eq!(
                                    stored.metadata.scope,
                                    crate::acp::event_sender::EventScope::HistoryOnly
                                );
                                assert!(stored.metadata.resource_ref.is_none());
                            }
                        }
                        if count > seen {
                            seen = count;
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                }
            })
            .await
            .expect("real Native frame reaches admitted first IO");
        }

        pub(crate) async fn quiesce(&mut self, scope: &mut ManagedScope) {
            let watcher = shared_watch();
            let tracking = self
                .tracking
                .take()
                .unwrap()
                .await
                .expect("actual SDK trackers observed");
            let queue = self
                .queue
                .take()
                .unwrap()
                .await
                .expect("actual SDK queue observed");
            tokio::time::timeout(
                Duration::from_secs(10),
                scope.state.acp_supervisor.detach_all(),
            )
            .await
            .expect("SDK cancellation releases its original resource drain");
            assert!(
                tracking.tasks.is_empty() && tracking.agents.is_empty(),
                "SDK owner cancellation releases its actual task and agent readers"
            );
            tokio::time::timeout(Duration::from_secs(10), async {
                while queue.upgrade().is_some() || watcher.subscriber_count() != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("detached bootstrap has no producer, watcher, or admitted reader");
        }
    }

    pub(crate) struct AgentFixture {
        _native: NativeBootstrap,
        pub(crate) path: PathBuf,
        pub(crate) authority: Arc<NativeResourceAuthority>,
    }

    impl AgentFixture {
        pub(crate) async fn new(scope: &mut ManagedScope) -> Self {
            let mut native = NativeBootstrap::new(scope).await;
            let path = native.home.join("agent.jsonl");
            std::fs::write(&path, "").unwrap();
            native
                .tool(
                    scope,
                    "toolu_1",
                    "Agent",
                    serde_json::json!({
                        "agentId":"dup","description":"fixture","prompt":"fixture",
                        "outputFile":path,"status":"async_launched"
                    }),
                )
                .await;
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    match native
                        .managed
                        .recv()
                        .await
                        .expect("original Native observer remains open")
                    {
                        Event::BackgroundAgentLaunched {
                            agent_id,
                            tool_call_id,
                            ..
                        } if agent_id == "dup" && tool_call_id == "toolu_1" => break,
                        Event::BackgroundAgentCompleted { .. } => {
                            panic!("bootstrap Agent ended before hydration")
                        }
                        _ => {}
                    }
                }
            })
            .await
            .expect("Native Agent launch is published");
            let mut witness = None;
            for stored in scope
                .state
                .acp_event_store
                .replay_from(&native.managed.id, 0)
                .unwrap()
            {
                if matches!(stored.event.as_ref(), Event::BackgroundAgentLaunched { agent_id, .. } if agent_id == "dup")
                {
                    let Some(allocated) = stored.metadata.resource_witness else {
                        assert_eq!(
                            stored.metadata.scope,
                            crate::acp::event_sender::EventScope::HistoryOnly
                        );
                        assert!(stored.metadata.resource_ref.is_none());
                        continue;
                    };
                    assert_eq!(stored.metadata.resource_ref, Some(allocated.allocation));
                    assert!(witness.replace(allocated).is_none());
                }
            }
            native.quiesce(scope).await;
            let authority = NativeResourceAuthority::adopt(
                witness.expect("persisted Agent witness"),
                &native.receipt,
            )
            .unwrap();
            assert!(
                matches!(authority.selector(), crate::acp::establishment::NativeResourceSelector::Agent {
                agent_id, tool_call_id, output_file, source: crate::acp::establishment::NativeResourceLocation::Host,
            } if agent_id == "dup" && tool_call_id == "toolu_1" && output_file == path.to_str().unwrap())
            );
            Self {
                _native: native,
                path,
                authority,
            }
        }
    }

    #[derive(Clone, Copy)]
    pub(crate) enum JournalPath {
        Inside,
        Outside,
        Escaping,
        Empty,
        Home,
    }

    pub(crate) struct WorkflowFixture {
        pub(crate) native: NativeBootstrap,
        pub(crate) dir: PathBuf,
        pub(crate) launch: WorkflowLaunch,
        pub(crate) known: WorkflowAgents,
        pub(crate) authority: Arc<NativeResourceAuthority>,
        pub(crate) active: Arc<ActiveTasks>,
        pub(crate) agents: Arc<ActiveAgents>,
        pub(crate) watch: Arc<FileWatchService>,
        pub(crate) tx: Option<mpsc::Sender<SdkEvent>>,
        pub(crate) rx: mpsc::Receiver<SdkEvent>,
    }

    impl WorkflowFixture {
        pub(crate) async fn new(
            scope: &mut ManagedScope,
            journal: &str,
            launched: &[&str],
            completed: &[&str],
            path: JournalPath,
        ) -> Self {
            let mut native = NativeBootstrap::new(scope).await;
            let inside = native.home.join("projects/-enc/subagents/workflows/wf_1");
            std::fs::create_dir_all(&inside).unwrap();
            let outside = match path {
                JournalPath::Outside | JournalPath::Escaping => Some(scope.tempdir()),
                _ => None,
            };
            let dir = match path {
                JournalPath::Inside => std::fs::canonicalize(&inside).unwrap(),
                JournalPath::Outside => outside.as_ref().unwrap().path().to_path_buf(),
                JournalPath::Escaping => {
                    let mut escaping = native.home.clone();
                    for _ in native.home.ancestors().skip(1) {
                        escaping.push("..");
                    }
                    escaping.push(outside.as_ref().unwrap().path().strip_prefix("/").unwrap());
                    assert_eq!(
                        std::fs::canonicalize(&escaping).unwrap(),
                        std::fs::canonicalize(outside.as_ref().unwrap().path()).unwrap()
                    );
                    escaping
                }
                JournalPath::Empty => PathBuf::new(),
                JournalPath::Home => native.home.clone(),
            };
            let writable = if dir.as_os_str().is_empty() {
                &inside
            } else {
                &dir
            };
            std::fs::write(writable.join("journal.jsonl"), journal).unwrap();
            for agent in ["a1", "a2"] {
                std::fs::write(writable.join(format!("agent-{agent}.jsonl")), "").unwrap();
            }
            native
                .tool(
                    scope,
                    "toolu_1",
                    "Workflow",
                    serde_json::json!({
                        "status":"async_launched","taskId":"t-1","runId":"wf_1",
                        "workflowName":"drafts","transcriptDir":dir
                    }),
                )
                .await;
            let expected_launched: HashSet<String> =
                launched.iter().map(|id| (*id).to_owned()).collect();
            let expected_completed: HashSet<String> =
                completed.iter().map(|id| (*id).to_owned()).collect();
            tokio::time::timeout(Duration::from_secs(10), async {
                let mut parent = false;
                let mut seen_launched = HashSet::new();
                let mut seen_completed = HashSet::new();
                while !parent
                    || seen_launched != expected_launched
                    || seen_completed != expected_completed
                {
                    match native
                        .managed
                        .recv()
                        .await
                        .expect("original Native observer remains open")
                    {
                        Event::BackgroundTaskStarted {
                            task_id,
                            tool_call_id: Some(tool),
                            workflow: Some(run),
                            ..
                        } if task_id == "t-1" && tool == "toolu_1" && run.run_id == "wf_1" => {
                            parent = true
                        }
                        Event::BackgroundAgentLaunched { agent_id, .. } => {
                            assert!(expected_launched.contains(&agent_id));
                            seen_launched.insert(agent_id);
                        }
                        Event::BackgroundAgentCompleted {
                            agent_id, status, ..
                        } => {
                            assert!(expected_completed.contains(&agent_id));
                            assert_eq!(status, BackgroundAgentStatus::Completed);
                            seen_completed.insert(agent_id);
                        }
                        Event::BackgroundTaskFinished { .. } => {
                            panic!("bootstrap parent ended before hydration")
                        }
                        _ => {}
                    }
                }
            })
            .await
            .expect("real Workflow allocations/outcomes are published");
            let retained = scope
                .state
                .acp_event_store
                .unresolved_background_task_launches(&native.managed.id)
                .unwrap()
                .into_iter()
                .find(|launch| {
                    launch.task_id == "t-1" && launch.tool_call_id.as_deref() == Some("toolu_1")
                })
                .unwrap();
            let witness = retained.witness.expect("actual parent allocation");
            let launch = retained.workflow.unwrap();
            let known = retained.workflow_agents;
            assert_eq!(known.launched, expected_launched);
            assert_eq!(known.completed, expected_completed);
            for agent in launched {
                let child = known
                    .resources
                    .get(*agent)
                    .expect("persisted child allocation");
                assert!(
                    matches!(child.selector.as_ref(), crate::acp::establishment::NativeResourceSelector::WorkflowChild {
                    parent_allocation, run_id, task_id, agent_id, output_file,
                    source: crate::acp::establishment::NativeResourceLocation::Host,
                } if *parent_allocation == witness.allocation && run_id == "wf_1" && task_id == "t-1"
                    && agent_id == agent && output_file == dir.join(format!("agent-{agent}.jsonl")).to_str().unwrap())
                );
            }
            native.quiesce(scope).await;
            let authority = NativeResourceAuthority::adopt(witness, &native.receipt).unwrap();
            let (tx, rx) = mpsc::channel(64);
            Self {
                native,
                dir,
                launch,
                known,
                authority,
                active: Arc::default(),
                agents: Arc::default(),
                watch: FileWatchService::noop(),
                tx: Some(tx),
                rx,
            }
        }

        pub(crate) async fn start(&self) -> JoinHandle<()> {
            let tx = self.tx.as_ref().unwrap();
            wait_for_tracking(async || {
                TaskTailer {
                    claude_home: Some(self.native.home.clone()),
                    watch: Arc::clone(&self.watch),
                    task_id: "t-1".into(),
                    tool_call_id: Some("toolu_1".into()),
                    session_id: self.native.session_id.clone(),
                    active: Arc::clone(&self.active),
                    event_tx: tx.downgrade().into(),
                    start: TailerStart::Resumed,
                    authority: Arc::clone(&self.authority),
                    idle: Arc::new(|| {}),
                    workflow: Some(WorkflowJournal {
                        launch: self.launch.clone(),
                        known: self.known.clone(),
                        tool_call_id: "toolu_1".into(),
                        claude_home: self.native.home.clone(),
                        bg_agents: Arc::clone(&self.agents),
                        watch: Arc::clone(&self.watch),
                        event_tx: tx.downgrade().into(),
                    }),
                }
                .spawn(None)
                .await
            })
            .await
        }

        pub(crate) fn append_journal(&self, lines: &str) {
            append(&self.dir.join("journal.jsonl"), lines);
        }

        pub(crate) async fn finish(&mut self, task: JoinHandle<()>) -> Vec<AcpEventData> {
            let text = "<task-notification>\n<task-id>t-1</task-id>\n<tool-use-id>toolu_1</tool-use-id>\n<status>completed</status>\n</task-notification>";
            append(
                &self.native.transcript,
                &format!(
                    "{}\n",
                    serde_json::json!({
                        "type":"queue-operation","operation":"enqueue","timestamp":"2026-01-01T00:00:05Z","content":text
                    })
                ),
            );
            join(task).await;
            self.tx = None;
            let data = closed(&mut self.rx).await;
            self.released().await;
            let parent = data
                .last()
                .expect("parent completion published after journal settlement");
            assert!(
                matches!(parent.event.as_ref(), Event::BackgroundTaskFinished { task_id, .. } if task_id == "t-1")
            );
            assert_eq!(
                parent.metadata.resource_ref,
                Some(self.authority.witness().unwrap().allocation)
            );
            data
        }

        pub(crate) async fn released(&self) {
            tokio::time::timeout(Duration::from_secs(10), async {
                while !self.active.is_empty()
                    || !self.agents.is_empty()
                    || self.watch.subscriber_count() != 0
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("resource owners and watcher subscriptions released");
        }
    }
}
