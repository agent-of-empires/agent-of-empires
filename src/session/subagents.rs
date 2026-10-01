//! Claude Code subagents of a terminal session, read from the agent's own
//! transcript store.
//!
//! Subagents run inside the parent `claude` process, so they have no pane.
//! Claude writes each one to
//! `<store>/projects/<cwd>/<sid>/subagents/agent-<id>.{jsonl,meta.json}`,
//! which is all this module reads. Every read goes through `AnchoredDir`
//! because a sandboxed session's store is writable from inside the container.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::Deserialize;
use serde_json::Value;

use crate::session::AnchoredDir;

/// Only the end of a transcript is parsed: it holds the state and the recent
/// activity the preview shows.
const TAIL_BYTES: u64 = 256 * 1024;
/// Enough for the first line, which carries the spawn timestamp.
const HEAD_BYTES: u64 = 64 * 1024;
/// Bound for `open_regular`; the tail read keeps the cost independent of it.
const TRANSCRIPT_MAX_BYTES: usize = 1 << 30;
const META_MAX_BYTES: usize = 16 * 1024;
const SUBAGENT_SCAN_MAX: usize = 1024;
const ACTIVITY_MAX: usize = 200;
const DETAIL_MAX_CHARS: usize = 120;
/// Finished subagents kept per session, newest first. Running ones are always kept.
pub const FINISHED_SHOWN: usize = 5;
/// How long a missing parent transcript waits before `projects/` is scanned again.
const LOCATE_RETRY: Duration = Duration::from_secs(5);
/// How often a found conversation is located again, to pick up a transcript
/// a later directory change started.
const RELOCATE_EVERY: Duration = Duration::from_secs(60);
/// Silence after which a subagent of an idle parent counts as stopped. Longer
/// than Claude's 10 minute tool timeout, so a long command is not mistaken
/// for a dead process.
const STALE_AFTER: Duration = Duration::from_secs(15 * 60);
/// Parent transcript bytes read per poll, so a large backlog is caught up over
/// several polls rather than one long read.
const PARENT_READ_MAX: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentState {
    Running,
    Done,
    /// Ended without finishing and without an error: interrupted, killed, or
    /// orphaned by a parent process that went away.
    Stopped,
    /// Claude reported that the subagent failed.
    Failed,
}

/// One line of a subagent's recent activity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubagentActivity {
    Prompt(String),
    Text(String),
    Tool { name: String, detail: String },
}

/// What the sidebar row shows. Activity is read separately, only for the
/// subagent whose preview is open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subagent {
    pub agent_id: String,
    pub agent_type: String,
    pub description: String,
    pub state: SubagentState,
    pub started_at: Option<SystemTime>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Meta {
    #[serde(default)]
    agent_type: String,
    #[serde(default)]
    description: String,
}

/// A transcript's identity and size; an unchanged version is not re-read.
type FileVersion = (libc::ino_t, libc::off_t, libc::time_t, libc::c_long);

fn file_version(dir: &AnchoredDir, name: &Path) -> Option<FileVersion> {
    let stat = dir.entry_stat(name).ok()??;
    Some((stat.st_ino, stat.st_size, stat.st_mtime, stat.st_mtime_nsec))
}

struct CachedSubagent {
    version: FileVersion,
    subagent: Subagent,
    /// Timestamp of the newest message, for [`settle`].
    last_entry_at: Option<SystemTime>,
}

/// Every subagent recorded under `subagents_dir` (a session's
/// `<sid>/subagents` directory), oldest first, with its newest message time.
/// `cache` holds the previous read of each transcript, keyed by its path.
fn read_subagents(
    store: &AnchoredDir,
    subagents_dir: &Path,
    cache: &mut HashMap<PathBuf, CachedSubagent>,
    seen: &mut HashSet<PathBuf>,
) -> Vec<(Subagent, Option<SystemTime>)> {
    let Ok(dir) = store.child(subagents_dir) else {
        return Vec::new();
    };
    let Ok(names) = dir.read_dir(Path::new(""), SUBAGENT_SCAN_MAX) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for name in &names {
        let Some(agent_id) = agent_id_of(name) else {
            continue;
        };
        let Some(version) = file_version(&dir, Path::new(name)) else {
            continue;
        };
        let key = store.path().join(subagents_dir).join(name);
        seen.insert(key.clone());
        let previous = cache.get(&key);
        if let Some(cached) = previous.filter(|cached| cached.version == version) {
            found.push((cached.subagent.clone(), cached.last_entry_at));
            continue;
        }
        let known = previous.map(|cached| &cached.subagent);
        let Some((subagent, last_entry_at)) = read_one(&dir, agent_id, known) else {
            continue;
        };
        found.push((subagent.clone(), last_entry_at));
        cache.insert(
            key,
            CachedSubagent {
                version,
                subagent,
                last_entry_at,
            },
        );
    }
    found.sort_by(|(a, _), (b, _)| {
        a.started_at
            .cmp(&b.started_at)
            .then_with(|| a.agent_id.cmp(&b.agent_id))
    });
    found
}

fn agent_id_of(name: &OsStr) -> Option<&str> {
    let id = name
        .to_str()?
        .strip_prefix("agent-")?
        .strip_suffix(".jsonl")?;
    (!id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
    .then_some(id)
}

fn transcript_name(agent_id: &str) -> PathBuf {
    PathBuf::from(format!("agent-{agent_id}.jsonl"))
}

/// Read a subagent whose transcript is new or changed. `known` is the previous
/// read, whose type, description, and start time never change.
fn read_one(
    dir: &AnchoredDir,
    agent_id: &str,
    known: Option<&Subagent>,
) -> Option<(Subagent, Option<SystemTime>)> {
    let transcript = transcript_name(agent_id);
    let (tail, truncated) = read_tail(dir, &transcript)?;
    let (state, last_entry_at) = summarize_tail(&tail, truncated);
    let subagent = match known {
        Some(known) => Subagent {
            state,
            ..known.clone()
        },
        None => {
            let meta: Option<Meta> = dir
                .read_regular(
                    Path::new(&format!("agent-{agent_id}.meta.json")),
                    META_MAX_BYTES,
                )
                .ok()
                .flatten()
                .and_then(|bytes| serde_json::from_slice(&bytes).ok());
            let (agent_type, description) = meta
                .map(|meta| (meta.agent_type, meta.description))
                .unwrap_or_default();
            Subagent {
                agent_id: agent_id.to_string(),
                agent_type: if agent_type.is_empty() {
                    "agent".to_string()
                } else {
                    agent_type
                },
                description,
                state,
                started_at: read_started_at(dir, &transcript),
            }
        }
    };
    Some((subagent, last_entry_at))
}

/// The last [`TAIL_BYTES`] of a transcript, and whether that starts mid-file
/// (so its first line may be partial).
fn read_tail(dir: &AnchoredDir, relative: &Path) -> Option<(Vec<u8>, bool)> {
    let mut file = dir.open_regular(relative, TRANSCRIPT_MAX_BYTES).ok()??;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut tail = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut tail).ok()?;
    Some((tail, start > 0))
}

/// When the subagent was spawned: the timestamp of its first transcript line.
/// The meta file's mtime is not it, since Claude rewrites that file later.
fn read_started_at(dir: &AnchoredDir, relative: &Path) -> Option<SystemTime> {
    #[derive(Deserialize)]
    struct First {
        timestamp: String,
    }
    let file = dir.open_regular(relative, TRANSCRIPT_MAX_BYTES).ok()??;
    let mut head = Vec::new();
    file.take(HEAD_BYTES).read_to_end(&mut head).ok()?;
    let line = head.split(|&b| b == b'\n').next()?;
    let first: First = serde_json::from_slice(line).ok()?;
    parse_timestamp(&first.timestamp)
}

fn parse_timestamp(value: &str) -> Option<SystemTime> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(SystemTime::from)
}

/// The complete lines of a tail read.
fn tail_lines(bytes: &[u8], truncated: bool) -> impl DoubleEndedIterator<Item = &[u8]> {
    let mut lines = bytes.split(|&b| b == b'\n');
    if truncated {
        lines.next();
    }
    lines
}

/// State and time of the newest message, the only lines parsed here: it is
/// done once that message is an assistant turn that ended, failed when it is
/// an API error, and stopped when it is Claude's interrupt marker.
fn summarize_tail(bytes: &[u8], truncated: bool) -> (SubagentState, Option<SystemTime>) {
    for line in tail_lines(bytes, truncated).rev() {
        let Ok(entry) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let state = match entry["type"].as_str() {
            // Claude's synthetic error turn also ends with `stop_sequence`.
            Some("assistant") if entry["isApiErrorMessage"] == true => SubagentState::Failed,
            Some("assistant") => match entry["message"]["stop_reason"].as_str() {
                Some("end_turn" | "stop_sequence") => SubagentState::Done,
                _ => SubagentState::Running,
            },
            Some("user") => {
                if user_text(&entry["message"]["content"])
                    .is_some_and(|text| text.starts_with("[Request interrupted"))
                {
                    SubagentState::Stopped
                } else {
                    SubagentState::Running
                }
            }
            _ => continue,
        };
        return (state, entry["timestamp"].as_str().and_then(parse_timestamp));
    }
    (SubagentState::Running, None)
}

fn user_text(content: &Value) -> Option<&str> {
    match content {
        Value::String(text) => Some(text),
        Value::Array(blocks) => blocks
            .iter()
            .find(|block| block["type"] == "text")
            .and_then(|block| block["text"].as_str()),
        _ => None,
    }
}

/// The recent activity a preview shows, oldest first.
fn parse_activity(bytes: &[u8], truncated: bool) -> Vec<SubagentActivity> {
    let mut activity = Vec::new();
    for line in tail_lines(bytes, truncated) {
        let Ok(entry) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        let content = &entry["message"]["content"];
        match entry["type"].as_str() {
            Some("assistant") => {
                for block in content.as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("text") => push_text(&mut activity, block["text"].as_str(), false),
                        Some("tool_use") => activity.push(SubagentActivity::Tool {
                            name: block["name"].as_str().unwrap_or("tool").to_string(),
                            detail: tool_detail(&block["input"]),
                        }),
                        _ => {}
                    }
                }
            }
            Some("user") if content.is_string() => {
                push_text(&mut activity, user_text(content), true);
            }
            _ => {}
        }
    }
    if activity.len() > ACTIVITY_MAX {
        activity.drain(..activity.len() - ACTIVITY_MAX);
    }
    activity
}

/// How each background subagent last ended, by agent id, from the
/// `<task-notification>` messages Claude posts into the parent transcript.
#[derive(Default)]
struct ParentLog {
    /// Bytes of the parent transcript already read.
    offset: u64,
    /// `offset` is inside a line longer than [`PARENT_READ_MAX`], which is
    /// skipped rather than read.
    skipping: bool,
    ended: Ended,
}

/// How each background subagent last ended, by agent id.
type Ended = HashMap<String, (SubagentState, Option<SystemTime>)>;

impl ParentLog {
    /// Read whatever the parent transcript gained since the last call.
    fn catch_up(&mut self, store: &AnchoredDir, transcript: &Path) {
        let Ok(Some(mut file)) = store.open_regular(transcript, usize::MAX) else {
            return;
        };
        let Ok(len) = file.metadata().map(|meta| meta.len()) else {
            return;
        };
        if len < self.offset {
            // Rewritten in place; start over.
            *self = Self::default();
        }
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return;
        }
        let mut bytes = Vec::new();
        if file.take(PARENT_READ_MAX).read_to_end(&mut bytes).is_err() {
            return;
        }
        self.consume(&bytes, bytes.len() as u64 == PARENT_READ_MAX);
    }

    /// Fold the complete lines of `bytes`, read from `offset`. `full` means the
    /// read hit [`PARENT_READ_MAX`], so a window with no line end is one
    /// oversized line to skip rather than a line still being written.
    fn consume(&mut self, bytes: &[u8], full: bool) {
        let mut start = 0;
        if self.skipping {
            match bytes.iter().position(|&b| b == b'\n') {
                Some(end) => {
                    start = end + 1;
                    self.skipping = false;
                }
                None => {
                    self.offset += bytes.len() as u64;
                    return;
                }
            }
        }
        let rest = &bytes[start..];
        let Some(complete) = rest.iter().rposition(|&b| b == b'\n').map(|i| i + 1) else {
            if full {
                self.skipping = true;
                self.offset += bytes.len() as u64;
            } else {
                self.offset += start as u64;
            }
            return;
        };
        for line in rest[..complete].split(|&b| b == b'\n') {
            if let Some((agent_id, state, at)) = parse_task_notification(line) {
                self.ended.insert(agent_id, (state, at));
            }
        }
        self.offset += (start + complete) as u64;
    }
}

/// `(agent id, final state, time)` from a parent transcript line carrying a
/// subagent's `<task-notification>`. An idle parent records it as a user
/// message; a busy one as a `queued_command` attachment delivered later.
fn parse_task_notification(line: &[u8]) -> Option<(String, SubagentState, Option<SystemTime>)> {
    const MARKER: &[u8] = b"<task-notification>";
    if !line.windows(MARKER.len()).any(|window| window == MARKER) {
        return None;
    }
    let entry: Value = serde_json::from_slice(line).ok()?;
    let attachment = &entry["attachment"];
    let text = match entry["type"].as_str()? {
        "user" => entry["message"]["content"].as_str()?,
        "attachment" if attachment["type"] == "queued_command" => attachment["prompt"].as_str()?,
        _ => return None,
    };
    let tag = |name: &str| {
        let open = format!("<{name}>");
        let start = text.find(&open)? + open.len();
        let len = text[start..].find(&format!("</{name}>"))?;
        Some(text[start..start + len].trim())
    };
    let state = match tag("status")? {
        "completed" => SubagentState::Done,
        "failed" => SubagentState::Failed,
        _ => SubagentState::Stopped,
    };
    let at = entry["timestamp"]
        .as_str()
        .or_else(|| attachment["timestamp"].as_str())
        .and_then(parse_timestamp);
    Some((tag("task-id")?.to_string(), state, at))
}

/// Settle subagents whose own transcript still reads as running: a parent
/// notification newer than their last message is final, and one whose idle
/// parent has heard nothing from it for [`STALE_AFTER`] is orphaned. Each
/// subagent comes with the time of its newest message.
fn settle(
    subagents: &mut [(Subagent, Option<SystemTime>)],
    ended: &Ended,
    parent_busy: bool,
    now: SystemTime,
) {
    for (subagent, last_entry_at) in subagents
        .iter_mut()
        .filter(|(subagent, _)| subagent.state == SubagentState::Running)
    {
        if let Some((state, at)) = ended.get(&subagent.agent_id) {
            // A subagent can be resumed after it ends; newer output wins.
            let resumed = matches!((*last_entry_at, at), (Some(last), Some(at)) if last > *at);
            if !resumed {
                subagent.state = *state;
                continue;
            }
        }
        let silent = last_entry_at
            .and_then(|last| now.duration_since(last).ok())
            .is_some_and(|silence| silence > STALE_AFTER);
        if !parent_busy && silent {
            subagent.state = SubagentState::Stopped;
        }
    }
}

fn push_text(activity: &mut Vec<SubagentActivity>, text: Option<&str>, prompt: bool) {
    let Some(text) = text.map(str::trim).filter(|t| !t.is_empty()) else {
        return;
    };
    activity.push(if prompt {
        SubagentActivity::Prompt(text.to_string())
    } else {
        SubagentActivity::Text(text.to_string())
    });
}

/// A one-line summary of a tool call's input: the field a reader recognises
/// it by, else nothing.
fn tool_detail(input: &Value) -> String {
    let detail = [
        "description",
        "command",
        "file_path",
        "path",
        "pattern",
        "url",
        "query",
        "prompt",
    ]
    .iter()
    .find_map(|key| input[key].as_str())
    .unwrap_or("");
    let line = detail.lines().next().unwrap_or("").trim();
    if line.chars().count() > DETAIL_MAX_CHARS {
        let cut: String = line.chars().take(DETAIL_MAX_CHARS - 1).collect();
        format!("{cut}…")
    } else {
        line.to_string()
    }
}

/// Keep every running subagent plus the [`FINISHED_SHOWN`] most recently
/// started finished ones, in their original order.
pub fn retain_recent(subagents: &mut Vec<Subagent>) {
    let mut finished_seen = 0;
    let keep: Vec<bool> = subagents
        .iter()
        .rev()
        .map(|subagent| {
            subagent.state == SubagentState::Running || {
                finished_seen += 1;
                finished_seen <= FINISHED_SHOWN
            }
        })
        .collect();
    let mut keep = keep.into_iter().rev();
    subagents.retain(|_| keep.next().unwrap_or(false));
}

/// A session whose subagents the TUI should list: its Claude store, current
/// conversation id, and whether the parent agent is mid-turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentSource {
    pub instance_id: String,
    pub store: PathBuf,
    pub session_id: String,
    pub parent_busy: bool,
}

impl SubagentSource {
    fn conversation(&self) -> (PathBuf, String) {
        (self.store.clone(), self.session_id.clone())
    }
}

/// The subagent whose preview is open, as `(instance id, agent id)`.
pub type SubagentFocus = (String, String);

/// One poll's result.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubagentScan {
    /// Subagents per instance id, sessions with none omitted.
    pub subagents: HashMap<String, Vec<Subagent>>,
    /// Recent activity of the focused subagent, when it is still listed.
    pub focus: Option<(SubagentFocus, Vec<SubagentActivity>)>,
}

/// Where a conversation's transcripts were found, and when that was checked.
struct Located {
    /// `projects/<encoded cwd>` directories holding `<sid>.jsonl`, one per
    /// directory the session spoke in.
    project_dirs: Vec<PathBuf>,
    checked_at: Instant,
}

/// Remembers where each conversation's transcripts live so a poll costs one
/// directory read per session instead of a scan of `projects/`.
#[derive(Default)]
pub struct SubagentScanner {
    /// Keyed by `(store, session id)`.
    located: HashMap<(PathBuf, String), Located>,
    /// Keyed by the parent transcript's path.
    parents: HashMap<PathBuf, ParentLog>,
    transcripts: HashMap<PathBuf, CachedSubagent>,
    /// The focused transcript's path, version, and parsed activity.
    activity: Option<(PathBuf, FileVersion, Vec<SubagentActivity>)>,
}

impl SubagentScanner {
    pub fn scan(
        &mut self,
        sources: &[SubagentSource],
        focus: Option<&SubagentFocus>,
    ) -> SubagentScan {
        self.scan_at(sources, focus, SystemTime::now())
    }

    fn scan_at(
        &mut self,
        sources: &[SubagentSource],
        focus: Option<&SubagentFocus>,
        now: SystemTime,
    ) -> SubagentScan {
        let live: HashSet<(PathBuf, String)> =
            sources.iter().map(SubagentSource::conversation).collect();
        self.located.retain(|key, _| live.contains(key));
        let mut out = SubagentScan::default();
        let mut seen = HashSet::new();
        let mut parents_seen = HashSet::new();
        for source in sources {
            let Ok(store) = AnchoredDir::open(&source.store) else {
                continue;
            };
            let project_dirs = self.project_dirs(&store, source);
            let mut subagents = Vec::new();
            let mut ended = Ended::new();
            // Where each subagent's transcript lives, for the focused preview.
            let mut dirs: HashMap<String, PathBuf> = HashMap::new();
            for project_dir in &project_dirs {
                let subagents_dir = project_dir.join(&source.session_id).join("subagents");
                let found =
                    read_subagents(&store, &subagents_dir, &mut self.transcripts, &mut seen);
                if found.is_empty() {
                    continue;
                }
                for (subagent, _) in &found {
                    dirs.insert(subagent.agent_id.clone(), subagents_dir.clone());
                }
                subagents.extend(found);
                let parent = project_dir.join(format!("{}.jsonl", source.session_id));
                let key = store.path().join(&parent);
                let log = self.parents.entry(key.clone()).or_default();
                log.catch_up(&store, &parent);
                ended.extend(log.ended.iter().map(|(id, end)| (id.clone(), *end)));
                parents_seen.insert(key);
            }
            if subagents.is_empty() {
                continue;
            }
            subagents.sort_by(|(a, _), (b, _)| {
                a.started_at
                    .cmp(&b.started_at)
                    .then_with(|| a.agent_id.cmp(&b.agent_id))
            });
            settle(&mut subagents, &ended, source.parent_busy, now);
            let mut subagents: Vec<Subagent> = subagents
                .into_iter()
                .map(|(subagent, _)| subagent)
                .collect();
            retain_recent(&mut subagents);
            if let Some(focus) = focus.filter(|(instance, agent)| {
                *instance == source.instance_id && subagents.iter().any(|s| s.agent_id == *agent)
            }) {
                let activity = dirs
                    .get(&focus.1)
                    .and_then(|dir| self.focused_activity(&store, dir, &focus.1));
                if let Some(activity) = activity {
                    out.focus = Some((focus.clone(), activity));
                }
            }
            out.subagents.insert(source.instance_id.clone(), subagents);
        }
        self.transcripts.retain(|path, _| seen.contains(path));
        self.parents.retain(|path, _| parents_seen.contains(path));
        if out.focus.is_none() {
            self.activity = None;
        }
        out
    }

    /// The conversation's project directories, from the cache while it is fresh.
    fn project_dirs(&mut self, store: &AnchoredDir, source: &SubagentSource) -> Vec<PathBuf> {
        let key = source.conversation();
        if let Some(located) = self.located.get(&key) {
            let ttl = if located.project_dirs.is_empty() {
                LOCATE_RETRY
            } else {
                RELOCATE_EVERY
            };
            if located.checked_at.elapsed() < ttl {
                return located.project_dirs.clone();
            }
        }
        let project_dirs: Vec<PathBuf> =
            crate::session::conversation_carry::claude_transcripts_for(store, &source.session_id)
                .unwrap_or_default()
                .iter()
                .filter_map(|transcript| transcript.parent().map(Path::to_path_buf))
                .collect();
        self.located.insert(
            key,
            Located {
                project_dirs: project_dirs.clone(),
                checked_at: Instant::now(),
            },
        );
        project_dirs
    }

    /// The focused subagent's activity, re-parsed only when its transcript changed.
    fn focused_activity(
        &mut self,
        store: &AnchoredDir,
        subagents_dir: &Path,
        agent_id: &str,
    ) -> Option<Vec<SubagentActivity>> {
        let dir = store.child(subagents_dir).ok()?;
        let name = transcript_name(agent_id);
        let version = file_version(&dir, &name)?;
        let path = store.path().join(subagents_dir).join(&name);
        if let Some((cached_path, cached_version, activity)) = &self.activity {
            if *cached_path == path && *cached_version == version {
                return Some(activity.clone());
            }
        }
        let (tail, truncated) = read_tail(&dir, &name)?;
        let activity = parse_activity(&tail, truncated);
        self.activity = Some((path, version, activity.clone()));
        Some(activity)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn line(value: Value) -> String {
        format!("{value}\n")
    }

    fn assistant(content: Value, stop_reason: Option<&str>) -> String {
        line(
            json!({"type": "assistant", "message": {"content": content, "stop_reason": stop_reason}}),
        )
    }

    fn user(content: Value) -> String {
        line(json!({"type": "user", "message": {"content": content}}))
    }

    #[test]
    fn transcript_tail_yields_state_and_activity() {
        let prompt = user(json!("List the files"));
        let tool = assistant(
            json!([{"type": "thinking", "thinking": ""},
                   {"type": "tool_use", "name": "Bash", "input": {"command": "ls -la\nsecond line"}}]),
            None,
        );
        let result = user(json!([{"type": "tool_result", "content": "a b"}]));
        let done = assistant(
            json!([{"type": "text", "text": "Found 2 files."}]),
            Some("end_turn"),
        );
        let interrupted = user(json!([{"type": "text", "text": "[Request interrupted by user]"}]));
        let attachment = line(json!({"type": "attachment", "attachment": {"type": "date"}}));
        // Claude Code's synthetic API error turn carries `stop_reason: "stop_sequence"`.
        let api_error = line(json!({
            "type": "assistant",
            "isApiErrorMessage": true,
            "message": {"content": [{"type": "text", "text": "API Error: 529 Overloaded"}],
                        "stop_reason": "stop_sequence"},
        }));

        let cases: [(&str, String, bool, SubagentState, usize); 7] = [
            (
                "tool call pending",
                [prompt.clone(), tool.clone()].concat(),
                false,
                SubagentState::Running,
                2,
            ),
            (
                "awaiting next turn",
                [tool.clone(), result.clone()].concat(),
                false,
                SubagentState::Running,
                1,
            ),
            (
                "finished",
                [prompt.clone(), tool.clone(), result.clone(), done.clone()].concat(),
                false,
                SubagentState::Done,
                3,
            ),
            (
                "trailing attachment",
                [done.clone(), attachment].concat(),
                false,
                SubagentState::Done,
                1,
            ),
            (
                "api error",
                [tool.clone(), result.clone(), api_error].concat(),
                false,
                SubagentState::Failed,
                2,
            ),
            (
                "interrupted",
                [tool.clone(), interrupted].concat(),
                false,
                SubagentState::Stopped,
                1,
            ),
            (
                "partial first line dropped",
                [user(json!("stray")), done.clone()].concat(),
                true,
                SubagentState::Done,
                1,
            ),
        ];
        for (label, body, truncated, state, activity_len) in cases {
            assert_eq!(
                summarize_tail(body.as_bytes(), truncated).0,
                state,
                "{label}"
            );
            let activity = parse_activity(body.as_bytes(), truncated);
            assert_eq!(activity.len(), activity_len, "{label}: {activity:?}");
        }

        assert_eq!(
            parse_activity([prompt, tool, done].concat().as_bytes(), false),
            vec![
                SubagentActivity::Prompt("List the files".into()),
                SubagentActivity::Tool {
                    name: "Bash".into(),
                    detail: "ls -la".into()
                },
                SubagentActivity::Text("Found 2 files.".into()),
            ]
        );
    }

    fn notification(agent_id: &str, status: &str, at: &str) -> String {
        line(json!({
            "type": "user",
            "timestamp": at,
            "message": {"content": format!(
                "<task-notification>\n<task-id>{agent_id}</task-id>\n<tool-use-id>toolu_1</tool-use-id>\n<status>{status}</status>\n<summary>Agent stopped</summary>\n</task-notification>"
            )},
        }))
    }

    #[test]
    fn task_notifications_name_the_agent_and_its_end() {
        let at = "2026-10-01T15:26:00.000Z";
        let queued = line(
            json!({"type": "queue-operation", "content": "<task-notification><task-id>a1</task-id><status>completed</status>"}),
        );
        for (label, input, expected) in [
            (
                "completed",
                notification("a1", "completed", at),
                Some(("a1", SubagentState::Done)),
            ),
            (
                "killed",
                notification("a2", "killed", at),
                Some(("a2", SubagentState::Stopped)),
            ),
            (
                "failed",
                notification("a3", "failed", at),
                Some(("a3", SubagentState::Failed)),
            ),
            (
                "delivered to a busy parent",
                line(json!({
                    "type": "attachment",
                    "attachment": {
                        "type": "queued_command",
                        "timestamp": at,
                        "prompt": "<task-notification>\n<task-id>a4</task-id>\n<status>completed</status>\n</task-notification>",
                    },
                })),
                Some(("a4", SubagentState::Done)),
            ),
            ("queue copy ignored", queued, None),
            ("plain prompt", user(json!("hello")), None),
        ] {
            let got = parse_task_notification(input.trim_end().as_bytes());
            assert_eq!(
                got.as_ref().map(|(id, state, _)| (id.as_str(), *state)),
                expected,
                "{label}"
            );
            if let Some((_, _, when)) = got {
                assert_eq!(when, parse_timestamp(at), "{label}");
            }
        }
    }

    #[test]
    fn parent_log_skips_a_line_longer_than_one_read() {
        let done = notification("a1", "completed", "2026-10-01T15:26:00.000Z");
        let mut log = ParentLog::default();
        // A full window with no line end: skip it rather than wait forever.
        log.consume(&[b'x'; 16], true);
        assert!(log.skipping);
        // The oversized line ends, then a complete notification follows.
        let next = [b"xx\n".as_slice(), done.as_bytes()].concat();
        log.consume(&next, false);
        assert!(!log.skipping);
        assert_eq!(log.offset, 16 + next.len() as u64);
        assert_eq!(log.ended["a1"].0, SubagentState::Done);
        // A short read with no line end is a line still being written.
        log.consume(b"{\"partial", false);
        assert_eq!(log.offset, 16 + next.len() as u64);
        assert!(!log.skipping);
    }

    #[test]
    fn settle_trusts_newer_notifications_and_expires_orphans() {
        let last = parse_timestamp("2026-10-01T15:00:00.000Z").unwrap();
        let minutes = |m: u64| last + Duration::from_secs(m * 60);
        let ended = |state, at: SystemTime| HashMap::from([("a1".to_string(), (state, Some(at)))]);
        // (label, notification, parent busy, now, expected)
        let cases = [
            (
                "recent output",
                HashMap::new(),
                false,
                minutes(1),
                SubagentState::Running,
            ),
            (
                "silent, idle parent",
                HashMap::new(),
                false,
                minutes(20),
                SubagentState::Stopped,
            ),
            (
                "silent, busy parent",
                HashMap::new(),
                true,
                minutes(20),
                SubagentState::Running,
            ),
            (
                "completed",
                ended(SubagentState::Done, minutes(1)),
                true,
                minutes(1),
                SubagentState::Done,
            ),
            (
                "killed",
                ended(SubagentState::Stopped, minutes(1)),
                true,
                minutes(1),
                SubagentState::Stopped,
            ),
            (
                "resumed after ending",
                ended(SubagentState::Done, last - Duration::from_secs(60)),
                true,
                minutes(1),
                SubagentState::Running,
            ),
        ];
        for (label, ended, busy, now, expected) in cases {
            let mut subagents = vec![(
                Subagent {
                    agent_id: "a1".into(),
                    agent_type: "Explore".into(),
                    description: String::new(),
                    state: SubagentState::Running,
                    started_at: None,
                },
                Some(last),
            )];
            settle(&mut subagents, &ended, busy, now);
            assert_eq!(subagents[0].0.state, expected, "{label}");
        }
    }

    #[test]
    fn retain_recent_keeps_running_and_newest_finished() {
        let make = |id: usize, state| Subagent {
            agent_id: id.to_string(),
            agent_type: "Explore".into(),
            description: String::new(),
            state,
            started_at: None,
        };
        let mut subagents: Vec<Subagent> = (0..FINISHED_SHOWN + 3)
            .map(|id| {
                make(
                    id,
                    if id == 0 {
                        SubagentState::Running
                    } else {
                        SubagentState::Done
                    },
                )
            })
            .collect();
        retain_recent(&mut subagents);
        let ids: Vec<String> = subagents.iter().map(|s| s.agent_id.clone()).collect();
        let mut expected = vec!["0".to_string()];
        expected.extend((3..FINISHED_SHOWN + 3).map(|id| id.to_string()));
        assert_eq!(ids, expected);
    }

    #[test]
    fn scanner_finds_subagents_through_the_parent_transcript() {
        let store = tempfile::tempdir().unwrap();
        let sid = "b7ea66f4-5394-48b4-8498-2764c8a662a9";
        let project = store.path().join("projects").join("-workspace");
        let dir = project.join(sid).join("subagents");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(project.join(format!("{sid}.jsonl")), "").unwrap();
        let prompt =
            |at: &str| line(json!({"type": "user", "timestamp": at, "message": {"content": "go"}}));
        // a01 was spawned after a02, so it lists second.
        std::fs::write(
            dir.join("agent-a01.meta.json"),
            r#"{"agentType":"Explore","description":"find callers"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join("agent-a01.jsonl"),
            [
                prompt("2026-10-01T15:26:00.000Z"),
                assistant(json!([{"type": "text", "text": "done"}]), Some("end_turn")),
            ]
            .concat(),
        )
        .unwrap();
        let a02 = dir.join("agent-a02.jsonl");
        std::fs::write(&a02, prompt("2026-10-01T15:25:00.000Z")).unwrap();
        // Not a transcript, and a symlink a container could plant.
        std::fs::write(dir.join("notes.txt"), "").unwrap();
        std::os::unix::fs::symlink("/etc/hostname", dir.join("agent-a03.jsonl")).unwrap();

        let source = SubagentSource {
            instance_id: "inst".into(),
            store: store.path().to_path_buf(),
            session_id: sid.into(),
            parent_busy: false,
        };
        let now = parse_timestamp("2026-10-01T15:30:00.000Z").unwrap();
        let mut scanner = SubagentScanner::default();
        let summary =
            |scanner: &mut SubagentScanner| -> Vec<(String, String, String, SubagentState)> {
                scanner
                    .scan_at(std::slice::from_ref(&source), None, now)
                    .subagents["inst"]
                    .iter()
                    .map(|s| {
                        (
                            s.agent_id.clone(),
                            s.agent_type.clone(),
                            s.description.clone(),
                            s.state,
                        )
                    })
                    .collect()
            };
        let entry = |id: &str, kind: &str, description: &str, state| {
            (
                id.to_string(),
                kind.to_string(),
                description.to_string(),
                state,
            )
        };
        assert_eq!(
            summary(&mut scanner),
            vec![
                entry("a02", "agent", "", SubagentState::Running),
                entry("a01", "Explore", "find callers", SubagentState::Done),
            ]
        );

        // Only the focused subagent's activity is read, and only while it is listed.
        let focus = |agent: &str| ("inst".to_string(), agent.to_string());
        let scan = scanner.scan_at(std::slice::from_ref(&source), Some(&focus("a01")), now);
        assert_eq!(
            scan.focus,
            Some((
                focus("a01"),
                vec![
                    SubagentActivity::Prompt("go".into()),
                    SubagentActivity::Text("done".into())
                ]
            ))
        );
        let scan = scanner.scan_at(std::slice::from_ref(&source), Some(&focus("a03")), now);
        assert_eq!(scan.focus, None);

        // The parent hears a02 was killed; its own transcript never says so.
        let mut parent = std::fs::OpenOptions::new()
            .append(true)
            .open(project.join(format!("{sid}.jsonl")))
            .unwrap();
        std::io::Write::write_all(
            &mut parent,
            notification("a02", "killed", "2026-10-01T15:26:00.000Z").as_bytes(),
        )
        .unwrap();
        assert_eq!(summary(&mut scanner)[0].3, SubagentState::Stopped);

        // A grown transcript is re-read rather than served from the cache.
        let mut file = std::fs::OpenOptions::new().append(true).open(&a02).unwrap();
        std::io::Write::write_all(
            &mut file,
            assistant(json!([{"type": "text", "text": "ok"}]), Some("end_turn")).as_bytes(),
        )
        .unwrap();
        assert_eq!(summary(&mut scanner)[0].3, SubagentState::Done);

        let other = SubagentSource {
            session_id: "11111111-2222-3333-4444-555555555555".into(),
            ..source.clone()
        };
        assert!(scanner.scan(&[other], None).subagents.is_empty());
    }

    #[test]
    fn scanner_reads_every_directory_a_conversation_spoke_in() {
        let store = tempfile::tempdir().unwrap();
        let sid = "b7ea66f4-5394-48b4-8498-2764c8a662a9";
        for (cwd, agent) in [("-repo", "a01"), ("-repo-sub", "a02")] {
            let project = store.path().join("projects").join(cwd);
            let dir = project.join(sid).join("subagents");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(project.join(format!("{sid}.jsonl")), "").unwrap();
            std::fs::write(dir.join(format!("agent-{agent}.jsonl")), user(json!("go"))).unwrap();
        }
        let source = SubagentSource {
            instance_id: "inst".into(),
            store: store.path().to_path_buf(),
            session_id: sid.into(),
            parent_busy: true,
        };
        let scan = SubagentScanner::default().scan(&[source], None);
        let mut ids: Vec<&str> = scan.subagents["inst"]
            .iter()
            .map(|s| s.agent_id.as_str())
            .collect();
        ids.sort_unstable();
        assert_eq!(ids, ["a01", "a02"]);
    }
}
