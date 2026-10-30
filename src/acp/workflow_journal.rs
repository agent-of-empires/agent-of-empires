//! Follows a Claude `Workflow` run's `journal.jsonl`, which lists each agent
//! the run starts (`started`) and the result it returns (`result`).
//!
//! The journal, not the agent's transcript, ends a workflow agent: an agent
//! asked for structured output finishes on a tool call and never writes the
//! `end_turn` the sub-agent tailer completes on. Each agent's transcript is
//! still followed for progress (`spawn_progress_tailer`).

use std::collections::{HashMap, HashSet};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use tokio::sync::mpsc::WeakSender;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::acp::background_agent::{preview, spawn_progress_tailer};
use crate::acp::state::{
    BackgroundAgentStatus, Event, WorkflowAgents, WorkflowLaunch, WorkflowRef,
};
use crate::acp::task_transcript::{
    FALLBACK_POLL, MAX_LINE_BYTES, READ_CHUNK_BYTES, SAFETY_REREAD, WATCH_DEBOUNCE,
};
use crate::file_watch::{FileMatcher, FileWatchService, WatchSpec};

pub(crate) struct WorkflowJournal {
    pub launch: WorkflowLaunch,
    /// The `Workflow` tool call, which every agent of the run links to.
    pub tool_call_id: String,
    pub claude_home: PathBuf,
    pub known: WorkflowAgents,
    pub event_tx: WeakSender<Event>,
    /// The between-prompt tracker's active sub-agent set.
    pub bg_agents: Arc<Mutex<HashSet<String>>>,
    pub watch: Arc<FileWatchService>,
}

enum Dir {
    Ready(PathBuf),
    NotYet,
    Rejected,
}

impl WorkflowJournal {
    /// Follows the journal until `ended` fires (the run is over: what is left
    /// is read, then agents that never reported are detached) or `stopped`
    /// does (another launch took over the task).
    pub(crate) async fn run(self, ended: CancellationToken, stopped: CancellationToken) {
        let dir = loop {
            match resolve_dir(&self.claude_home, &self.launch.transcript_dir).await {
                Dir::Ready(dir) => break dir,
                Dir::Rejected => {
                    warn!(
                        target: "acp.protocol",
                        run_id = %self.launch.run_id,
                        "workflow transcript directory is outside the Claude home; not following its agents"
                    );
                    return;
                }
                Dir::NotYet => tokio::select! {
                    () = ended.cancelled() => return,
                    () = stopped.cancelled() => return,
                    () = tokio::time::sleep(FALLBACK_POLL) => {}
                },
            }
        };
        let journal = dir.join("journal.jsonl");
        let (mut changes, _subscription) = match self.watch.subscribe_channel(
            WatchSpec {
                dir: dir.clone(),
                matcher: FileMatcher::Exact(journal.clone()),
                debounce: Some(WATCH_DEBOUNCE),
            },
            16,
        ) {
            Ok((changes, subscription)) => (changes, Some(subscription)),
            Err(_) => (tokio::sync::mpsc::channel(1).1, None),
        };
        let mut follower = Follower {
            launched: self.known.launched.clone(),
            completed: self.known.completed.clone(),
            journal: self,
            dir,
            offset: 0,
            carry: Vec::new(),
            skipping_line: false,
            tailers: HashMap::new(),
        };
        let mut watched = true;
        loop {
            if follower.journal.event_tx.upgrade().is_none() || !follower.read_new(&journal).await {
                follower.stop_tailers();
                return;
            }
            tokio::select! {
                biased;
                () = stopped.cancelled() => {
                    follower.stop_tailers();
                    return;
                }
                () = ended.cancelled() => {
                    if follower.read_new(&journal).await {
                        follower.detach_unreported().await;
                    }
                    follower.stop_tailers();
                    return;
                }
                change = changes.recv(), if watched => watched = change.is_some(),
                () = tokio::time::sleep(if watched { SAFETY_REREAD } else { FALLBACK_POLL }) => {}
            }
        }
    }
}

/// `transcript_dir` comes from the agent process, so it is only followed
/// inside the Claude home.
async fn resolve_dir(claude_home: &Path, transcript_dir: &str) -> Dir {
    if transcript_dir.is_empty() {
        return Dir::Rejected;
    }
    let Ok(home) = tokio::fs::canonicalize(claude_home).await else {
        return Dir::Rejected;
    };
    match tokio::fs::canonicalize(transcript_dir).await {
        Ok(dir) if dir.starts_with(&home) && dir != home => Dir::Ready(dir),
        Ok(_) => Dir::Rejected,
        Err(e) if e.kind() == ErrorKind::NotFound => Dir::NotYet,
        Err(_) => Dir::Rejected,
    }
}

/// Agent ids name transcript files, so only plain ids are accepted.
fn valid_agent_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// What a `result` entry says, short enough for the panel.
fn result_text(result: &serde_json::Value) -> Option<String> {
    let text = match result {
        serde_json::Value::Null => return None,
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(o) => match ["summary", "title"]
            .iter()
            .find_map(|key| o.get(*key).and_then(|v| v.as_str()))
        {
            Some(s) => s.to_string(),
            None => result.to_string(),
        },
        other => other.to_string(),
    };
    Some(preview(&text)).filter(|t| !t.is_empty())
}

struct Follower {
    journal: WorkflowJournal,
    dir: PathBuf,
    offset: u64,
    carry: Vec<u8>,
    /// Inside a line too long to buffer.
    skipping_line: bool,
    launched: HashSet<String>,
    completed: HashSet<String>,
    tailers: HashMap<String, CancellationToken>,
}

impl Follower {
    /// Handles every complete line appended since the last read. False once
    /// the session's event channel is gone.
    async fn read_new(&mut self, journal: &Path) -> bool {
        let Ok(mut file) = tokio::fs::File::open(journal).await else {
            return true;
        };
        let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
        if len < self.offset {
            // Rewritten: read it again; the sets keep this idempotent.
            self.offset = 0;
            self.carry.clear();
            self.skipping_line = false;
        }
        if file.seek(SeekFrom::Start(self.offset)).await.is_err() {
            return true;
        }
        let mut chunk = vec![0; READ_CHUNK_BYTES];
        loop {
            let n = match file.read(&mut chunk).await {
                Ok(0) | Err(_) => return true,
                Ok(n) => n,
            };
            self.offset += n as u64;
            for line in self.split_lines(&chunk[..n]) {
                if !self.handle_line(&line).await {
                    return false;
                }
            }
        }
    }

    fn split_lines(&mut self, mut rest: &[u8]) -> Vec<Vec<u8>> {
        let mut lines = Vec::new();
        while let Some(end) = rest.iter().position(|b| *b == b'\n') {
            if self.skipping_line {
                self.skipping_line = false;
            } else if self.carry.len() + end <= MAX_LINE_BYTES {
                self.carry.extend_from_slice(&rest[..end]);
                lines.push(std::mem::take(&mut self.carry));
            }
            self.carry.clear();
            rest = &rest[end + 1..];
        }
        if !self.skipping_line {
            if self.carry.len() + rest.len() > MAX_LINE_BYTES {
                self.carry.clear();
                self.skipping_line = true;
            } else {
                self.carry.extend_from_slice(rest);
            }
        }
        lines
    }

    async fn handle_line(&mut self, line: &[u8]) -> bool {
        let Ok(entry) = serde_json::from_slice::<serde_json::Value>(line) else {
            return true;
        };
        let str_field = |key: &str| entry.get(key).and_then(|v| v.as_str());
        let Some(agent_id) = str_field("agentId").filter(|id| valid_agent_id(id)) else {
            return true;
        };
        match str_field("type") {
            Some("started") => {
                let output_file = self.dir.join(format!("agent-{agent_id}.jsonl"));
                if self.launched.insert(agent_id.to_string()) {
                    let launched = Event::BackgroundAgentLaunched {
                        agent_id: agent_id.to_string(),
                        tool_call_id: self.journal.tool_call_id.clone(),
                        description: str_field("label").unwrap_or("workflow agent").to_string(),
                        prompt: String::new(),
                        model: String::new(),
                        output_file: output_file.to_string_lossy().into_owned(),
                        workflow: Some(WorkflowRef {
                            run_id: self.journal.launch.run_id.clone(),
                            name: self.journal.launch.name.clone(),
                            phase: str_field("phase").unwrap_or_default().to_string(),
                        }),
                        started_at: Utc::now(),
                    };
                    if !self.send(launched).await {
                        return false;
                    }
                }
                if !self.completed.contains(agent_id) && !self.tailers.contains_key(agent_id) {
                    let stop = CancellationToken::new();
                    self.tailers.insert(agent_id.to_string(), stop.clone());
                    let _ = spawn_progress_tailer(
                        agent_id.to_string(),
                        output_file.to_string_lossy().into_owned(),
                        self.journal.event_tx.clone(),
                        self.journal.bg_agents.clone(),
                        stop,
                    );
                }
                true
            }
            Some("result") => {
                if !self.completed.insert(agent_id.to_string()) {
                    return true;
                }
                if let Some(stop) = self.tailers.remove(agent_id) {
                    stop.cancel();
                }
                let result = entry.get("result").and_then(result_text);
                self.send(completed(
                    agent_id,
                    BackgroundAgentStatus::Completed,
                    result,
                    None,
                ))
                .await
            }
            _ => true,
        }
    }

    async fn detach_unreported(&mut self) {
        let mut unreported: Vec<String> =
            self.launched.difference(&self.completed).cloned().collect();
        unreported.sort();
        for agent_id in unreported {
            let event = completed(
                &agent_id,
                BackgroundAgentStatus::Detached,
                None,
                Some("workflow ended before this agent reported".into()),
            );
            self.completed.insert(agent_id);
            if !self.send(event).await {
                return;
            }
        }
    }

    fn stop_tailers(&mut self) {
        for (_, stop) in self.tailers.drain() {
            stop.cancel();
        }
    }

    async fn send(&self, event: Event) -> bool {
        match self.journal.event_tx.upgrade() {
            Some(tx) => tx.send(event).await.is_ok(),
            None => false,
        }
    }
}

fn completed(
    agent_id: &str,
    status: BackgroundAgentStatus,
    result: Option<String>,
    warning: Option<String>,
) -> Event {
    Event::BackgroundAgentCompleted {
        agent_id: agent_id.to_string(),
        status,
        tools: Vec::new(),
        result,
        warning,
        ended_at: Utc::now(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct Fixture {
        home: tempfile::TempDir,
        dir: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let home = tempfile::tempdir().unwrap();
            let dir = home.path().join("projects/p/s/subagents/workflows/wf_1");
            std::fs::create_dir_all(&dir).unwrap();
            Self { home, dir }
        }

        fn append(&self, lines: &[&str]) {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.dir.join("journal.jsonl"))
                .unwrap();
            for line in lines {
                writeln!(file, "{line}").unwrap();
            }
        }

        fn journal(
            &self,
            transcript_dir: &Path,
            known: WorkflowAgents,
            tx: &tokio::sync::mpsc::Sender<Event>,
        ) -> WorkflowJournal {
            WorkflowJournal {
                launch: WorkflowLaunch {
                    run_id: "wf_1".into(),
                    name: "drafts".into(),
                    summary: None,
                    transcript_dir: transcript_dir.to_string_lossy().into_owned(),
                },
                tool_call_id: "toolu_wf".into(),
                claude_home: self.home.path().to_path_buf(),
                known,
                event_tx: tx.downgrade(),
                bg_agents: Arc::default(),
                watch: FileWatchService::noop(),
            }
        }
    }

    const STARTED_A1: &str =
        r#"{"type":"started","agentId":"a1","label":"draft:one","phase":"Draft","key":"k1"}"#;
    const STARTED_A2: &str =
        r#"{"type":"started","agentId":"a2","label":"verify:one","phase":"Verify","key":"k2"}"#;
    const RESULT_A1: &str = r#"{"type":"result","agentId":"a1","key":"k1","result":{"path":"/tmp/x.md","summary":"drafted"}}"#;

    /// Runs the journal to completion with the run already over, and returns
    /// every event it (and its tailers) published.
    async fn run_ended(
        journal: WorkflowJournal,
        tx: tokio::sync::mpsc::Sender<Event>,
        mut rx: tokio::sync::mpsc::Receiver<Event>,
    ) -> Vec<Event> {
        let ended = CancellationToken::new();
        ended.cancel();
        journal.run(ended, CancellationToken::new()).await;
        drop(tx);
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        events
    }

    fn summarize(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .map(|event| match event {
                Event::BackgroundAgentLaunched {
                    agent_id,
                    tool_call_id,
                    description,
                    output_file,
                    workflow: Some(w),
                    ..
                } => {
                    assert!(output_file.ends_with(&format!("agent-{agent_id}.jsonl")));
                    format!(
                        "launched {agent_id} {tool_call_id} {description} {}/{}/{}",
                        w.run_id, w.name, w.phase
                    )
                }
                Event::BackgroundAgentCompleted {
                    agent_id,
                    status,
                    result,
                    ..
                } => format!("completed {agent_id} {status:?} {result:?}"),
                other => format!("{other:?}"),
            })
            .collect()
    }

    #[tokio::test]
    async fn journal_entries_launch_and_complete_agents() {
        let fx = Fixture::new();
        fx.append(&[
            r#"{"type":"launched"}"#,
            STARTED_A1,
            STARTED_A2,
            RESULT_A1,
            // Ignored: duplicate result, path-like agent id, unknown entry.
            RESULT_A1,
            r#"{"type":"started","agentId":"../escape","label":"x","phase":"Draft"}"#,
            r#"{"type":"progress","agentId":"a1"}"#,
        ]);
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let journal = fx.journal(&fx.dir, WorkflowAgents::default(), &tx);
        assert_eq!(
            summarize(&run_ended(journal, tx, rx).await),
            [
                "launched a1 toolu_wf draft:one wf_1/drafts/Draft",
                "launched a2 toolu_wf verify:one wf_1/drafts/Verify",
                r#"completed a1 Completed Some("drafted")"#,
                "completed a2 Detached None",
            ]
        );
    }

    #[tokio::test]
    async fn a_resumed_read_skips_what_the_log_already_has() {
        let fx = Fixture::new();
        fx.append(&[STARTED_A1, STARTED_A2, RESULT_A1]);
        let known = WorkflowAgents {
            launched: HashSet::from(["a1".to_string(), "a2".to_string()]),
            completed: HashSet::from(["a1".to_string()]),
        };
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        let journal = fx.journal(&fx.dir, known, &tx);
        assert_eq!(
            summarize(&run_ended(journal, tx, rx).await),
            ["completed a2 Detached None"]
        );
    }

    #[tokio::test]
    async fn a_result_written_as_the_run_ends_is_not_lost() {
        let fx = Fixture::new();
        fx.append(&[STARTED_A1]);
        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        let journal = fx.journal(&fx.dir, WorkflowAgents::default(), &tx);
        let ended = CancellationToken::new();
        let run = tokio::spawn(journal.run(ended.clone(), CancellationToken::new()));
        assert!(matches!(
            rx.recv().await,
            Some(Event::BackgroundAgentLaunched { .. })
        ));
        fx.append(&[RESULT_A1]);
        ended.cancel();
        run.await.unwrap();
        drop(tx);
        let mut rest = Vec::new();
        while let Some(event) = rx.recv().await {
            rest.push(event);
        }
        assert_eq!(
            summarize(&rest),
            [r#"completed a1 Completed Some("drafted")"#]
        );
    }

    /// The supervisor's drain recovers only once every sender of a dead
    /// connection is gone, so nothing a run starts may hold one.
    #[tokio::test]
    async fn a_dead_connection_is_not_kept_open_by_workflow_agents() {
        let fx = Fixture::new();
        fx.append(&[STARTED_A1]);
        std::fs::write(fx.dir.join("agent-a1.jsonl"), "").unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(32);
        let journal = fx.journal(&fx.dir, WorkflowAgents::default(), &tx);
        let run = tokio::spawn(journal.run(CancellationToken::new(), CancellationToken::new()));
        assert!(matches!(
            rx.recv().await,
            Some(Event::BackgroundAgentLaunched { .. })
        ));
        // The connection ends with the run unfinished; the receiver stays open.
        drop(tx);
        let closed = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while rx.recv().await.is_some() {}
        })
        .await;
        assert!(closed.is_ok(), "a workflow reader kept the channel open");
        tokio::time::timeout(std::time::Duration::from_secs(10), run)
            .await
            .expect("the journal follower outlived its connection")
            .unwrap();
    }

    #[tokio::test]
    async fn a_directory_outside_the_claude_home_is_not_followed() {
        let fx = Fixture::new();
        fx.append(&[STARTED_A1]);
        let outside = tempfile::tempdir().unwrap();
        std::fs::copy(
            fx.dir.join("journal.jsonl"),
            outside.path().join("journal.jsonl"),
        )
        .unwrap();
        let escaping = fx
            .dir
            .join("../../../../../..")
            .join(outside.path().strip_prefix("/").unwrap());
        for dir in [
            outside.path().to_path_buf(),
            escaping,
            PathBuf::new(),
            fx.home.path().to_path_buf(),
        ] {
            let (tx, rx) = tokio::sync::mpsc::channel(32);
            let journal = fx.journal(&dir, WorkflowAgents::default(), &tx);
            assert!(run_ended(journal, tx, rx).await.is_empty(), "{dir:?}");
        }
    }

    #[test]
    fn lines_split_across_reads_are_joined() {
        let fx = Fixture::new();
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let mut follower = Follower {
            journal: fx.journal(&fx.dir, WorkflowAgents::default(), &tx),
            dir: fx.dir.clone(),
            offset: 0,
            carry: Vec::new(),
            skipping_line: false,
            launched: HashSet::new(),
            completed: HashSet::new(),
            tailers: HashMap::new(),
        };
        assert!(follower.split_lines(b"{\"a\":").is_empty());
        assert_eq!(
            follower.split_lines(b"1}\n{\"b\":2}\n{\"c"),
            [b"{\"a\":1}".to_vec(), b"{\"b\":2}".to_vec()]
        );
        assert_eq!(follower.carry, b"{\"c");
    }

    #[test]
    fn result_text_prefers_a_summary() {
        for (result, expect) in [
            (serde_json::json!({"summary": "s", "title": "t"}), Some("s")),
            (serde_json::json!({"title": "t", "fixes": []}), Some("t")),
            (serde_json::json!("plain"), Some("plain")),
            (serde_json::json!({"n": 1}), Some(r#"{"n":1}"#)),
            (serde_json::Value::Null, None),
            (serde_json::json!(""), None),
        ] {
            assert_eq!(result_text(&result).as_deref(), expect, "{result}");
        }
    }
}
