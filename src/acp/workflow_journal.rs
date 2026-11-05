//! Follows a Claude `Workflow` run's `journal.jsonl`, which lists each agent
//! the run starts (`started`) and the result it returns (`result`).
//!
//! The journal, not the agent's transcript, ends a workflow agent: an agent
//! asked for structured output finishes on a tool call and never writes the
//! `end_turn` the sub-agent tailer completes on. Each agent's transcript is
//! still followed for progress (`spawn_progress_tailer`).

use std::collections::{HashMap, HashSet};
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::acp::event_sender::WeakEventSender;
use chrono::Utc;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::acp::background_agent::{preview, spawn_progress_tailer, ActiveAgents};
use crate::acp::state::{
    BackgroundAgentStatus, Event, WorkflowAgents, WorkflowLaunch, WorkflowRef,
};
use crate::acp::task_transcript::{
    TaskReadOwner, FALLBACK_POLL, MAX_LINE_BYTES, READ_CHUNK_BYTES, SAFETY_REREAD, WATCH_DEBOUNCE,
};
use crate::file_watch::{FileMatcher, FileWatchService, WatchSpec};

pub(crate) struct WorkflowJournal {
    pub launch: WorkflowLaunch,
    /// The `Workflow` tool call, which every agent of the run links to.
    pub tool_call_id: String,
    pub claude_home: PathBuf,
    pub known: WorkflowAgents,
    pub event_tx: WeakEventSender,
    pub bg_agents: Arc<ActiveAgents>,
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
    pub(crate) async fn run(
        mut self,
        ended: CancellationToken,
        stopped: CancellationToken,
        owner: TaskReadOwner,
    ) {
        let home = Arc::new(self.claude_home.clone());
        let requested: Arc<str> = self.launch.transcript_dir.as_str().into();
        let dir = loop {
            let home = Arc::clone(&home);
            let requested = Arc::clone(&requested);
            let resolved = owner
                .blocking(move || Ok(resolve_dir(&home, &requested)))
                .await;
            match resolved {
                Err(_) => return,
                Ok(dir) => match dir {
                    Dir::Ready(dir) => break dir,
                    Dir::Rejected => {
                        warn!(
                            target: "acp.protocol",
                            run_id = %self.launch.run_id,
                            "workflow transcript directory is outside the Claude home; not following its agents"
                        );
                        return self.settle_without_journal(ended, stopped, owner).await;
                    }
                    Dir::NotYet => tokio::select! {
                        biased;
                        () = stopped.cancelled() => return,
                        () = ended.cancelled() => return self.settle_without_journal(ended, stopped, owner).await,
                        () = tokio::time::sleep(FALLBACK_POLL) => {}
                    },
                },
            }
        };
        if owner.authority().bind_workflow_root(&dir).is_err() {
            return;
        }
        let journal = Arc::new(dir.join("journal.jsonl"));
        let subscription = owner.effect(|| {
            Ok(self.watch.subscribe_channel(
                WatchSpec {
                    dir: dir.clone(),
                    matcher: FileMatcher::Exact(journal.as_ref().clone()),
                    debounce: Some(WATCH_DEBOUNCE),
                },
                16,
            ))
        });
        let Ok(subscription) = subscription else {
            return;
        };
        let (mut changes, _subscription) = match subscription {
            Ok((changes, subscription)) => (changes, Some(subscription)),
            Err(_) => (tokio::sync::mpsc::channel(1).1, None),
        };
        let launched = std::mem::take(&mut self.known.launched);
        let completed = std::mem::take(&mut self.known.completed);
        let mut follower = Follower {
            launched,
            completed,
            journal: self,
            dir,
            owner,
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

impl WorkflowJournal {
    /// With no journal to read, the agents the log recorded as launched (a
    /// resumed run) are still settled when the run ends.
    async fn settle_without_journal(
        self,
        ended: CancellationToken,
        stopped: CancellationToken,
        owner: TaskReadOwner,
    ) {
        tokio::select! {
            biased;
            () = stopped.cancelled() => return,
            () = ended.cancelled() => {}
        }
        let mut unreported: Vec<&String> = self
            .known
            .launched
            .difference(&self.known.completed)
            .collect();
        unreported.sort();
        for agent_id in unreported {
            let witness = self.known.resources.get(agent_id).map(Arc::as_ref);
            if !owner
                .publish_resource(&self.event_tx, detached(agent_id), witness)
                .await
            {
                return;
            }
        }
    }
}

fn resolve_dir(claude_home: &Path, transcript_dir: &str) -> Dir {
    if transcript_dir.is_empty() {
        return Dir::Rejected;
    }
    let Ok(home) = std::fs::canonicalize(claude_home) else {
        return Dir::Rejected;
    };
    match std::fs::canonicalize(transcript_dir) {
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
    owner: TaskReadOwner,
    offset: u64,
    carry: Vec<u8>,
    /// Inside a line too long to buffer.
    skipping_line: bool,
    launched: HashSet<String>,
    completed: HashSet<String>,
    tailers: HashMap<String, (CancellationToken, Option<tokio::task::JoinHandle<()>>)>,
}

impl Follower {
    /// Handles every complete line appended since the last read. False once
    /// the session's event channel is gone.
    async fn read_new(&mut self, journal: &Arc<PathBuf>) -> bool {
        let path = Arc::clone(journal);
        let offset = self.offset;
        let opened = self
            .owner
            .blocking(move || {
                let Ok(mut file) = std::fs::File::open(path.as_ref()) else {
                    return Ok(None);
                };
                let len = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
                let start = if len < offset { 0 } else { offset };
                if file.seek(SeekFrom::Start(start)).is_err() {
                    return Ok(None);
                }
                Ok(Some((file, len, start)))
            })
            .await;
        let (mut file, len, start) = match opened {
            Ok(Some(opened)) => opened,
            Ok(None) => return true,
            Err(_) => return false,
        };
        if len < self.offset {
            self.carry.clear();
            self.skipping_line = false;
        }
        self.offset = start;
        let mut chunk = vec![0; READ_CHUNK_BYTES];
        loop {
            let read = self
                .owner
                .blocking(move || {
                    let read = file.read(&mut chunk);
                    Ok((file, chunk, read))
                })
                .await;
            let (returned_file, returned_chunk, read) = match read {
                Ok(read) => read,
                Err(_) => return false,
            };
            file = returned_file;
            chunk = returned_chunk;
            let n = match read {
                Ok(0) | Err(_) => return true,
                Ok(n) => n,
            };
            self.offset += n as u64;
            for line in Self::split_lines(&mut self.carry, &mut self.skipping_line, &chunk[..n]) {
                if !self.handle_line(&line).await {
                    return false;
                }
            }
        }
    }

    fn split_lines(carry: &mut Vec<u8>, skipping_line: &mut bool, mut rest: &[u8]) -> Vec<Vec<u8>> {
        let mut lines = Vec::new();
        while let Some(end) = rest.iter().position(|b| *b == b'\n') {
            if *skipping_line {
                *skipping_line = false;
            } else if carry.len() + end <= MAX_LINE_BYTES {
                carry.extend_from_slice(&rest[..end]);
                lines.push(std::mem::take(carry));
            }
            carry.clear();
            rest = &rest[end + 1..];
        }
        if !*skipping_line {
            if carry.len() + rest.len() > MAX_LINE_BYTES {
                carry.clear();
                *skipping_line = true;
            } else {
                carry.extend_from_slice(rest);
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
                let launch_event = if self.launched.insert(agent_id.to_string()) {
                    Some(Event::BackgroundAgentLaunched {
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
                    })
                } else {
                    None
                };
                if !self.completed.contains(agent_id) && !self.tailers.contains_key(agent_id) {
                    let stop = CancellationToken::new();
                    let retained = self.journal.known.resources.get(agent_id).cloned();
                    let authority = match self.owner.authority().workflow_child(
                        agent_id,
                        &output_file,
                        retained,
                    ) {
                        Ok(authority) => authority,
                        Err(_) => {
                            return launch_event.is_none() || self.send(launch_event.unwrap()).await
                        }
                    };
                    let tailer = spawn_progress_tailer(
                        (
                            agent_id.to_string(),
                            output_file.to_string_lossy().into_owned(),
                        ),
                        self.journal.event_tx.clone(),
                        self.journal.bg_agents.clone(),
                        stop.clone(),
                        authority,
                        launch_event,
                        self.owner.clone(),
                    )
                    .await;
                    let tailer = tailer.map(|(task, witness)| {
                        self.journal
                            .known
                            .resources
                            .insert(agent_id.to_string(), witness);
                        task
                    });
                    self.tailers.insert(agent_id.to_string(), (stop, tailer));
                } else if let Some(event) = launch_event {
                    return self.send(event).await;
                }
                true
            }
            Some("result") => {
                if !self.completed.insert(agent_id.to_string()) {
                    return true;
                }
                self.finish_tailer(agent_id).await;
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
            self.finish_tailer(&agent_id).await;
            let event = detached(&agent_id);
            self.completed.insert(agent_id);
            if !self.send(event).await {
                return;
            }
        }
    }

    /// Stops an agent's tailer and waits for its final progress, which then
    /// precedes the completion.
    async fn finish_tailer(&mut self, agent_id: &str) {
        if let Some((stop, tailer)) = self.tailers.remove(agent_id) {
            stop.cancel();
            if let Some(tailer) = tailer {
                let _ = tailer.await;
            }
        }
    }

    fn stop_tailers(&mut self) {
        for (stop, _) in self.tailers.values() {
            stop.cancel();
        }
        self.tailers.clear();
    }

    async fn send(&self, event: Event) -> bool {
        let witness = match &event {
            Event::BackgroundAgentProgress { agent_id, .. }
            | Event::BackgroundAgentCompleted { agent_id, .. } => {
                self.journal.known.resources.get(agent_id).map(Arc::as_ref)
            }
            _ => None,
        };
        self.owner
            .publish_resource(&self.journal.event_tx, event, witness)
            .await
    }
}

fn detached(agent_id: &str) -> Event {
    completed(
        agent_id,
        BackgroundAgentStatus::Detached,
        None,
        Some("workflow ended before this agent reported".into()),
    )
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
    #[test]
    fn lines_split_across_reads_are_joined() {
        let mut carry = Vec::new();
        let mut skipping_line = false;
        assert!(Follower::split_lines(&mut carry, &mut skipping_line, b"{\"a\":").is_empty());
        assert_eq!(
            Follower::split_lines(&mut carry, &mut skipping_line, b"1}\n{\"b\":2}\n{\"c"),
            [b"{\"a\":1}".to_vec(), b"{\"b\":2}".to_vec()]
        );
        assert_eq!(carry, b"{\"c");
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
#[cfg(test)]
mod real_journal_tests {
    use super::*;
    use crate::acp::event_sender::AcpEventData;
    use crate::acp::supervisor::test_support::managed::with_managed;
    use crate::acp::task_transcript::resource_test_support::{
        self as fixture, JournalPath, WorkflowFixture, RESULT_A1, STARTED_A1, STARTED_A2,
    };

    fn child_events(data: &[AcpEventData]) -> Vec<&AcpEventData> {
        data.iter()
            .filter(|d| {
                matches!(
                    d.event.as_ref(),
                    Event::BackgroundAgentLaunched { .. }
                        | Event::BackgroundAgentProgress { .. }
                        | Event::BackgroundAgentCompleted { .. }
                )
            })
            .collect()
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn journal_entries_launch_and_complete_agents() {
        with_managed(|scope| Box::pin(async move {
            let mut f = WorkflowFixture::new(scope, "", &[], &[], JournalPath::Inside).await;
            f.append_journal(&format!("{{\"type\":\"launched\"}}\n{STARTED_A1}{STARTED_A2}{RESULT_A1}{RESULT_A1}{}{}",
                "{\"type\":\"started\",\"agentId\":\"../escape\",\"label\":\"x\",\"phase\":\"Draft\"}\n",
                "{\"type\":\"progress\",\"agentId\":\"a1\"}\n"));
            let task = f.start().await;
            let data = f.finish(task).await;
            let events = child_events(&data);
            assert_eq!(events.len(), 4);
            let mut allocations = std::collections::HashMap::new();
            for (d, id, description, phase) in [
                (events[0], "a1", "draft:one", "Draft"),
                (events[1], "a2", "verify:one", "Verify"),
            ] {
                assert!(matches!(d.event.as_ref(), Event::BackgroundAgentLaunched {
                    agent_id, tool_call_id, description: actual, output_file, workflow: Some(run), ..
                } if agent_id == id && tool_call_id == "toolu_1" && actual == description
                    && output_file.ends_with(&format!("agent-{id}.jsonl"))
                    && run.run_id == "wf_1" && run.name == "drafts" && run.phase == phase));
                let witness = d.metadata.resource_witness.as_ref().expect("actual child allocation");
                assert_eq!(d.metadata.resource_ref, Some(witness.allocation));
                allocations.insert(id, witness.allocation);
            }
            assert!(matches!(events[2].event.as_ref(), Event::BackgroundAgentCompleted {
                agent_id, status: BackgroundAgentStatus::Completed, result: Some(result), ..
            } if agent_id == "a1" && result == "drafted"));
            assert!(matches!(events[3].event.as_ref(), Event::BackgroundAgentCompleted {
                agent_id, status: BackgroundAgentStatus::Detached, result: None, ..
            } if agent_id == "a2"));
            assert_eq!(events[2].metadata.resource_ref, Some(allocations["a1"]));
            assert_eq!(events[3].metadata.resource_ref, Some(allocations["a2"]));
        })).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_resumed_read_skips_what_the_log_already_has() {
        with_managed(|scope| {
            Box::pin(async move {
                let mut f = WorkflowFixture::new(
                    scope,
                    &format!("{STARTED_A1}{STARTED_A2}{RESULT_A1}"),
                    &["a1", "a2"],
                    &["a1"],
                    JournalPath::Inside,
                )
                .await;
                let a1 = f.known.resources["a1"].allocation;
                let a2 = f.known.resources["a2"].allocation;
                assert_ne!(a1, a2);
                let task = f.start().await;
                let data = f.finish(task).await;
                let events = child_events(&data);
                assert_eq!(
                    events.len(),
                    1,
                    "no replayed launch or repeated completed result"
                );
                assert!(
                    matches!(events[0].event.as_ref(), Event::BackgroundAgentCompleted {
                agent_id, status: BackgroundAgentStatus::Detached, result: None, ..
            } if agent_id == "a2")
                );
                assert_eq!(events[0].metadata.resource_ref, Some(a2));
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_result_written_as_the_run_ends_is_not_lost() {
        with_managed(|scope| Box::pin(async move {
            let mut f = WorkflowFixture::new(scope, "", &[], &[], JournalPath::Inside).await;
            f.append_journal(STARTED_A1);
            let task = f.start().await;
            let launched = fixture::next(&mut f.rx).await;
            assert!(matches!(launched.event.as_ref(), Event::BackgroundAgentLaunched { agent_id, .. } if agent_id == "a1"));
            let allocation = launched.metadata.resource_ref.expect("actual child allocation");
            f.append_journal(RESULT_A1);
            let data = f.finish(task).await;
            let events = child_events(&data);
            assert_eq!(events.len(), 1);
            assert!(matches!(events[0].event.as_ref(), Event::BackgroundAgentCompleted {
                agent_id, status: BackgroundAgentStatus::Completed, result: Some(result), ..
            } if agent_id == "a1" && result == "drafted"));
            assert_eq!(events[0].metadata.resource_ref, Some(allocation));
        })).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_dead_connection_is_not_kept_open_by_workflow_agents() {
        with_managed(|scope| {
            Box::pin(async move {
                let mut f = WorkflowFixture::new(scope, "", &[], &[], JournalPath::Inside).await;
                f.append_journal(STARTED_A1);
                let task = f.start().await;
                let launched = fixture::next(&mut f.rx).await;
                assert!(matches!(
                    launched.event.as_ref(),
                    Event::BackgroundAgentLaunched { .. }
                ));
                f.tx = None;
                assert!(fixture::closed(&mut f.rx).await.is_empty());
                fixture::join(task).await;
                f.released().await;
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_run_without_its_journal_settles_recorded_agents() {
        with_managed(|scope| {
            Box::pin(async move {
                for reject in [false, true] {
                    let mut f = WorkflowFixture::new(
                        scope,
                        &format!("{STARTED_A1}{STARTED_A2}{RESULT_A1}"),
                        &["a1", "a2"],
                        &["a1"],
                        JournalPath::Inside,
                    )
                    .await;
                    let allocation = f.known.resources["a2"].allocation;
                    std::fs::remove_dir_all(&f.dir).unwrap();
                    if reject {
                        let outside = scope.tempdir();
                        std::fs::write(outside.path().join("journal.jsonl"), STARTED_A1).unwrap();
                        // Same originally admitted selector; only its filesystem target changes.
                        #[cfg(unix)]
                        std::os::unix::fs::symlink(outside.path(), &f.dir).unwrap();
                        #[cfg(not(unix))]
                        panic!("managed native resource fixture requires Unix symlink support");
                    }
                    let task = f.start().await;
                    let data = f.finish(task).await;
                    let events = child_events(&data);
                    assert_eq!(events.len(), 1);
                    assert!(
                        matches!(events[0].event.as_ref(), Event::BackgroundAgentCompleted {
                    agent_id, status: BackgroundAgentStatus::Detached, result: None, ..
                } if agent_id == "a2")
                    );
                    assert_eq!(events[0].metadata.resource_ref, Some(allocation));
                }
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_directory_outside_the_claude_home_is_not_followed() {
        with_managed(|scope| {
            Box::pin(async move {
                for path in [
                    JournalPath::Outside,
                    JournalPath::Escaping,
                    JournalPath::Empty,
                    JournalPath::Home,
                ] {
                    // The rejected path is the actual RPC's original transcriptDir, not a modified selector.
                    let mut f = WorkflowFixture::new(scope, STARTED_A1, &[], &[], path).await;
                    let task = f.start().await;
                    let data = f.finish(task).await;
                    assert!(
                        child_events(&data).is_empty(),
                        "rejected directory must not launch/read an agent"
                    );
                    assert!(
                        f.known.resources.is_empty(),
                        "bootstrap did not manufacture child custody"
                    );
                }
            })
        })
        .await;
    }
}
