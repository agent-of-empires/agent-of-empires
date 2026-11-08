//! Background async sub-agent transcript tailer.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::acp::event_sender::{EventSender, WeakEventSender};
use chrono::Utc;
use std::io::{Read, Seek, SeekFrom};
use tokio_util::sync::CancellationToken;

use crate::acp::establishment::NativeResourceAuthority;
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
    pub(crate) fn from_location(
        location: &crate::acp::establishment::NativeResourceLocation,
    ) -> Option<Self> {
        use crate::acp::establishment::NativeResourceLocation;
        match location {
            NativeResourceLocation::Host => Some(Self::Host),
            NativeResourceLocation::Container { runtime, container } => {
                let runtime = match runtime.as_str() {
                    "docker" => "docker",
                    "podman" => "podman",
                    "container" => "container",
                    _ => return None,
                };
                Some(Self::Container {
                    runtime,
                    container: container.clone(),
                })
            }
        }
    }

    async fn exists(&self, path: &Arc<str>, owner: &Arc<AgentOwner>) -> anyhow::Result<bool> {
        match self {
            TranscriptSource::Host => {
                let path = Arc::clone(path);
                let owner = Arc::clone(owner);
                tokio::task::spawn_blocking(move || {
                    owner.with_read(&TranscriptSource::Host, &path, || {
                        Ok(std::fs::metadata(path.as_ref()).is_ok())
                    })
                })
                .await?
            }
            TranscriptSource::Container { runtime, container } => {
                let mut cmd = tokio::process::Command::new(runtime);
                cmd.args(["exec", container, "test", "-e", path])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .kill_on_drop(true);
                let mut child = owner.with_read(self, path, || Ok(cmd.spawn()?))?;
                Ok(matches!(
                    tokio::time::timeout(CONTAINER_EXEC_TIMEOUT, child.wait()).await,
                    Ok(Ok(status)) if status.success()
                ))
            }
        }
    }

    async fn read_from(
        &self,
        path: &Arc<str>,
        offset: u64,
        owner: &Arc<AgentOwner>,
    ) -> anyhow::Result<Vec<u8>> {
        match self {
            TranscriptSource::Host => {
                let path = Arc::clone(path);
                let owner = Arc::clone(owner);
                tokio::task::spawn_blocking(move || {
                    owner.with_read(&TranscriptSource::Host, &path, || {
                        let Ok(mut file) = std::fs::File::open(path.as_ref()) else {
                            return Ok(Vec::new());
                        };
                        if file.seek(SeekFrom::Start(offset)).is_err() {
                            return Ok(Vec::new());
                        }
                        let mut chunk = Vec::new();
                        if file.read_to_end(&mut chunk).is_err() {
                            return Ok(Vec::new());
                        }
                        Ok(chunk)
                    })
                })
                .await?
            }
            TranscriptSource::Container { runtime, container } => {
                let start = format!("+{}", offset.saturating_add(1));
                let mut cmd = tokio::process::Command::new(runtime);
                cmd.args(["exec", container, "tail", "-c", &start, path])
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .kill_on_drop(true);
                let child = owner.with_read(self, path, || Ok(cmd.spawn()?))?;
                Ok(
                    match tokio::time::timeout(CONTAINER_EXEC_TIMEOUT, child.wait_with_output())
                        .await
                    {
                        Ok(Ok(output)) if output.status.success() => output.stdout,
                        _ => Vec::new(),
                    },
                )
            }
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct ActiveAgents {
    owners: Mutex<HashMap<String, Arc<AgentOwner>>>,
}

#[derive(Debug)]
struct AgentOwner {
    authority: Arc<NativeResourceAuthority>,
    cancellation: CancellationToken,
    active: std::sync::Weak<ActiveAgents>,
    agent_id: String,
    parent: Option<crate::acp::task_transcript::TaskReadOwner>,
}

impl AgentOwner {
    fn with_read<T>(
        &self,
        source: &TranscriptSource,
        path: &str,
        effect: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let read = || {
            let active = self
                .active
                .upgrade()
                .ok_or(crate::acp::establishment::EstablishmentRefusal::Superseded)?;
            let owners = active
                .owners
                .lock()
                .expect("bg-agent owners mutex poisoned");
            anyhow::ensure!(
                !self.cancellation.is_cancelled()
                    && owners
                        .get(&self.agent_id)
                        .is_some_and(|current| std::ptr::eq(current.as_ref(), self)),
                crate::acp::establishment::EstablishmentRefusal::Superseded
            );
            self.authority.check_transcript(source, path)?;
            effect()
        };
        match &self.parent {
            Some(parent) => parent.child_effect(&self.authority, read),
            None => self.authority.with_read(read),
        }
    }
}

impl ActiveAgents {
    pub(crate) fn is_empty(&self) -> bool {
        self.owners
            .lock()
            .expect("bg-agent owners mutex poisoned")
            .is_empty()
    }

    pub(crate) fn clear(&self) {
        let mut owners = self.owners.lock().expect("bg-agent owners mutex poisoned");
        for owner in owners.values() {
            owner.cancellation.cancel();
        }
        owners.clear();
    }

    fn allocate(
        self: &Arc<Self>,
        agent_id: String,
        authority: Arc<NativeResourceAuthority>,
        parent: Option<crate::acp::task_transcript::TaskReadOwner>,
    ) -> anyhow::Result<Option<ActiveGuard>> {
        let insert = || {
            let mut owners = self.owners.lock().expect("bg-agent owners mutex poisoned");
            if owners.get(&agent_id).is_some_and(|current| {
                !current.cancellation.is_cancelled() && current.authority.same_resource(&authority)
            }) {
                return Ok(None);
            }
            let allocation = authority
                .witness()
                .map_or_else(uuid::Uuid::new_v4, |witness| witness.allocation);
            let owner = Arc::new(AgentOwner {
                authority: authority.for_tracking_allocation(allocation)?,
                active: Arc::downgrade(self),
                agent_id: agent_id.clone(),
                cancellation: CancellationToken::new(),
                parent: parent.clone(),
            });
            if let Some(previous) = owners.insert(agent_id, Arc::clone(&owner)) {
                previous.cancellation.cancel();
            }
            Ok(Some(ActiveGuard {
                active: Arc::clone(self),
                owner,
            }))
        };
        match &parent {
            Some(parent) => parent.effect(insert),
            None => authority.admit_tracking(insert),
        }
    }
}

struct ActiveGuard {
    active: Arc<ActiveAgents>,
    owner: Arc<AgentOwner>,
}

impl ActiveGuard {
    async fn send(&self, events: &EventSender, event: Event) -> bool {
        let Ok(permit) = events.reserve().await else {
            return false;
        };
        let publish = || {
            let owners = self
                .active
                .owners
                .lock()
                .expect("bg-agent owners mutex poisoned");
            anyhow::ensure!(
                !self.owner.cancellation.is_cancelled()
                    && owners
                        .get(&self.owner.agent_id)
                        .is_some_and(|current| Arc::ptr_eq(current, &self.owner)),
                crate::acp::establishment::EstablishmentRefusal::Superseded
            );
            permit.send(event);
            Ok(())
        };
        match &self.owner.parent {
            Some(parent) => parent.with_owner(publish).is_ok(),
            None => publish().is_ok(),
        }
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.owner.cancellation.cancel();
        let mut owners = self
            .active
            .owners
            .lock()
            .expect("bg-agent owners mutex poisoned");
        if owners
            .get(&self.owner.agent_id)
            .is_some_and(|current| Arc::ptr_eq(current, &self.owner))
        {
            owners.remove(&self.owner.agent_id);
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

pub(crate) async fn spawn_tailer(
    transcript: (String, String),
    source: TranscriptSource,
    event_tx: EventSender,
    active: Arc<ActiveAgents>,
    start: TailerStart,
    authority: Arc<NativeResourceAuthority>,
    start_event: Option<Event>,
) {
    let (agent_id, output_file) = transcript;
    let tracking = match active.allocate(agent_id.clone(), authority, None) {
        Ok(Some(tracking)) => tracking,
        Ok(None) | Err(_) => {
            if let Some(event) = start_event {
                let _ = event_tx
                    .with_metadata(crate::acp::event_sender::EventMetadata::history())
                    .send(event)
                    .await;
            }
            return;
        }
    };
    let output_file: Arc<str> = output_file.into();
    let first_read = source.exists(&output_file, &tracking.owner).await;
    if let Some(event) = start_event {
        let start_tx = tracking.owner.authority.start_sender(&event_tx);
        if !tracking.send(&start_tx, event).await {
            return;
        }
    }
    let Ok(first_exists) = first_read else {
        return;
    };
    let event_tx = tracking.owner.authority.progress_sender(&event_tx);
    tokio::spawn(async move {
        tokio::select! {
            biased;
            _ = tracking.owner.cancellation.cancelled() => {}
            _ = run_tailer(agent_id, output_file, source, event_tx, start, &tracking, first_exists) => {}
        }
    });
}

/// The workflow journal reports completion after this reader's final progress.
pub(crate) async fn spawn_progress_tailer(
    transcript: (String, String),
    event_tx: WeakEventSender,
    active: Arc<ActiveAgents>,
    stop: CancellationToken,
    authority: Arc<NativeResourceAuthority>,
    start_event: Option<Event>,
    parent: crate::acp::task_transcript::TaskReadOwner,
) -> Option<(
    tokio::task::JoinHandle<()>,
    Arc<crate::acp::establishment::NativeResourceWitness>,
)> {
    let (agent_id, output_file) = transcript;
    let tracking = match active.allocate(agent_id.clone(), authority, Some(parent.clone())) {
        Ok(Some(tracking)) => tracking,
        Ok(None) | Err(_) => {
            if let Some(event) = start_event {
                let _ = parent.publish_resource(&event_tx, event, None).await;
            }
            return None;
        }
    };
    let output_file: Arc<str> = output_file.into();
    let first_read = TranscriptSource::Host
        .exists(&output_file, &tracking.owner)
        .await;
    if let Some(event) = start_event {
        let tx = event_tx.upgrade()?;
        if !tracking
            .send(&tracking.owner.authority.start_sender(&tx), event)
            .await
        {
            return None;
        }
    }
    if first_read.is_err() {
        return None;
    }
    let witness = tracking.owner.authority.witness()?;
    Some((
        tokio::spawn(async move {
            tokio::select! {
                biased;
                _ = tracking.owner.cancellation.cancelled() => {}
                _ = run_progress_tailer(agent_id, output_file, event_tx, stop, &tracking) => {}
            }
        }),
        witness,
    ))
}

async fn run_progress_tailer(
    agent_id: String,
    output_file: Arc<str>,
    event_tx: WeakEventSender,
    stop: CancellationToken,
    tracking: &ActiveGuard,
) {
    let source = TranscriptSource::Host;
    let mut offset: u64 = 0;
    let mut line_buf = String::new();
    let mut snap = Snapshot::default();
    let mut last_progress = Utc::now() - chrono::Duration::seconds(10);
    let mut last_growth = Utc::now();
    let mut stalled_emitted = false;
    loop {
        let stopping = stop.is_cancelled();
        let grew = match source.exists(&output_file, &tracking.owner).await {
            Ok(true) => match read_new_lines(
                &source,
                &output_file,
                &mut offset,
                &mut line_buf,
                &mut snap,
                &tracking.owner,
            )
            .await
            {
                Ok(grew) => grew,
                Err(_) => return,
            },
            Ok(false) => false,
            Err(_) => return,
        };
        if stopping {
            if snap.parsed_any {
                if let Some(tx) = event_tx.upgrade() {
                    let tx = tracking.owner.authority.progress_sender(&tx);
                    let _ = tracking
                        .send(
                            &tx,
                            progress(agent_id, BackgroundAgentStatus::Running, &snap),
                        )
                        .await;
                }
            }
            return;
        }
        let now = Utc::now();
        if grew {
            last_growth = now;
            stalled_emitted = false;
        }
        let idle = (now - last_growth).to_std().unwrap_or(Duration::ZERO);
        let status = if idle >= STALL_AFTER {
            BackgroundAgentStatus::Stalled
        } else {
            BackgroundAgentStatus::Running
        };
        let throttle_ok = (now - last_progress)
            .to_std()
            .map(|d| d >= PROGRESS_THROTTLE)
            .unwrap_or(true);
        let stall_edge = status == BackgroundAgentStatus::Stalled && !stalled_emitted;
        if (grew && throttle_ok) || stall_edge {
            let Some(tx) = event_tx.upgrade() else {
                return;
            };
            let tx = tracking.owner.authority.progress_sender(&tx);
            if !tracking
                .send(&tx, progress(agent_id.clone(), status, &snap))
                .await
            {
                return;
            }
            last_progress = now;
            stalled_emitted |= stall_edge;
        } else if event_tx.upgrade().is_none() {
            return;
        }
        tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
            _ = stop.cancelled() => {}
        }
    }
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
    output_file: Arc<str>,
    source: TranscriptSource,
    event_tx: EventSender,
    start: TailerStart,
    tracking: &ActiveGuard,
    mut exists: bool,
) {
    let mut waited = Duration::ZERO;
    while !exists {
        if waited >= WAIT_FILE_FOR {
            let (status, warning) = missing_transcript_outcome(start);
            let _ = tracking
                .send(
                    &event_tx,
                    completed(agent_id, status, Vec::new(), None, Some(warning.into())),
                )
                .await;
            return;
        }
        tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => waited += POLL_INTERVAL,
            _ = event_tx.closed() => return, // session gone
        }
        exists = match source.exists(&output_file, &tracking.owner).await {
            Ok(exists) => exists,
            Err(_) => return,
        };
    }

    let mut offset: u64 = 0;
    let mut line_buf = String::new();
    let mut snap = Snapshot::default();
    let mut last_progress = Utc::now() - chrono::Duration::seconds(10);
    let mut last_growth = Utc::now();
    let mut stalled_emitted = false;

    loop {
        let grew = match read_new_lines(
            &source,
            &output_file,
            &mut offset,
            &mut line_buf,
            &mut snap,
            &tracking.owner,
        )
        .await
        {
            Ok(grew) => grew,
            Err(_) => return,
        };
        let now = Utc::now();
        if grew {
            last_growth = now;
            stalled_emitted = false;
        }

        if snap.done {
            let warning = format_warning(&snap);
            let _ = tracking
                .send(
                    &event_tx,
                    completed(
                        agent_id,
                        BackgroundAgentStatus::Completed,
                        snapshot_tools(&snap),
                        snap.result.clone(),
                        warning,
                    ),
                )
                .await;
            return;
        }

        let idle = (now - last_growth).to_std().unwrap_or(Duration::ZERO);
        if idle >= ABORT_AFTER {
            let (status, result, warning) = infer_idle_outcome(&snap);
            let _ = tracking
                .send(
                    &event_tx,
                    completed(agent_id, status, snapshot_tools(&snap), result, warning),
                )
                .await;
            return;
        }

        let status = if idle >= STALL_AFTER {
            BackgroundAgentStatus::Stalled
        } else {
            BackgroundAgentStatus::Running
        };

        let throttle_ok = (now - last_progress)
            .to_std()
            .map(|d| d >= PROGRESS_THROTTLE)
            .unwrap_or(true);
        let stall_edge = status == BackgroundAgentStatus::Stalled && !stalled_emitted;
        if (grew && throttle_ok) || stall_edge {
            if !tracking
                .send(&event_tx, progress(agent_id.clone(), status, &snap))
                .await
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
    path: &Arc<str>,
    offset: &mut u64,
    line_buf: &mut String,
    snap: &mut Snapshot,
    owner: &Arc<AgentOwner>,
) -> anyhow::Result<bool> {
    let chunk = source.read_from(path, *offset, owner).await?;
    if chunk.is_empty() {
        return Ok(false);
    }
    *offset += chunk.len() as u64;
    line_buf.push_str(&String::from_utf8_lossy(&chunk));
    while let Some(nl) = line_buf.find('\n') {
        let line: String = line_buf.drain(..=nl).collect();
        let line = line.trim();
        if !line.is_empty() {
            fold_line(line, snap);
        }
    }
    Ok(true)
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
pub(crate) fn preview(text: &str) -> String {
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

    #[test]
    fn fold_and_idle_outcome_track_transcript_progress() {
        let cases = vec![
            (
                vec![
                    r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash"}]}}"#,
                    r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":false}]}}"#,
                    r#"{"type":"assistant","message":{"stop_reason":null,"content":[{"type":"text","text":"final report"}]}}"#,
                ],
                BackgroundAgentStatus::Completed,
                Some("final report"),
                "final text, no dangling tool, no end_turn marker: done",
            ),
            (
                vec![
                    r#"{"type":"assistant","message":{"content":[{"type":"text","text":"working on it"}]}}"#,
                    r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash"}]}}"#,
                ],
                BackgroundAgentStatus::Stalled,
                None,
                "a tool call still awaiting its result: hung",
            ),
            (
                vec![
                    r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash"},{"type":"text","text":"spoke after the call"}]}}"#,
                ],
                BackgroundAgentStatus::Stalled,
                None,
                "text after an unresolved tool call: still mid-action",
            ),
            (
                vec!["not json at all"],
                BackgroundAgentStatus::Stalled,
                None,
                "nothing parsed: hung",
            ),
        ];
        for (lines, expected_status, expected_result, desc) in cases {
            let (status, result, _) = infer_idle_outcome(&folded(&lines));
            assert_eq!(status, expected_status, "{desc}");
            assert_eq!(result.as_deref(), expected_result, "{desc}");
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

        // A missing transcript is an error only for a live launch; a resumed one lost tracking.
        assert_eq!(
            missing_transcript_outcome(TailerStart::Live).0,
            BackgroundAgentStatus::Error
        );
        assert_eq!(
            missing_transcript_outcome(TailerStart::Resumed).0,
            BackgroundAgentStatus::Detached
        );

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
}
#[cfg(test)]
mod real_resource_tests {
    use super::*;
    use crate::acp::event_sender::SdkEvent;
    use crate::acp::supervisor::test_support::managed::with_managed;
    use crate::acp::task_transcript::resource_test_support::{
        self as fixture, AgentFixture, JournalPath, WorkflowFixture, RESULT_A1, STARTED_A1,
    };

    #[tokio::test]
    #[serial_test::serial]
    async fn spawn_tailer_is_a_noop_when_the_agent_id_is_already_active() {
        with_managed(|scope| {
            Box::pin(async move {
                let f = AgentFixture::new(scope).await;
                let active = Arc::<ActiveAgents>::default();
                let (tx, mut rx) = tokio::sync::mpsc::channel::<SdkEvent>(8);
                let events: EventSender = tx.clone().into();
                let first = fixture::wait_for_tracking(async || {
                    spawn_tailer(
                        ("dup".into(), f.path.to_str().unwrap().into()),
                        TranscriptSource::Host,
                        events.clone(),
                        Arc::clone(&active),
                        TailerStart::Live,
                        Arc::clone(&f.authority),
                        None,
                    )
                    .await;
                    active.owners.lock().unwrap().get("dup").cloned()
                })
                .await;
                spawn_tailer(
                    ("dup".into(), f.path.to_str().unwrap().into()),
                    TranscriptSource::Host,
                    events.clone(),
                    Arc::clone(&active),
                    TailerStart::Live,
                    Arc::clone(&f.authority),
                    None,
                )
                .await;
                let second = active
                    .owners
                    .lock()
                    .unwrap()
                    .get("dup")
                    .expect("owner retained")
                    .clone();
                assert!(
                    Arc::ptr_eq(&first, &second),
                    "same resource must not replace its active reader"
                );
                assert_eq!(
                    first.authority.witness().unwrap().allocation,
                    f.authority.witness().unwrap().allocation
                );
                active.clear();
                drop(events);
                drop(tx);
                assert!(
                    fixture::closed(&mut rx).await.is_empty(),
                    "duplicate must not publish progress or completion"
                );
                assert!(active.is_empty());
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn host_read_new_lines_reads_from_offset_and_buffers_partials() {
        with_managed(|scope| Box::pin(async move {
            let f = AgentFixture::new(scope).await;
            let active = Arc::<ActiveAgents>::default();
            let tracking = fixture::retry_registry_busy(async || {
                active.allocate("dup".into(), Arc::clone(&f.authority), None)
            }).await.expect("actual resource tracking allocated");
            let path: Arc<str> = f.path.to_str().unwrap().into();
            let line = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash"}]}}"#;
            std::fs::write(&f.path, line).unwrap();
            let mut offset = 0;
            let mut carry = String::new();
            let mut snap = Snapshot::default();
            assert!(fixture::retry_registry_busy(async || {
                read_new_lines(&TranscriptSource::Host, &path, &mut offset, &mut carry,
                    &mut snap, &tracking.owner).await
            }).await);
            assert_eq!(snap.tool_count, 0, "partial line must not fold");
            assert_eq!(offset, line.len() as u64);
            std::fs::write(&f.path, format!("{line}\n{line}\n")).unwrap();
            assert!(fixture::retry_registry_busy(async || {
                read_new_lines(&TranscriptSource::Host, &path, &mut offset, &mut carry,
                    &mut snap, &tracking.owner).await
            }).await);
            assert_eq!(snap.tool_count, 2);
            assert!(carry.is_empty());
            let before = offset;
            assert!(!fixture::retry_registry_busy(async || {
                read_new_lines(&TranscriptSource::Host, &path, &mut offset, &mut carry,
                    &mut snap, &tracking.owner).await
            }).await);
            assert_eq!(offset, before);
            drop(tracking);
            assert!(active.is_empty());
        })).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn progress_tailer_reports_progress_but_never_completes() {
        with_managed(|scope| Box::pin(async move {
            let mut f = WorkflowFixture::new(scope, "", &[], &[], JournalPath::Inside).await;
            std::fs::write(f.dir.join("agent-a1.jsonl"), concat!(
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Bash\",\"input\":{\"command\":\"ls\"}}]}}\n",
                "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"t1\",\"is_error\":false}]}}\n",
                "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"end_turn\",\"content\":[{\"type\":\"text\",\"text\":\"done\"}]}}\n",
            )).unwrap();
            f.append_journal(STARTED_A1);
            let task = f.start().await;
            let launched = fixture::next(&mut f.rx).await;
            assert!(matches!(launched.event.as_ref(), Event::BackgroundAgentLaunched { agent_id, .. } if agent_id == "a1"));
            let allocation = launched.metadata.resource_witness.as_ref().expect("real child first-I/O witness").allocation;
            assert_eq!(launched.metadata.resource_ref, Some(allocation));
            let first = fixture::next(&mut f.rx).await;
            assert!(matches!(first.event.as_ref(), Event::BackgroundAgentProgress { tool_count: 1, .. }));
            assert_eq!(first.metadata.resource_ref, Some(allocation));
            fixture::append(&f.dir.join("agent-a1.jsonl"),
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"t2\",\"name\":\"Read\"}]}}\n");
            f.append_journal(RESULT_A1);
            let mut data = vec![launched, first];
            data.extend(f.finish(task).await);
            let completed: Vec<_> = data.iter().enumerate().filter(|(_, d)|
                matches!(d.event.as_ref(), Event::BackgroundAgentCompleted { .. })).collect();
            assert_eq!(completed.len(), 1, "only journal publishes child completion");
            let (completed_at, completion) = completed[0];
            assert!(matches!(completion.event.as_ref(), Event::BackgroundAgentCompleted {
                agent_id, status: BackgroundAgentStatus::Completed, result: Some(result), ..
            } if agent_id == "a1" && result == "drafted"));
            assert_eq!(completion.metadata.resource_ref, Some(allocation));
            let final_progress = data[..completed_at].iter().rfind(|d|
                matches!(d.event.as_ref(), Event::BackgroundAgentProgress { .. })).unwrap();
            assert!(matches!(final_progress.event.as_ref(), Event::BackgroundAgentProgress { tool_count: 2, .. }));
            assert_eq!(final_progress.metadata.resource_ref, Some(allocation));
            let mut state = crate::acp::AcpState::default();
            for (seq, d) in data.iter().enumerate() {
                state.apply_scoped_event(seq as u64 + 1, d.event.as_ref(), &d.metadata).unwrap();
            }
            let agent = &state.background_agents[0];
            assert_eq!(agent.resource_ref, Some(allocation));
            assert_eq!(agent.status, BackgroundAgentStatus::Completed);
            assert_eq!(agent.tool_count, 2);
            assert_eq!(agent.tools.len(), 2);
            assert_eq!(agent.tools[0].name, "Bash");
            assert_eq!(agent.tools[0].ok, Some(true));
            assert_eq!(agent.tools[1].name, "Read");
            assert_eq!(agent.result.as_deref(), Some("drafted"));
        })).await;
    }
}
