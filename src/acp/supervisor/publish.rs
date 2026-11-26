//! Supervisor-authored events, each allocated the session's next seq.

use std::sync::Arc;
use tracing::info;

use super::{
    publish_next_persisted, with_publication_seq, BroadcastSink, PromptDisposition,
    PublishedPrompt, SeqMap, Supervisor, WorkerKind,
};
use crate::acp::approvals::ApprovalDecision;
use crate::acp::elicitations::ElicitationOutcome;
use crate::acp::event_sender::{AcpEventData, EventScope};
use crate::acp::event_store::{AttachmentBlob, UnresolvedBackgroundAgentLaunch};
use crate::acp::state::{BackgroundAgentStatus, Event, StartupCause, SubagentState};

/// A prompt to record and route through [`Supervisor::publish_user_prompt_with_attachments`].
#[derive(Default)]
pub struct UserPrompt<'a> {
    pub text: String,
    pub attachments: &'a [AttachmentBlob],
    /// Transcript row id first, then the ids of queued rows folded into the same turn.
    pub prompt_ids: Vec<String>,
    /// A daemon-queued rate-limit continuation rather than text the user just typed,
    /// which the transcript renders no row for (#4040).
    pub synthesized: bool,
    pub origin: crate::daemon::PromptOrigin,
    pub origin_plugin: Option<String>,
    /// The profile a caller already cut the turn with; `None` looks it up.
    pub classified_as: Option<&'static crate::acp::agent_profiles::AgentProfile>,
}

impl<S: BroadcastSink> Supervisor<S> {
    pub(super) fn publish_next(&self, session_id: &str, event: Event) -> u64 {
        publish_next_persisted(
            self.sink.as_ref(),
            &self.next_seqs,
            session_id,
            AcpEventData::from_host(event),
        )
        .0
    }

    /// Publish an `AgentStartupError` for a session whose worker never came online.
    pub fn publish_startup_error(&self, session_id: &str, message: String) {
        self.publish_startup_error_with_cause(session_id, message, None);
    }

    /// A retained runtime refused admission; this is not a native I/O/death
    /// observation. Keep both native and worker exclusions through publication.
    pub(crate) async fn publish_control_refusal(
        &self,
        session_id: &str,
        scope: &crate::acp::state::NativeObservationScope,
        refusal: crate::process::worker_registry::ControlAdmissionRefusal,
        originating_epoch: Option<u64>,
    ) -> bool {
        let workers = Arc::clone(&self.workers);
        let lifecycle = Arc::clone(&self.lifecycle);
        let next_seqs = Arc::clone(&self.next_seqs);
        let sink = Arc::clone(&self.sink);
        let session_id = session_id.to_owned();
        let scope = scope.clone();
        let (done, published) = tokio::sync::oneshot::channel();
        let publication = super::WorkerPublication::new(async move {
            let result = loop {
                let published = async {
                    let workers = workers.lock().await;
                    if workers.contains_key(&session_id) {
                        return Some(false);
                    }
                    let lifecycle = super::lock_recover(&lifecycle);
                    let current = lifecycle.current_lease(&session_id);
                    let epoch = match lifecycle.phase(&session_id) {
                        crate::acp::runner_lifecycle::WorkerPhase::Absent => None,
                        crate::acp::runner_lifecycle::WorkerPhase::Resuming => {
                            let Some(lease) = current.as_ref().filter(|lease| {
                                originating_epoch == Some(lease.epoch())
                                    && lease.spawned_runner() == refusal.owner
                                    && lease
                                        .native_scope()
                                        .ok()
                                        .flatten()
                                        .is_some_and(|native| native.context == scope.context)
                            }) else {
                                return Some(false);
                            };
                            Some(lease.epoch())
                        }
                        _ => return Some(false),
                    };
                    let _native = match scope.fence.try_lock_observation_matching(|_| true) {
                        Ok(Some(guard)) => guard,
                        Ok(None) => return None,
                        Err(_) => return Some(false),
                    };
                    if !crate::process::worker_registry::refusal_still_applies(
                        &session_id,
                        &refusal,
                    ) {
                        return Some(false);
                    }
                    let observed = refusal.observation();
                    let subject = crate::acp::state::NativeEventSubject {
                        context: scope.context,
                        owner: refusal.owner,
                        kind: crate::acp::state::NativeSubjectEvent::AdmissionRefusal(refusal),
                    };
                    let event = Event::AcpConnectionUnavailable {
                        source: observed.source,
                        cause: observed.cause,
                        phase: observed.phase,
                        native_subject: Some(subject),
                    };
                    let mut data = AcpEventData::from_history(event);
                    data.metadata.scope = EventScope::SessionCurrent;
                    data.metadata.availability_denial = true;
                    let seq = with_publication_seq(&next_seqs, &session_id, |seq| seq);
                    drop(lifecycle);
                    drop(workers);
                    let persisted = match epoch {
                        Some(epoch) => {
                            sink.publish_replay_from_worker(
                                &session_id,
                                seq,
                                &data,
                                epoch,
                                Some(subject),
                                None,
                            )
                            .1
                        }
                        None => sink.publish_persisted(&session_id, seq, &data),
                    };
                    Some(persisted)
                }
                .await;
                if let Some(published) = published {
                    break published;
                }
                let fence = Arc::clone(&scope.fence);
                if !matches!(
                    tokio::task::spawn_blocking(move || fence.lock_observation().map(drop)).await,
                    Ok(Ok(()))
                ) {
                    break false;
                }
            };
            let _ = done.send(result);
        });
        if let Some(publication) = self.sink.publish_from_worker(publication) {
            publication.complete().await;
        }
        published.await.unwrap_or(false)
    }

    pub(super) async fn publish_initial_stopped(
        &self,
        session_id: &str,
        original: std::sync::Weak<crate::acp::acp_client::AcpClient>,
        epoch: u64,
        intent: uuid::Uuid,
        prompt_seq: u64,
    ) -> Result<(), super::SupervisorError> {
        let workers = Arc::clone(&self.workers);
        let lifecycle = Arc::clone(&self.lifecycle);
        let next_seqs = Arc::clone(&self.next_seqs);
        let sink = Arc::clone(&self.sink);
        let session_id = session_id.to_owned();
        let (done, completed) = tokio::sync::oneshot::channel();
        let publication = super::WorkerPublication::new(async move {
            let result = loop {
                let mut wait = None;
                let result = async {
                    let workers = workers.lock().await;
                    let Some(handle) = workers.get(&session_id) else {
                        return Some(Err(super::SupervisorError::Superseded(session_id.clone())));
                    };
                    let Some(client) = original
                        .upgrade()
                        .filter(|client| Arc::ptr_eq(client, &handle.client))
                    else {
                        return Some(Err(super::SupervisorError::Superseded(session_id.clone())));
                    };
                    let lifecycle = super::lock_recover(&lifecycle);
                    if lifecycle.phase(&session_id)
                        != crate::acp::runner_lifecycle::WorkerPhase::Running
                        || lifecycle.current_lease(&session_id).as_ref() != Some(&handle.lease)
                        || handle.lease.epoch() != epoch
                        || handle.lease.cancellation().is_cancelled()
                    {
                        return Some(Err(super::SupervisorError::Superseded(session_id.clone())));
                    }
                    let scope = match handle.lease.native_scope() {
                        Ok(Some(scope)) => scope,
                        _ => return Some(Err(super::SupervisorError::NativeStoreUnavailable)),
                    };
                    let _native = match scope.fence.try_lock_observation_matching(|row| {
                        row.status != crate::session::Status::Stopped
                            && row.archived_at.is_none()
                            && row.trashed_at.is_none()
                            && row.terminal_handoff.is_none()
                            && scope
                                .choice
                                .as_ref()
                                .is_some_and(|choice| choice.matches(row))
                    }) {
                        Ok(Some(guard)) => guard,
                        Ok(None) => {
                            wait = Some(Arc::clone(&scope.fence));
                            return None;
                        }
                        Err(_) => return Some(Err(super::SupervisorError::NativeStoreUnavailable)),
                    };
                    if !client.take_cancelled_initial_prompt(intent, prompt_seq) {
                        return Some(Err(super::SupervisorError::Superseded(session_id.clone())));
                    }
                    let data = AcpEventData::from_host(Event::Stopped {
                        reason: "user_stopped".into(),
                    });
                    let seq = with_publication_seq(&next_seqs, &session_id, |seq| seq);
                    drop(lifecycle);
                    drop(workers);
                    Some(if sink.publish_persisted(&session_id, seq, &data) {
                        Ok(())
                    } else {
                        Err(super::SupervisorError::NativeStoreUnavailable)
                    })
                }
                .await;
                if let Some(result) = result {
                    break result;
                }
                let fence = wait.expect("unavailable Native fence supplied a wait");
                if !matches!(
                    tokio::task::spawn_blocking(move || fence.lock_observation().map(drop)).await,
                    Ok(Ok(()))
                ) {
                    break Err(super::SupervisorError::NativeStoreUnavailable);
                }
            };
            let _ = done.send(result);
        });
        if let Some(publication) = self.sink.publish_from_worker(publication) {
            publication.complete().await;
        }
        completed
            .await
            .unwrap_or(Err(super::SupervisorError::ControlStateUnavailable))
    }

    /// As [`Self::publish_startup_error`], naming the cause for clients that act on it.
    pub fn publish_startup_error_with_cause(
        &self,
        session_id: &str,
        message: String,
        cause: Option<StartupCause>,
    ) {
        self.publish_next(
            session_id,
            Event::AgentStartupError {
                message,
                cause,
                native_subject: None,
            },
        );
    }

    /// The startup error of a launch held for the sandbox login: the instruction, and the
    /// `command` and `profile` the recovery needs, as the 409 body carries them.
    pub fn publish_login_required(
        &self,
        session_id: &str,
        blocked: &crate::session::SandboxLoginRequired,
    ) {
        self.publish_startup_error_with_cause(
            session_id,
            blocked.to_string(),
            Some(blocked.startup_cause()),
        );
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
        self.sink
            .queue_persisted(
                session_id,
                Arc::clone(&self.next_seqs),
                AcpEventData::from_host(Event::Stopped {
                    reason: reason.to_owned(),
                }),
                Some(expected_seq),
            )
            .wait()
            .is_some_and(|(_, stored)| stored)
    }

    pub fn publish_agent_switched(
        &self,
        session_id: &str,
        from: String,
        to: String,
        reason: String,
    ) -> u64 {
        self.publish_next(session_id, Event::AgentSwitched { from, to, reason })
    }

    pub fn publish_conversation_summary(
        &self,
        session_id: &str,
        text: String,
        summarized_until_seq: u64,
    ) {
        self.publish_next(
            session_id,
            Event::ConversationSummary {
                text,
                summarized_until_seq,
            },
        );
    }

    /// `resets_at` is when the resume fired, not necessarily a reset the agent reported.
    pub fn publish_rate_limit_auto_resumed(
        &self,
        session_id: &str,
        resets_at: crate::time::Timestamp,
        manual: bool,
    ) -> Result<u64, super::SupervisorError> {
        let (seq, stored) = publish_next_persisted(
            self.sink.as_ref(),
            &self.next_seqs,
            session_id,
            AcpEventData::from_host(Event::RateLimitAutoResumed { resets_at, manual }),
        );
        stored
            .then_some(seq)
            .ok_or(super::SupervisorError::NativeStoreUnavailable)
    }

    /// Refuse a deferred release if its checked event frontier changed.
    pub(crate) fn publish_rate_limit_auto_resumed_if_seq(
        &self,
        session_id: &str,
        resets_at: crate::time::Timestamp,
        expected_seq: u64,
    ) -> Result<(), super::SupervisorError> {
        match self
            .sink
            .queue_persisted(
                session_id,
                Arc::clone(&self.next_seqs),
                AcpEventData::from_host(Event::RateLimitAutoResumed {
                    resets_at,
                    manual: true,
                }),
                Some(expected_seq),
            )
            .wait()
        {
            Some((_, true)) => Ok(()),
            Some((_, false)) => Err(super::SupervisorError::NativeStoreUnavailable),
            None => Err(super::SupervisorError::SpawnCancelled(
                session_id.to_owned(),
            )),
        }
    }

    /// Close a turn that was in flight when the previous daemon died.
    pub fn synthesize_stopped_for_orphan(&self, session_id: &str, reason: &str) {
        let seq = self.publish_next(
            session_id,
            Event::Stopped {
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
        self.publish_user_prompt_with_attachments(
            session_id,
            UserPrompt {
                text,
                ..Default::default()
            },
        )
        .await
        .disposition
    }

    /// Record a `UserPromptSent` (attachments stored as blobs keyed to its seq)
    /// and decide how the caller routes the text. See [`UserPrompt`] for the fields.
    pub async fn publish_user_prompt_with_attachments(
        &self,
        session_id: &str,
        prompt: UserPrompt<'_>,
    ) -> PublishedPrompt {
        let UserPrompt {
            text,
            attachments,
            prompt_ids,
            synthesized,
            origin,
            origin_plugin,
            classified_as,
        } = prompt;
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
            last_seq: None,
        };
        // The first id owns the row; queue drains preserve every folded submission id.
        let mut prompt_ids = prompt_ids.into_iter();
        let event = Event::UserPromptSent {
            text,
            attachments: attachments.iter().map(AttachmentBlob::to_ref).collect(),
            prompt_id: prompt_ids.next(),
            merged_prompt_ids: prompt_ids.collect(),
            synthesized,
            origin_plugin,
            origin,
        };
        let Some((seq, persisted)) = self
            .sink
            .queue_prompt(
                session_id,
                Arc::clone(&self.next_seqs),
                attachments,
                AcpEventData::from_host(event),
            )
            .wait()
        else {
            return unrecorded;
        };
        if !persisted {
            self.sink.delete_attachments_for_seq(session_id, seq);
            return unrecorded;
        }
        let last_seq = if is_clear && disposition == PromptDisposition::Forward {
            self.publish_next(session_id, Event::SessionCleared)
        } else {
            seq
        };
        #[cfg(test)]
        let gate = self.prompt_publish_gate.lock().unwrap().take();
        #[cfg(test)]
        if let Some(gate) = gate {
            gate.wait().await;
            gate.wait().await;
        }
        PublishedPrompt {
            disposition,
            seq: Some(seq),
            last_seq: Some(last_seq),
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
            Event::UserDiffCommentsPrompt {
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

    /// Bind identity validation to the client that will receive the classified turn.
    pub(crate) async fn classified_receiver(
        &self,
        session_id: &str,
    ) -> Option<(
        std::sync::Arc<crate::acp::acp_client::AcpClient>,
        String,
        u64,
    )> {
        let (client, epoch) = {
            let workers = self.workers.lock().await;
            let handle = workers.get(session_id)?;
            (std::sync::Arc::clone(&handle.client), handle.launch_epoch)
        };
        let agent = self.worker_agent_key(session_id).await?;
        self.classified_receiver_is_ready(session_id, epoch)
            .await
            .then_some((client, agent, epoch))
    }

    pub(crate) async fn classified_receiver_is_ready(&self, session_id: &str, epoch: u64) -> bool {
        let context_pending =
            super::lock_recover(&self.pending_context_resets).contains(session_id);
        !crate::process::worker_registry::stop_requested(session_id)
            && !context_pending
            && self.is_current_launch(session_id, epoch).await
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
    next_seqs: &Arc<SeqMap>,
    session_id: &str,
    warning: &str,
) {
    complete_stopped_worker_work(sink, next_seqs, session_id, Some(warning));
}

/// End tracking with the original allocation, without claiming a Native outcome.
pub(super) fn complete_stopped_worker_work<S: BroadcastSink>(
    sink: &S,
    next_seqs: &Arc<SeqMap>,
    session_id: &str,
    warning: Option<&str>,
) -> bool {
    let finish_task = |task_id, allocation| {
        publish_tracking_end(
            sink,
            next_seqs,
            session_id,
            Event::BackgroundTaskFinished {
                task_id,
                ended_at: Some(crate::time::Timestamp::now()),
            },
            allocation,
        )
    };
    let finish_agent = |agent_id, allocation| {
        publish_background_agent_detached(
            sink, next_seqs, session_id, agent_id, warning, allocation,
        )
    };
    if let Some(store) = sink.establishment_store() {
        let (Ok(tasks), Ok(agents)) = (
            store.unresolved_background_task_launches(session_id),
            store.unresolved_background_agent_tracking_launches(session_id),
        ) else {
            return false;
        };
        for task in tasks {
            if !finish_task(
                task.task_id,
                task.witness.as_ref().map(|witness| witness.allocation),
            ) {
                return false;
            }
        }
        for agent in agents {
            if !finish_agent(
                agent.agent_id,
                agent.witness.as_ref().map(|witness| witness.allocation),
            ) {
                return false;
            }
        }
    } else {
        for task_id in sink.unresolved_background_task_ids(session_id) {
            if !finish_task(task_id, None) {
                return false;
            }
        }
        for agent_id in sink.unresolved_background_agent_ids(session_id) {
            if !finish_agent(agent_id, None) {
                return false;
            }
        }
    }
    for event in cancelled_open_subagents(sink, session_id) {
        if !publish_next_persisted(sink, next_seqs, session_id, AcpEventData::from_host(event)).1 {
            return false;
        }
    }
    true
}

/// A dead worker's working subagents will never report their idle, so each
/// closes as cancelled.
pub(super) fn cancelled_open_subagents<S: BroadcastSink>(sink: &S, session_id: &str) -> Vec<Event> {
    sink.open_subagents(session_id)
        .into_iter()
        .map(|mut subagent| {
            subagent.state = SubagentState::Idle;
            subagent.stop_reason = Some("cancelled".into());
            Event::SubagentUpdated {
                subagent,
                at: crate::time::Timestamp::now(),
            }
        })
        .collect()
}

fn publish_background_agent_detached<S: BroadcastSink>(
    sink: &S,
    next_seqs: &Arc<SeqMap>,
    session_id: &str,
    agent_id: String,
    warning: Option<&str>,
    allocation: Option<uuid::Uuid>,
) -> bool {
    publish_tracking_end(
        sink,
        next_seqs,
        session_id,
        Event::BackgroundAgentCompleted {
            agent_id,
            status: BackgroundAgentStatus::Detached,
            tools: Vec::new(),
            result: None,
            warning: warning.map(str::to_owned),
            ended_at: crate::time::Timestamp::now(),
        },
        allocation,
    )
}

fn publish_tracking_end<S: BroadcastSink>(
    sink: &S,
    next_seqs: &Arc<SeqMap>,
    session_id: &str,
    event: Event,
    allocation: Option<uuid::Uuid>,
) -> bool {
    let mut data = AcpEventData::from_host(event);
    data.metadata.resource_ref = allocation;
    publish_next_persisted(sink, next_seqs, session_id, data).1
}

/// Missing paths are untrackable; usable paths still require their real witness at adoption.
pub(super) fn collect_resumable_background_agent_launches<S: BroadcastSink>(
    sink: &S,
    next_seqs: &Arc<SeqMap>,
    session_id: &str,
) -> anyhow::Result<Vec<UnresolvedBackgroundAgentLaunch>> {
    let (resumable, untrackable): (Vec<_>, Vec<_>) = sink
        .unresolved_background_agent_launches(session_id)?
        .into_iter()
        .partition(|l| !l.output_file.is_empty());
    for launch in untrackable {
        anyhow::ensure!(
            publish_background_agent_detached(
                sink,
                next_seqs,
                session_id,
                launch.agent_id,
                Some("session reattached before this sub-agent finished; tracking stopped"),
                launch.witness.as_ref().map(|witness| witness.allocation),
            ),
            "background agent detach was not persisted"
        );
    }
    Ok(resumable)
}

/// Cancel approvals and elicitations a dead worker left unresolved in the log:
/// their responders died with it, so the cards would fail on submit.
pub(super) fn cancel_orphaned_requests_on<S: BroadcastSink>(
    sink: &S,
    next_seqs: &Arc<SeqMap>,
    session_id: &str,
) {
    let publish = |event: Event| {
        publish_next_persisted(sink, next_seqs, session_id, AcpEventData::from_host(event));
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

/// The session's registry record while its runner is alive. A record that predates
/// `agent_key` ran `claude`.
fn live_record_agent(session_id: &str) -> Option<crate::process::worker_registry::WorkerRecord> {
    crate::process::worker_registry::load(session_id)
        .ok()
        .flatten()
        .filter(crate::process::worker_registry::is_record_live)
        .map(|mut record| {
            if record.agent_key.is_empty() {
                record.agent_key = "claude".into();
            }
            record
        })
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::acp::approvals::Nonce;
    use crate::process::worker_registry;

    #[test]
    fn login_required_startup_error_carries_the_recovery_fields() {
        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        let blocked = crate::session::SandboxLoginRequired {
            tool: "claude".to_string(),
            credential: std::path::PathBuf::from("/test/.credentials.json"),
            command: Some("my claude".to_string()),
            profile: Some("work".to_string()),
        };
        sup.publish_login_required("s-1", &blocked);
        sup.publish_login_required(
            "s-2",
            &crate::session::SandboxLoginRequired {
                command: None,
                profile: None,
                ..blocked.clone()
            },
        );

        let frames = sink.frames.lock().unwrap().clone();
        assert_eq!(frames.len(), 2);
        let Event::AgentStartupError { message, cause, .. } = &frames[0].2 else {
            panic!("expected a startup error, got {:?}", frames[0].2);
        };
        assert_eq!(*message, blocked.to_string());
        assert_eq!(
            *cause,
            Some(StartupCause::SandboxLoginRequired {
                command: Some("my claude".to_string()),
                profile: Some("work".to_string()),
            })
        );
        // Only what is set goes on the wire, as in the 409 body.
        let wire = serde_json::to_value(&frames[1].2).unwrap();
        assert_eq!(
            wire["AgentStartupError"]["cause"],
            serde_json::json!({ "kind": "sandbox_login_required" }),
            "{wire}"
        );
    }

    #[test]
    fn tracking_cleanup_refuses_unreadable_durable_allocations() {
        let (sink, _store, _events, database) = channel_sink();
        let supervisor = Supervisor::new(sink.clone());

        let fault = rusqlite::Connection::open(database.path().join("acp.db")).unwrap();
        fault.execute_batch("DROP TABLE acp_events").unwrap();
        assert!(!complete_stopped_worker_work(
            sink.as_ref(),
            &supervisor.next_seqs,
            "tracking",
            None
        ));
    }

    #[tokio::test]
    async fn durable_forget_clears_history_and_budget_before_restarting_the_sequence() {
        let (sink, store, mut published, _database) = channel_sink();
        let sup = Supervisor::new(sink.clone());
        sup.hydrate_seqs([("s-hydrated".to_string(), 42)]);

        sup.publish_startup_error("s-1", "boom".into());
        sup.publish_user_prompt("s-1", "first prompt".into()).await;
        let resets_at = crate::time::Timestamp::now();
        assert_eq!(
            sup.publish_rate_limit_auto_resumed("s-1", resets_at, false)
                .unwrap(),
            3
        );
        assert_eq!(
            sup.publish_next(
                "s-2",
                Event::Stopped {
                    reason: "user_stopped".into()
                }
            ),
            1,
            "sessions count separately"
        );
        sup.publish_user_prompt("s-hydrated", "after restart".into())
            .await;

        let frames: Vec<_> = (0..5).map(|_| published.try_recv().unwrap()).collect();
        let seqs: Vec<(&str, u64)> = frames
            .iter()
            .map(|frame| (frame.session_id.as_str(), frame.seq))
            .collect();
        assert_eq!(
            seqs,
            [
                ("s-1", 1),
                ("s-1", 2),
                ("s-1", 3),
                ("s-2", 1),
                ("s-hydrated", 43)
            ]
        );
        assert!(matches!(
            frames[1].event.as_ref(),
            Event::UserPromptSent { text, .. } if text == "first prompt"
        ));
        assert!(matches!(
            frames[2].event.as_ref(),
            Event::RateLimitAutoResumed { resets_at: ts, .. } if *ts == resets_at
        ));

        sup.forget_session("s-1").unwrap();
        sup.publish_user_prompt("s-1", "new explicit input".into())
            .await;
        let replay = store.replay_from("s-1", 0).unwrap();
        assert!(matches!(replay.as_slice(), [stored]
            if stored.seq == 1 && matches!(stored.event.as_ref(),
                Event::UserPromptSent { text, .. } if text == "new explicit input")));
        assert_eq!(
            store.rate_limit_redelivery_streak("s-1").unwrap(),
            crate::acp::event_store::ControlKnowledge::Known(0)
        );
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
            UserPrompt {
                text: "resent after rate limit".into(),
                synthesized: true,
                ..Default::default()
            },
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

    /// A queue drain folds several rows into one turn: the first id names the
    /// event and the rest ride in `merged_prompt_ids` so a client settles all of them (#5401).
    #[tokio::test]
    async fn publish_user_prompt_splits_leading_id_from_merged_ids() {
        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        for ids in [vec![], vec!["a"], vec!["a", "b", "c"]] {
            let ids: Vec<String> = ids.into_iter().map(String::from).collect();
            sup.publish_user_prompt_with_attachments(
                "s-1",
                UserPrompt {
                    text: "t".into(),
                    prompt_ids: ids,
                    ..Default::default()
                },
            )
            .await;
        }

        let got: Vec<(Option<String>, Vec<String>)> = sink
            .frames
            .lock()
            .unwrap()
            .iter()
            .map(|(_, _, event)| match event {
                Event::UserPromptSent {
                    prompt_id,
                    merged_prompt_ids,
                    ..
                } => (prompt_id.clone(), merged_prompt_ids.clone()),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            got,
            [
                (None, vec![]),
                (Some("a".into()), vec![]),
                (Some("a".into()), s(&["b", "c"])),
            ]
        );
    }

    /// The published seq is the `UserPromptSent` event's, and there is none when the
    /// event did not reach the store; the text is still routed either way.
    #[tokio::test]
    async fn publish_user_prompt_reports_the_seq_it_recorded() {
        struct RefusesToPersist;
        impl BroadcastSink for RefusesToPersist {
            fn publish(&self, _: &str, _: u64, _: &crate::acp::event_sender::AcpEventData) {}
            fn publish_persisted(
                &self,
                _: &str,
                _: u64,
                _: &crate::acp::event_sender::AcpEventData,
            ) -> bool {
                false
            }
        }

        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        sup.publish_user_prompt("s-1", "first".into()).await;
        let published = sup
            .publish_user_prompt_with_attachments(
                "s-1",
                UserPrompt {
                    text: "second".into(),
                    ..Default::default()
                },
            )
            .await;
        let recorded = sink.frames.lock().unwrap().last().unwrap().1;
        assert_eq!(
            published,
            PublishedPrompt {
                disposition: PromptDisposition::Forward,
                seq: Some(recorded),
                last_seq: Some(recorded),
            }
        );

        let sup = Supervisor::new(std::sync::Arc::new(RefusesToPersist));
        let published = sup
            .publish_user_prompt_with_attachments(
                "s-1",
                UserPrompt {
                    text: "lost".into(),
                    ..Default::default()
                },
            )
            .await;
        assert_eq!(
            published,
            PublishedPrompt {
                disposition: PromptDisposition::Forward,
                seq: None,
                last_seq: None,
            }
        );
    }

    /// Observes the reserved ordinal after sequence exclusion has been released for persistence.
    #[derive(Default)]
    struct LockProbeSink {
        seqs: std::sync::OnceLock<std::sync::Arc<SeqMap>>,
        seen: std::sync::Mutex<Vec<(u64, bool)>>,
        durable: Arc<VecSink>,
    }

    impl BroadcastSink for LockProbeSink {
        fn publish(&self, session_id: &str, seq: u64, _event: &AcpEventData) {
            let counter = super::super::sequence_slot(self.seqs.get().unwrap(), session_id);
            let Ok(current) = counter.try_lock() else {
                panic!("persistence must not retain sequence exclusion");
            };
            let allocated = current.seq == seq;
            drop(current);
            self.seen.lock().unwrap().push((seq, allocated));
        }
        fn publish_persisted(&self, session_id: &str, seq: u64, data: &AcpEventData) -> bool {
            self.publish(session_id, seq, data);
            self.durable.publish_persisted(session_id, seq, data)
        }
    }

    /// Records the running epoch's admission count as each prompt event is stored.
    #[derive(Default)]
    struct AdmissionProbeSink {
        lease: std::sync::OnceLock<crate::acp::runner_lifecycle::Lease>,
        seen: std::sync::Mutex<Vec<u64>>,
    }

    impl BroadcastSink for AdmissionProbeSink {
        fn publish(&self, _: &str, _: u64, data: &AcpEventData) {
            if matches!(
                data.event.as_ref(),
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
        assert!(sink.seqs.set(std::sync::Arc::clone(&sup.next_seqs)).is_ok());

        sup.publish_startup_error("s-1", "boom".into());
        sup.publish_user_prompt("s-1", "prompt".into()).await;
        assert!(sup.publish_stopped_if_seq("s-1", "user_stopped", 2));

        assert_eq!(
            *sink.seen.lock().unwrap(),
            [(1, true), (2, true), (3, true)]
        );
    }
}
