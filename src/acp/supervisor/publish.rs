//! Supervisor-authored events, each allocated the session's next seq.

use tracing::info;

use super::{
    publish_next_persisted, publish_with_next_seq, BroadcastSink, PromptDisposition,
    PublishedPrompt, SeqMap, Supervisor, WorkerKind,
};
use crate::acp::approvals::ApprovalDecision;
use crate::acp::elicitations::ElicitationOutcome;
use crate::acp::event_store::{AttachmentBlob, UnresolvedBackgroundAgentLaunch};
use crate::acp::state::{BackgroundAgentStatus, Event};

impl<S: BroadcastSink> Supervisor<S> {
    pub(super) fn publish_next(&self, session_id: &str, event: &Event) -> u64 {
        publish_next_persisted(self.sink.as_ref(), &self.next_seqs, session_id, event).0
    }

    /// Publish an `AgentStartupError` for a session whose worker never came online.
    pub fn publish_startup_error(&self, session_id: &str, message: String) {
        self.publish_next(session_id, &Event::AgentStartupError { message });
    }

    /// Publish `Stopped { reason }` only if `expected_seq` is still the
    /// session's latest seq, so a repair decided from the log cannot land
    /// after something newer.
    pub fn publish_stopped_if_seq(
        &self,
        session_id: &str,
        reason: &str,
        expected_seq: u64,
    ) -> bool {
        let counter = super::session_seq(&self.next_seqs, session_id);
        let persisted = super::sink::blocking_io(|| {
            let mut last = super::lock_recover(&counter);
            if *last != expected_seq {
                return None;
            }
            *last = last.saturating_add(1);
            Some(self.sink.queue_persisted(
                session_id,
                *last,
                &Event::Stopped {
                    reason: reason.to_string(),
                },
            ))
        });
        let Some(persisted) = persisted else {
            return false;
        };
        // A failed save is already broadcast as an AgentStartupError.
        persisted.wait();
        true
    }

    pub fn publish_agent_switched(
        &self,
        session_id: &str,
        from: String,
        to: String,
        reason: String,
    ) -> u64 {
        self.publish_next(session_id, &Event::AgentSwitched { from, to, reason })
    }

    pub fn publish_conversation_summary(
        &self,
        session_id: &str,
        text: String,
        summarized_until_seq: u64,
    ) {
        self.publish_next(
            session_id,
            &Event::ConversationSummary {
                text,
                summarized_until_seq,
            },
        );
    }

    /// `resets_at` is when the resume fired, not necessarily a reset the agent reported.
    pub fn publish_rate_limit_auto_resumed(
        &self,
        session_id: &str,
        resets_at: chrono::DateTime<chrono::Utc>,
        manual: bool,
    ) -> u64 {
        self.publish_next(
            session_id,
            &Event::RateLimitAutoResumed { resets_at, manual },
        )
    }

    /// Close a turn that was in flight when the previous daemon died.
    pub fn synthesize_stopped_for_orphan(&self, session_id: &str, reason: &str) {
        let seq = self.publish_next(
            session_id,
            &Event::Stopped {
                reason: reason.to_string(),
            },
        );
        info!(
            target: "acp.supervisor",
            session = %session_id,
            seq,
            %reason,
            "publishing synthetic Stopped for orphaned in-flight turn"
        );
    }

    pub async fn publish_user_prompt(&self, session_id: &str, text: String) -> PromptDisposition {
        self.publish_user_prompt_with_attachments(session_id, text, &[], None, false, None, None)
            .await
            .disposition
    }

    /// Record a `UserPromptSent` (attachments stored as blobs keyed to its seq)
    /// and decide how the caller routes the text. `synthesized` marks a
    /// daemon-queued rate-limit continuation rather than text the user just
    /// typed, which the transcript renders no row for (#4040). `classified_as`
    /// is the profile a caller already cut the turn with; `None` looks it up.
    // Each argument is a separate field of the recorded event or of how it is routed, and the
    // callers pass them positionally from different sources; a struct would only rename them.
    #[allow(clippy::too_many_arguments)]
    pub async fn publish_user_prompt_with_attachments(
        &self,
        session_id: &str,
        text: String,
        attachments: &[AttachmentBlob],
        prompt_id: Option<String>,
        synthesized: bool,
        origin_plugin: Option<String>,
        classified_as: Option<&'static crate::acp::agent_profiles::AgentProfile>,
    ) -> PublishedPrompt {
        // Before any await or store, so an idle stop cannot claim the worker in between.
        super::lock_recover(&self.lifecycle).note_admission(session_id);
        let profile = match classified_as {
            Some(profile) => profile,
            None => self.profile_for_session(session_id, &text).await,
        };
        let is_clear = profile.is_clear_command(&text);
        let disposition = if is_clear && profile.clear_requires_driven_reset {
            PromptDisposition::ResetContext
        } else {
            PromptDisposition::Forward
        };
        let unrecorded = PublishedPrompt {
            disposition,
            seq: None,
        };
        let event = Event::UserPromptSent {
            text,
            attachments: attachments
                .iter()
                .map(|blob| crate::daemon::PromptAttachmentRef {
                    id: blob.id.clone(),
                    kind: blob.kind,
                    mime_type: blob.mime_type.clone(),
                    name: blob.name.clone(),
                    size: blob.data.len() as u64,
                })
                .collect(),
            prompt_id,
            synthesized,
            origin_plugin,
        };
        let (seq, persisted) = publish_with_next_seq(&self.next_seqs, session_id, |seq| {
            (
                seq,
                self.sink.queue_prompt(session_id, seq, attachments, &event),
            )
        });
        if !persisted.wait() {
            // Roll back so refs never point at blobs that were not published.
            self.sink.delete_attachments_for_seq(session_id, seq);
            return unrecorded;
        }
        if is_clear && disposition == PromptDisposition::Forward {
            self.publish_next(session_id, &Event::SessionCleared);
        }
        PublishedPrompt {
            disposition,
            seq: Some(seq),
        }
    }

    /// Publish a "Send diff comments" submission; never treated as a clear command.
    pub async fn publish_user_diff_comments_prompt(
        &self,
        session_id: &str,
        intro: String,
        outro: String,
        is_multi_repo: bool,
        comments: Vec<crate::acp::state::DiffComment>,
        assembled_markdown: String,
    ) {
        super::lock_recover(&self.lifecycle).note_admission(session_id);
        self.publish_next(
            session_id,
            &Event::UserDiffCommentsPrompt {
                intro,
                outro,
                is_multi_repo,
                comments,
                assembled_markdown,
            },
        );
    }

    /// The session's agent profile, read from its worker or registry record. A session with
    /// neither predates the key and runs `claude`.
    async fn profile_for_session(
        &self,
        session_id: &str,
        text: &str,
    ) -> &'static crate::acp::agent_profiles::AgentProfile {
        use crate::acp::agent_profiles::resolve;
        let Some((agent_key, source_profile, cwd)) = self.worker_agent(session_id).await else {
            return resolve("claude");
        };
        // Only a `/clear` or `/new` can match a declared alias, so other prompts skip the
        // configuration read.
        if !crate::acp::agent_profiles::could_be_declared_alias(text) {
            return resolve(&agent_key);
        }
        super::SessionAgent::Worker(agent_key)
            .classify(&source_profile.unwrap_or_default(), &cwd)
            .await
            .profile
    }

    /// The registry key, source profile and cwd of the session's live runner, else of its
    /// registry record while that record's runner is alive. A dead runner's record says
    /// nothing about the agent the next spawn runs.
    async fn worker_agent(
        &self,
        session_id: &str,
    ) -> Option<(String, Option<String>, std::path::PathBuf)> {
        if let Some(handle) = self.workers.lock().await.get(session_id) {
            if let WorkerKind::Runner { spawn_config } = &handle.kind {
                return Some((
                    spawn_config.agent_key.clone(),
                    spawn_config.source_profile.clone(),
                    spawn_config.cwd.clone(),
                ));
            }
        }
        let session_id = session_id.to_string();
        tokio::task::spawn_blocking(move || live_record_agent(&session_id))
            .await
            .ok()
            .flatten()
            .map(|record| (record.agent_key, record.source_profile, record.cwd))
    }

    /// The session's worker agent key; see [`Self::worker_agent`].
    pub(crate) async fn worker_agent_key(&self, session_id: &str) -> Option<String> {
        #[cfg(test)]
        let gate = self.agent_key_gate.lock().unwrap().take();
        #[cfg(test)]
        if let Some(gate) = &gate {
            gate.before_read.wait().await;
            gate.before_read.wait().await;
        }
        let key = self.worker_agent(session_id).await.map(|(key, ..)| key);
        #[cfg(test)]
        if let Some(gate) = gate {
            gate.after_read.wait().await;
            gate.after_read.wait().await;
        }
        key
    }

    /// [`Self::worker_agent_key`] for each of `session_ids` that has one, under one workers lock,
    /// reading registry records off the runtime.
    pub(crate) async fn worker_agent_keys(
        &self,
        session_ids: Vec<String>,
    ) -> std::collections::HashMap<String, String> {
        let mut keys = std::collections::HashMap::new();
        let mut unresolved = Vec::new();
        {
            let workers = self.workers.lock().await;
            for id in session_ids {
                match workers.get(&id).map(|handle| &handle.kind) {
                    Some(WorkerKind::Runner { spawn_config }) => {
                        keys.insert(id, spawn_config.agent_key.clone());
                    }
                    _ => unresolved.push(id),
                }
            }
        }
        let recorded = tokio::task::spawn_blocking(move || {
            unresolved
                .into_iter()
                .filter_map(|id| live_record_agent(&id).map(|record| (id, record.agent_key)))
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        keys.extend(recorded);
        keys
    }

    /// The session's agent registry key, else `claude` for records that predate the key.
    pub(super) async fn agent_key_for_session(&self, session_id: &str) -> String {
        self.worker_agent_key(session_id)
            .await
            .unwrap_or_else(|| "claude".to_string())
    }

    pub(super) fn cancel_orphaned_requests(&self, session_id: &str) {
        cancel_orphaned_requests_on(&*self.sink, &self.next_seqs, session_id);
    }

    /// Detach background sub-agents the previous daemon left outstanding.
    /// See [`detach_orphaned_background_agents_on`].
    pub(super) fn detach_orphaned_background_agents(&self, session_id: &str) {
        detach_orphaned_background_agents_on(
            &*self.sink,
            &self.next_seqs,
            session_id,
            WORKER_REPLACED_DETACH_WARNING,
        );
    }
}

/// Why `spawn` and the drain respawn path detach: both install a fresh worker
/// over the one whose tailer died.
pub(super) const WORKER_REPLACED_DETACH_WARNING: &str =
    "the worker that was tracking this sub-agent was replaced; tracking stopped";

/// Detach background sub-agents left running by a dead worker: its tailer died
/// with it, so nothing will ever report their outcome and the panel would show
/// them running forever (#4001). `warning` is caller-supplied because the
/// sites differ in why tracking stopped. `attach` instead resumes what it can;
/// see [`collect_resumable_background_agent_launches`].
pub(super) fn detach_orphaned_background_agents_on<S: BroadcastSink>(
    sink: &S,
    next_seqs: &SeqMap,
    session_id: &str,
    warning: &str,
) {
    for task_id in sink.unresolved_background_task_ids(session_id) {
        publish_next_persisted(
            sink,
            next_seqs,
            session_id,
            &Event::BackgroundTaskFinished {
                task_id,
                ended_at: Some(chrono::Utc::now()),
            },
        );
    }
    let stale_ids = sink.unresolved_background_agent_ids(session_id);
    if stale_ids.is_empty() {
        return;
    }
    info!(
        target: "acp.supervisor",
        session = %session_id,
        stale = stale_ids.len(),
        "detaching background sub-agents orphaned by daemon restart"
    );
    for agent_id in stale_ids {
        publish_background_agent_detached(sink, next_seqs, session_id, agent_id, warning);
    }
}

fn publish_background_agent_detached<S: BroadcastSink>(
    sink: &S,
    next_seqs: &SeqMap,
    session_id: &str,
    agent_id: String,
    warning: &str,
) {
    publish_next_persisted(
        sink,
        next_seqs,
        session_id,
        &Event::BackgroundAgentCompleted {
            agent_id,
            status: BackgroundAgentStatus::Detached,
            tools: Vec::new(),
            result: None,
            warning: Some(warning.to_string()),
            ended_at: chrono::Utc::now(),
        },
    );
}

/// The attach-path counterpart of [`detach_orphaned_background_agents_on`]:
/// the worker just attached is provably alive, so a sub-agent it launched may
/// still be running. Detaches only the launches with no transcript path to
/// resume from and returns the rest for `AcpClient::resume_background_tailing`.
/// Sync so the query and those publishes can run before the drain starts,
/// while the resume send stays an await outside the caller's lock.
pub(super) fn collect_resumable_background_agent_launches<S: BroadcastSink>(
    sink: &S,
    next_seqs: &SeqMap,
    session_id: &str,
) -> Vec<UnresolvedBackgroundAgentLaunch> {
    let (resumable, untrackable): (Vec<_>, Vec<_>) = sink
        .unresolved_background_agent_launches(session_id)
        .into_iter()
        .partition(|l| !l.output_file.is_empty());
    for launch in untrackable {
        publish_background_agent_detached(
            sink,
            next_seqs,
            session_id,
            launch.agent_id,
            "session reattached before this sub-agent finished; tracking stopped",
        );
    }
    resumable
}

/// Cancel approvals and elicitations a dead worker left unresolved in the log:
/// their responders died with it, so the cards would fail on submit.
pub(super) fn cancel_orphaned_requests_on<S: BroadcastSink>(
    sink: &S,
    next_seqs: &SeqMap,
    session_id: &str,
) {
    let publish = |event: Event| {
        publish_next_persisted(sink, next_seqs, session_id, &event);
    };
    let approvals = sink.unresolved_approval_nonces(session_id);
    if !approvals.is_empty() {
        info!(
            target: "acp.supervisor",
            session = %session_id,
            stale = approvals.len(),
            "cancelling approvals orphaned by daemon restart"
        );
        for nonce in approvals {
            publish(Event::ApprovalResolved {
                nonce,
                decision: ApprovalDecision::Cancelled,
            });
        }
        publish(Event::Stopped {
            reason: "approval_cancelled_on_restart".to_string(),
        });
    }
    let elicitations = sink.unresolved_elicitation_nonces(session_id);
    if !elicitations.is_empty() {
        info!(
            target: "acp.supervisor",
            session = %session_id,
            stale = elicitations.len(),
            "cancelling elicitations orphaned by daemon restart"
        );
        for nonce in elicitations {
            publish(Event::ElicitationResolved {
                nonce,
                outcome: ElicitationOutcome::Cancelled,
                answers: Vec::new(),
            });
        }
    }
}

/// The session's registry record, if it names an agent and its runner is alive.
fn live_record_agent(session_id: &str) -> Option<crate::process::worker_registry::WorkerRecord> {
    crate::process::worker_registry::load(session_id)
        .ok()
        .flatten()
        .filter(|record| {
            !record.agent_key.is_empty() && crate::process::worker_registry::is_record_live(record)
        })
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::acp::approvals::Nonce;
    use crate::process::worker_registry;

    #[tokio::test]
    async fn seq_counters_are_per_session_hydratable_and_forgettable() {
        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        sup.hydrate_seqs([("s-hydrated".to_string(), 42)]);

        sup.publish_startup_error("s-1", "boom".into());
        sup.publish_user_prompt("s-1", "first prompt".into()).await;
        let resets_at = chrono::Utc::now();
        assert_eq!(
            sup.publish_rate_limit_auto_resumed("s-1", resets_at, false),
            3
        );
        assert_eq!(
            publish_with_next_seq(&sup.next_seqs, "s-2", |seq| seq),
            1,
            "sessions count separately"
        );
        sup.publish_user_prompt("s-hydrated", "after restart".into())
            .await;

        let frames = sink.frames.lock().unwrap().clone();
        let seqs: Vec<(&str, u64)> = frames
            .iter()
            .map(|(id, seq, _)| (id.as_str(), *seq))
            .collect();
        assert_eq!(
            seqs,
            [("s-1", 1), ("s-1", 2), ("s-1", 3), ("s-hydrated", 43)]
        );
        assert!(matches!(
            &frames[1].2,
            Event::UserPromptSent { text, .. } if text == "first prompt"
        ));
        assert!(matches!(
            &frames[2].2,
            Event::RateLimitAutoResumed { resets_at: ts, .. } if *ts == resets_at
        ));

        sup.forget_session("s-1").unwrap();
        assert_eq!(publish_with_next_seq(&sup.next_seqs, "s-1", |seq| seq), 1);
    }

    #[tokio::test]
    async fn publish_stopped_if_seq_refuses_when_the_counter_moved() {
        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        sup.hydrate_seqs([("s-1".to_string(), 7)]);

        assert!(!sup.publish_stopped_if_seq("s-1", "inferred_prompt_complete", 6));
        assert!(sink.frames.lock().unwrap().is_empty());

        assert!(sup.publish_stopped_if_seq("s-1", "inferred_prompt_complete", 7));
        let frames = sink.frames.lock().unwrap().clone();
        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames[0].1, 8,
            "must land immediately after the observed seq"
        );
        assert!(
            matches!(&frames[0].2, Event::Stopped { reason } if reason == "inferred_prompt_complete")
        );
        assert!(!sup.publish_stopped_if_seq("s-1", "inferred_prompt_complete", 7));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn clear_commands_route_by_the_sessions_agent_profile() {
        let _home = isolate_home();
        // (session, record agent name and key or None for a legacy record, text,
        //  disposition, SessionCleared published)
        let cases = [
            (
                "opencode",
                Some(("opencode", "opencode")),
                "/new",
                PromptDisposition::Forward,
                true,
            ),
            (
                "codex",
                Some(("codex-acp", "codex")),
                "/new",
                PromptDisposition::ResetContext,
                false,
            ),
            (
                "codex-clear",
                Some(("codex-acp", "codex")),
                "/clear",
                PromptDisposition::Forward,
                false,
            ),
            (
                "claude",
                Some(("claude-agent-acp", "claude")),
                "/clear",
                PromptDisposition::ResetContext,
                false,
            ),
            (
                "claude-plain",
                Some(("claude-agent-acp", "claude")),
                "clear the build cache",
                PromptDisposition::Forward,
                false,
            ),
            (
                "legacy",
                None,
                "/clear",
                PromptDisposition::ResetContext,
                false,
            ),
            (
                "no-record",
                Some(("", "")),
                "tell me about /clear",
                PromptDisposition::Forward,
                false,
            ),
        ];
        for (session_id, agent, text, disposition, cleared) in cases {
            let dir = worker_registry::workers_dir().unwrap();
            let socket = dir.join(format!("{session_id}.sock"));
            // A record counts only while its runner's control socket exists.
            let mut _live = None;
            match agent {
                Some(("", "")) => {}
                Some((name, key)) => {
                    _live = Some(
                        std::os::unix::net::UnixListener::bind(
                            crate::process::worker::control_socket_sibling(&socket),
                        )
                        .unwrap(),
                    );
                    let mut record = worker_record(session_id, std::process::id(), socket);
                    record.agent_name = name.into();
                    record.agent_key = key.into();
                    worker_registry::save(&record).unwrap();
                }
                None => {
                    let legacy = serde_json::json!({
                        "runner_version": worker_registry::RUNNER_VERSION,
                        "session_id": session_id,
                        "pid": std::process::id(),
                        "socket_path": socket,
                        "agent_name": "claude-agent-acp",
                        "cwd": std::env::temp_dir(),
                        "model": null,
                        "additional_dirs": [],
                        "provider_env_keys": [],
                        "stored_acp_session_id": null,
                        "started_at": 0,
                        "last_attached_at": null,
                        "detached_at": null
                    });
                    std::fs::write(
                        dir.join(format!("{session_id}.json")),
                        serde_json::to_string(&legacy).unwrap(),
                    )
                    .unwrap();
                }
            }

            let sink = VecSink::new();
            let sup = Supervisor::new(sink.clone());
            assert_eq!(
                sup.publish_user_prompt(session_id, text.into()).await,
                disposition,
                "{session_id}"
            );
            let frames = sink.frames.lock().unwrap().clone();
            assert!(
                matches!(&frames[0].2, Event::UserPromptSent { text: sent, .. } if sent == text),
                "{session_id}"
            );
            if cleared {
                assert_eq!(frames.len(), 2, "{session_id}");
                assert!(matches!(&frames[1].2, Event::SessionCleared));
                assert_eq!(frames[1].1, 2, "SessionCleared uses the next seq");
            } else {
                assert_eq!(frames.len(), 1, "{session_id}: {frames:?}");
            }
            worker_registry::delete(session_id).ok();
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_custom_agents_declared_alias_drives_a_reset_and_nothing_else_does() {
        let (_home, _tmp) = isolate_home();
        let config = crate::session::get_app_dir().unwrap().join("config.toml");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(
            &config,
            r#"
[session.agent_clear_alias]
bridge-clear = "/clear"
bridge-new = "/new"
claude = "/new"
inherits-claude = "/new"

[session.agent_acp_cmd]
bridge-clear = "bridge-clear acp"
bridge-new = "bridge-new acp"

[session.agent_detect_as]
inherits-claude = "claude"
"#,
        )
        .unwrap();
        // (agent key, text, disposition, SessionCleared published)
        let cases = [
            (
                "bridge-clear",
                "/clear",
                PromptDisposition::ResetContext,
                false,
            ),
            ("bridge-clear", "/new", PromptDisposition::Forward, false),
            (
                "bridge-clear",
                "clear the cache",
                PromptDisposition::Forward,
                false,
            ),
            ("bridge-new", "/new", PromptDisposition::ResetContext, false),
            ("bridge-new", "/clear", PromptDisposition::Forward, false),
            // No alias declared: the text is an ordinary prompt, as before this setting.
            ("bridge-none", "/clear", PromptDisposition::Forward, false),
            // An agent that inherits a built-in adapter has no acp command of its own, so a
            // declared alias is ignored for it rather than advertised and then forwarded as text.
            ("inherits-claude", "/new", PromptDisposition::Forward, false),
            // A built-in agent keeps its own alias whatever the map declares for it.
            ("claude", "/clear", PromptDisposition::ResetContext, false),
            ("claude", "/new", PromptDisposition::Forward, false),
        ];
        for (n, (key, text, disposition, cleared)) in cases.into_iter().enumerate() {
            let session_id = format!("alias-{n}");
            let _live = save_agent_record(&session_id, key, true);

            let sink = VecSink::new();
            let sup = Supervisor::new(sink.clone());
            assert_eq!(
                sup.publish_user_prompt(&session_id, text.into()).await,
                disposition,
                "{key} {text:?}"
            );
            let frames = sink.frames.lock().unwrap().clone();
            assert_eq!(frames.len(), if cleared { 2 } else { 1 }, "{key} {text:?}");
            worker_registry::delete(&session_id).ok();
        }
    }

    #[tokio::test]
    async fn orphaned_requests_are_cancelled_in_seq_order() {
        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        sup.cancel_orphaned_requests("s-attach");
        assert!(
            sink.frames.lock().unwrap().is_empty(),
            "nothing stale, nothing published"
        );

        *sink.stale_nonces.lock().unwrap() = vec![Nonce("nonce-a".into()), Nonce("nonce-b".into())];
        *sink.stale_elicitation_nonces.lock().unwrap() = vec![Nonce("e-a".into())];
        sup.cancel_orphaned_requests("s-attach");
        let frames = sink.frames.lock().unwrap().clone();
        let summary: Vec<String> = frames
            .iter()
            .map(|(_, _, event)| match event {
                Event::ApprovalResolved { nonce, decision } => {
                    assert_eq!(*decision, ApprovalDecision::Cancelled);
                    format!("approval:{}", nonce.0)
                }
                Event::Stopped { reason } => format!("stopped:{reason}"),
                Event::ElicitationResolved { nonce, outcome, .. } => {
                    assert!(matches!(outcome, ElicitationOutcome::Cancelled));
                    format!("elicitation:{}", nonce.0)
                }
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            summary,
            [
                "approval:nonce-a",
                "approval:nonce-b",
                "stopped:approval_cancelled_on_restart",
                "elicitation:e-a"
            ]
        );
        let seqs: Vec<u64> = frames.iter().map(|f| f.1).collect();
        assert_eq!(seqs, [1, 2, 3, 4]);
    }

    /// `synthesized` rides through to the published event: a rate-limit resume
    /// continuation must stay distinguishable from an ordinary prompt so the
    /// transcript can skip rendering a duplicate row for it (#4040).
    #[tokio::test]
    async fn publish_user_prompt_carries_the_synthesized_flag() {
        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        sup.publish_user_prompt("s-1", "typed by the user".into())
            .await;
        sup.publish_user_prompt_with_attachments(
            "s-1",
            "resent after rate limit".into(),
            &[],
            None,
            true,
            None,
            None,
        )
        .await;

        let flags: Vec<bool> = sink
            .frames
            .lock()
            .unwrap()
            .iter()
            .map(|(_, _, event)| match event {
                Event::UserPromptSent { synthesized, .. } => *synthesized,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(flags, [false, true]);
    }

    /// The published seq is the `UserPromptSent` event's, and there is none when the
    /// event did not reach the store; the text is still routed either way.
    #[tokio::test]
    async fn publish_user_prompt_reports_the_seq_it_recorded() {
        struct RefusesToPersist;
        impl BroadcastSink for RefusesToPersist {
            fn publish(&self, _: &str, _: u64, _: &Event) {}
            fn publish_persisted(&self, _: &str, _: u64, _: &Event) -> bool {
                false
            }
        }

        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        sup.publish_user_prompt("s-1", "first".into()).await;
        let published = sup
            .publish_user_prompt_with_attachments(
                "s-1",
                "second".into(),
                &[],
                None,
                false,
                None,
                None,
            )
            .await;
        let recorded = sink.frames.lock().unwrap().last().unwrap().1;
        assert_eq!(
            published,
            PublishedPrompt {
                disposition: PromptDisposition::Forward,
                seq: Some(recorded),
            }
        );

        let sup = Supervisor::new(std::sync::Arc::new(RefusesToPersist));
        let published = sup
            .publish_user_prompt_with_attachments(
                "s-1",
                "lost".into(),
                &[],
                None,
                false,
                None,
                None,
            )
            .await;
        assert_eq!(
            published,
            PublishedPrompt {
                disposition: PromptDisposition::Forward,
                seq: None,
            }
        );
    }

    /// Records, for each broadcast, whether its session's seq counter was held.
    #[derive(Default)]
    struct LockProbeSink {
        seqs: std::sync::OnceLock<std::sync::Arc<SeqMap>>,
        held: std::sync::Mutex<Vec<(u64, bool)>>,
    }

    impl BroadcastSink for LockProbeSink {
        fn publish(&self, session_id: &str, seq: u64, _event: &Event) {
            let counter = super::super::session_seq(self.seqs.get().unwrap(), session_id);
            let held = matches!(counter.try_lock(), Err(std::sync::TryLockError::WouldBlock));
            self.held.lock().unwrap().push((seq, held));
        }
    }

    /// Records the running epoch's admission count as each prompt event is stored.
    #[derive(Default)]
    struct AdmissionProbeSink {
        lease: std::sync::OnceLock<crate::acp::runner_lifecycle::Lease>,
        seen: std::sync::Mutex<Vec<u64>>,
    }

    impl BroadcastSink for AdmissionProbeSink {
        fn publish(&self, _: &str, _: u64, event: &Event) {
            if matches!(
                event,
                Event::UserPromptSent { .. } | Event::UserDiffCommentsPrompt { .. }
            ) {
                self.seen
                    .lock()
                    .unwrap()
                    .push(self.lease.get().unwrap().admissions());
            }
        }
    }

    /// An idle stop claimed between a prompt's persistence and its admission
    /// count would stop the worker the prompt was just recorded for (#5048), so
    /// the count must already stand when the event is stored.
    #[tokio::test]
    async fn prompts_are_admitted_before_they_are_stored() {
        let sink = std::sync::Arc::new(AdmissionProbeSink::default());
        let sup = Supervisor::new(std::sync::Arc::clone(&sink));
        sup.test_insert_worker("s-1").await;
        sink.lease.set(sup.running_lease("s-1").unwrap()).unwrap();

        sup.publish_user_prompt("s-1", "typed".into()).await;
        sup.publish_user_diff_comments_prompt(
            "s-1",
            String::new(),
            String::new(),
            false,
            Vec::new(),
            "review".into(),
        )
        .await;

        assert_eq!(*sink.seen.lock().unwrap(), [1, 2]);
    }

    #[tokio::test]
    async fn each_publish_runs_under_its_sessions_seq_lock() {
        let sink = std::sync::Arc::new(LockProbeSink::default());
        let sup = Supervisor::new(std::sync::Arc::clone(&sink));
        sink.seqs
            .set(std::sync::Arc::clone(&sup.next_seqs))
            .unwrap();

        sup.publish_startup_error("s-1", "boom".into());
        sup.publish_user_prompt("s-1", "prompt".into()).await;
        assert!(sup.publish_stopped_if_seq("s-1", "user_stopped", 2));

        assert_eq!(
            *sink.held.lock().unwrap(),
            [(1, true), (2, true), (3, true)]
        );
    }
}
