//! Supervisor-authored events, each allocated the session's next seq.

use tracing::info;

use super::{next_seq, BroadcastSink, PromptDisposition, SeqMap, Supervisor, WorkerKind};
use crate::acp::approvals::ApprovalDecision;
use crate::acp::elicitations::ElicitationOutcome;
use crate::acp::event_store::{AttachmentBlob, UnresolvedBackgroundAgentLaunch};
use crate::acp::state::{BackgroundAgentStatus, Event};

impl<S: BroadcastSink> Supervisor<S> {
    pub(super) fn publish_next(&self, session_id: &str, event: &Event) -> u64 {
        let seq = next_seq(&self.next_seqs, session_id);
        self.sink.publish(session_id, seq, event);
        seq
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
        let seq = {
            let mut guard = super::lock_recover(&self.next_seqs);
            let current = guard.get(session_id).copied().unwrap_or(0);
            if current != expected_seq {
                return false;
            }
            let seq = current.saturating_add(1);
            guard.insert(session_id.to_string(), seq);
            seq
        };
        self.sink.publish(
            session_id,
            seq,
            &Event::Stopped {
                reason: reason.to_string(),
            },
        );
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
        self.publish_user_prompt_with_attachments(session_id, text, &[], None, false)
            .await
    }

    /// Record a `UserPromptSent` (attachments stored as blobs keyed to its seq)
    /// and decide how the caller routes the text. `synthesized` marks a
    /// daemon-queued rate-limit continuation rather than text the user just
    /// typed, which the transcript renders no row for (#4040).
    pub async fn publish_user_prompt_with_attachments(
        &self,
        session_id: &str,
        text: String,
        attachments: &[AttachmentBlob],
        prompt_id: Option<String>,
        synthesized: bool,
    ) -> PromptDisposition {
        let agent_key = self.agent_key_for_session(session_id).await;
        let profile = crate::acp::agent_profiles::resolve(&agent_key);
        let is_clear = profile.is_clear_command(&text);
        let disposition = if is_clear && profile.clear_requires_driven_reset {
            PromptDisposition::ResetContext
        } else {
            PromptDisposition::Forward
        };
        let seq = next_seq(&self.next_seqs, session_id);
        let mut refs = Vec::with_capacity(attachments.len());
        for blob in attachments {
            if !self.sink.record_attachment(session_id, seq, blob) {
                self.sink.delete_attachments_for_seq(session_id, seq);
                return disposition;
            }
            refs.push(crate::daemon::PromptAttachmentRef {
                id: blob.id.clone(),
                kind: blob.kind,
                mime_type: blob.mime_type.clone(),
                name: blob.name.clone(),
                size: blob.data.len() as u64,
            });
        }
        let event = Event::UserPromptSent {
            text,
            attachments: refs,
            prompt_id,
            synthesized,
        };
        if !self.sink.publish_persisted(session_id, seq, &event) {
            // Roll back so refs never point at blobs that were not published.
            self.sink.delete_attachments_for_seq(session_id, seq);
            return disposition;
        }
        if is_clear && disposition == PromptDisposition::Forward {
            self.publish_next(session_id, &Event::SessionCleared);
        }
        disposition
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

    /// The session's agent registry key: from the live runner, else its
    /// registry record, else `claude` for records that predate the key.
    pub(super) async fn agent_key_for_session(&self, session_id: &str) -> String {
        if let Some(handle) = self.workers.lock().await.get(session_id) {
            if let WorkerKind::Runner { spawn_config } = &handle.kind {
                return spawn_config.agent_key.clone();
            }
        }
        match crate::process::worker_registry::load(session_id) {
            Ok(Some(record)) if !record.agent_key.is_empty() => record.agent_key,
            _ => "claude".to_string(),
        }
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
    sink.publish(
        session_id,
        next_seq(next_seqs, session_id),
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
    let publish = |event: Event| sink.publish(session_id, next_seq(next_seqs, session_id), &event);
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
            next_seq(&sup.next_seqs, "s-2"),
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

        sup.forget_session("s-1");
        assert_eq!(next_seq(&sup.next_seqs, "s-1"), 1);
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
            match agent {
                Some(("", "")) => {}
                Some((name, key)) => {
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
}
