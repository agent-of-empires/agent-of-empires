//! The between-prompt command loop over an established session, including
//! the driven conversation reset.

use crate::acp::state::Event;
use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, McpServer, NewSessionRequest, NewSessionResponse,
    SessionConfigId, SessionConfigOption, SessionConfigValueId, SessionId,
    SetSessionConfigOptionRequest,
};
use agent_client_protocol::{Agent, ConnectionTo};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

use super::notifications::{now_ms, Shared};
use crate::acp::acp_client::between_prompt::{
    between_prompt_stop_reason, BETWEEN_PROMPT_IDLE_CHECK_INTERVAL,
};
use crate::acp::acp_client::commands::ClientCmd;
use crate::acp::acp_client::config_options::{
    config_option_failure_event, config_options_event, dispatch_set_config_option,
    dispatch_set_mode, mode_config_id, model_option, modes_available_event,
    thought_level_config_id, ConfigOptionDispatchPurpose, SessionChannels,
};
use crate::acp::acp_client::control::DaemonControlClient;
use crate::acp::acp_client::delete::handle_delete_session_cmd;
use crate::acp::acp_client::errors::acp_internal_error;
use crate::acp::acp_client::lifecycle::LifecycleEnvelope;
use crate::acp::acp_client::reset::{
    await_reset_request, ResetRequestError, ResetSessionOutcome, SESSION_RESET_IN_TASK_TIMEOUT,
};
use crate::acp::acp_client::session_identity::ordered_session_request;

pub(super) struct Session {
    pub(super) connection: ConnectionTo<Agent>,
    pub(super) shared: Arc<Shared>,
    pub(super) control: Option<Arc<DaemonControlClient>>,
    /// Mirrors the ingress's committed identity, refreshed each iteration so
    /// a reset's swap cannot be missed.
    pub(super) acp_session_id: SessionId,
    /// The id came from storage rather than this agent, so a prompt
    /// rejection may mean the agent dropped it (#3560).
    pub(super) session_from_storage: bool,
    pub(super) channels: SessionChannels,
    pub(super) steering_capable: bool,
    pub(super) source_profile: Option<String>,
    pub(super) default_effort: Option<String>,
    pub(super) default_mode: Option<String>,
    pub(super) default_model: Option<String>,
    pub(super) agent_cwd: PathBuf,
    /// Capability-filtered servers, forwarded again by a driven reset.
    pub(super) mcp_servers: Vec<McpServer>,
    pub(super) cmd_rx: mpsc::Receiver<ClientCmd>,
    pub(super) lifecycle_rx: mpsc::Receiver<LifecycleEnvelope>,
    /// Steers the adapter handed back unconsumed, run as ordinary turns.
    /// Kept here rather than re-sent on the bounded channel this task drains.
    pub(super) pending_prompts: VecDeque<Vec<ContentBlock>>,
}

impl Session {
    pub(super) async fn send_cancel(&self) -> Result<(), agent_client_protocol::Error> {
        match self.control.as_ref() {
            Some(control) => {
                control.cancel().await;
                Ok(())
            }
            None => self
                .connection
                .send_notification(CancelNotification::new(self.acp_session_id.clone())),
        }
    }

    pub(super) fn dispatch_config_option(&mut self, config_id: String, value: String) {
        // A reset re-applies `default_model`, so it follows the live pick.
        if self
            .channels
            .model_option
            .as_ref()
            .is_some_and(|option| option.id == config_id)
        {
            self.default_model = Some(value.clone());
        }
        dispatch_set_config_option(
            &self.connection,
            &self.acp_session_id,
            config_id,
            value,
            ConfigOptionDispatchPurpose::Generic,
            self.shared.event_tx.clone(),
        );
    }

    pub(super) fn dispatch_mode(&self, mode_id: String, while_prompting: bool) {
        dispatch_set_mode(
            &self.connection,
            &self.acp_session_id,
            mode_id,
            &self.channels,
            self.shared.event_tx.clone(),
            while_prompting,
        );
    }

    pub(super) async fn run(mut self) -> Result<(), agent_client_protocol::Error> {
        // Only polled while parked between prompts, so the per-prompt
        // watchdog stays the sole idle authority during a turn and this emit
        // is serialized with every command.
        let mut idle_tick = tokio::time::interval(BETWEEN_PROMPT_IDLE_CHECK_INTERVAL);
        idle_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            self.acp_session_id =
                self.shared.ingress.current().ok_or_else(|| {
                    acp_internal_error("native session is not established".into())
                })?;
            // Fallback prompts go first so later messages cannot overtake them.
            let cmd = match self.pending_prompts.pop_front() {
                Some(blocks) => Some(ClientCmd::Prompt(blocks)),
                None => tokio::select! {
                    cmd = self.cmd_rx.recv() => cmd,
                    _ = idle_tick.tick() => {
                        self.between_prompt_idle_tick().await;
                        continue;
                    }
                },
            };
            match cmd {
                Some(ClientCmd::Prompt(blocks)) => {
                    if self.run_prompt(blocks).await? {
                        break;
                    }
                }
                Some(ClientCmd::Cancel) => {
                    info!(target: "acp.protocol", "sending session/cancel (no prompt in flight)");
                    // Not `?`: a dead connection is when the UI most needs
                    // the synthetic Stopped below.
                    if let Err(e) = self.send_cancel().await {
                        warn!(
                            target: "acp.protocol",
                            error = %e,
                            "session/cancel (no prompt in flight) notification failed; still emitting Stopped"
                        );
                    }
                    // The UI thinks a turn is running but none is owned here;
                    // a spurious Stopped while idle is a reducer no-op (#2237).
                    self.stand_down_idle_completion();
                    self.shared
                        .emit(Event::Stopped {
                            reason: "cancelled".into(),
                        })
                        .await;
                }
                Some(ClientCmd::ForceStop) => {
                    // The supervisor publishes the terminal here (#1100).
                    info!(target: "acp.protocol", "force-stop requested with no prompt in flight; best-effort cancel only");
                    self.stand_down_idle_completion();
                    let _ = self.send_cancel().await;
                }
                Some(ClientCmd::SetMode(mode_id)) => self.dispatch_mode(mode_id, false),
                Some(ClientCmd::DeleteSession {
                    acp_session_id,
                    respond_to,
                }) => handle_delete_session_cmd(&self.connection, acp_session_id, respond_to),
                Some(ClientCmd::SetConfigOption { config_id, value }) => {
                    self.dispatch_config_option(config_id, value)
                }
                Some(ClientCmd::ResumeBackgroundTailing(launches)) => {
                    self.shared.resume_background_tailing(launches)
                }
                Some(ClientCmd::ResetSession {
                    text,
                    deadline,
                    respond_to,
                }) => self.reset_session(text, deadline, respond_to).await?,
                #[cfg(test)]
                Some(ClientCmd::FlushForTest(done)) => {
                    let _ = done.send(());
                }
                Some(ClientCmd::Shutdown) | None => {
                    info!(target: "acp.protocol", "shutdown received, exiting connection loop");
                    break;
                }
            }
        }
        Ok(())
    }

    /// This path owns the turn's terminal, so the other idle-completion
    /// paths must not add a duplicate (#2899).
    fn stand_down_idle_completion(&self) {
        self.shared.terminal_claim.claim();
        self.shared
            .adopted_turn_active
            .store(false, Ordering::Relaxed);
        self.shared.between_prompt.deactivate();
    }

    async fn between_prompt_idle_tick(&self) {
        let shared = &self.shared;
        let Some(cost_seen) = shared.between_prompt.take_idle_fire(now_ms()) else {
            return;
        };
        // An adopted turn races the detached resume-idle task for its
        // terminal; state is reset either way.
        let adopted = shared.adopted_turn_active.swap(false, Ordering::Relaxed);
        if adopted && !shared.terminal_claim.claim() {
            return;
        }
        let reason = between_prompt_stop_reason(adopted, cost_seen);
        info!(
            target: "acp.protocol",
            session = %shared.session_label,
            reason,
            "between-prompt idle watchdog: synthesizing Stopped for completed turn"
        );
        shared
            .emit(Event::Stopped {
                reason: reason.into(),
            })
            .await;
    }

    /// Open a fresh session on the live worker for a clear command whose
    /// adapter cannot hand back a durable post-reset id (#2979). The caller's
    /// deadline bounds every request, including queueing time.
    async fn reset_session(
        &mut self,
        text: String,
        deadline: tokio::time::Instant,
        respond_to: oneshot::Sender<ResetSessionOutcome>,
    ) -> Result<(), agent_client_protocol::Error> {
        let label = self.shared.session_label.clone();
        let ingress = self.shared.ingress.clone();
        // No identity transition while an agent callback holds the fence.
        let Ok(transition_guard) = tokio::time::timeout_at(deadline, ingress.fence.lock()).await
        else {
            let _ = respond_to.send(ResetSessionOutcome::Failed {
                message: "reset deadline expired while agent callbacks were in flight".into(),
            });
            return Ok(());
        };
        let work = self.shared.between_prompt.work_state();
        if work.is_busy() {
            // Old-session events would be attributed to the new conversation.
            warn!(
                target: "acp.protocol",
                tool_calls_in_flight = work.tool_calls,
                background_agents_in_flight = work.background_agents,
                "conversation reset requested while between-prompt work is in flight; refusing"
            );
            self.shared
                .emit(Event::PromptRejected {
                    reason: "agent_busy".into(),
                    text,
                })
                .await;
            let _ = respond_to.send(ResetSessionOutcome::Failed {
                message: "agent work is still in flight; wait for it to finish before clearing the conversation".into(),
            });
            return Ok(());
        }
        info!(
            target: "acp.protocol",
            session = %label,
            old_id = %self.acp_session_id.0,
            "conversation reset: issuing fresh session/new on the live worker"
        );
        let req =
            NewSessionRequest::new(self.agent_cwd.clone()).mcp_servers(self.mcp_servers.clone());
        let connection = self.connection.clone();
        let generation = ingress.begin();
        drop(transition_guard);
        let result = await_reset_request(deadline, || {
            ordered_session_request(
                &connection,
                &ingress,
                generation,
                req,
                |resp: &NewSessionResponse| resp.session_id.clone(),
            )
        })
        .await;
        // A failed reset keeps the old session, so its buffered updates are
        // replayed onto it rather than dropped.
        let commit_guard = ingress.fence.lock().await;
        let retained_id = result
            .as_ref()
            .map(|resp| resp.session_id.clone())
            .unwrap_or_else(|_| self.acp_session_id.clone());
        let mut replay = ingress.finish(Some(retained_id.clone()))?;
        if retained_id == self.acp_session_id {
            self.shared.apply_replay(std::mem::take(&mut replay)).await;
        }
        let message = match result {
            Ok(new_session) if new_session.session_id.0 != self.acp_session_id.0 => {
                let new_id = new_session.session_id.clone();
                // session/new is the irreversible commit; the loop picks the
                // id back up from the ingress on its next iteration.
                self.session_from_storage = false;
                self.channels = SessionChannels::new(
                    new_session.modes.as_ref(),
                    new_session.config_options.as_deref(),
                );
                info!(
                    target: "acp.protocol",
                    session = %label,
                    new_id = %new_id.0,
                    "conversation reset: session/new succeeded, swapped acp_session_id"
                );
                self.shared.emit(Event::SessionCleared).await;
                self.shared
                    .emit(Event::SessionContextReset {
                        reason: "conversation cleared; the agent started a fresh session".into(),
                    })
                    .await;
                self.shared
                    .emit(Event::AcpSessionAssigned {
                        acp_session_id: new_id.0.to_string(),
                    })
                    .await;
                self.shared.apply_replay(replay).await;
                drop(commit_guard);
                if let Some(modes) = &new_session.modes {
                    self.shared.emit(modes_available_event(modes)).await;
                }
                if let Some(event) = config_options_event(new_session.config_options.clone()) {
                    self.shared.emit(event).await;
                }
                self.reapply_defaults(&new_id, new_session.config_options.as_deref(), deadline)
                    .await;
                self.shared
                    .emit(Event::Stopped {
                        reason: "session_reset".into(),
                    })
                    .await;
                let _ = respond_to.send(ResetSessionOutcome::Reset {
                    new_acp_session_id: new_id.0.to_string(),
                });
                return Ok(());
            }
            // A stale runner answered from its handshake cache.
            Ok(_) => "the worker replayed the existing session \
                 instead of creating a fresh one; restart the \
                 structured view worker to clear context"
                .to_string(),
            Err(ResetRequestError::Acp(e)) => format!("session/new failed: {e}"),
            Err(ResetRequestError::TimedOut) => {
                "agent did not answer session/new before the reset deadline".to_string()
            }
        };
        warn!(target: "acp.protocol", session = %label, "conversation reset failed: {message}");
        self.shared
            .emit(Event::PromptRuntimeError {
                message: message.clone(),
            })
            .await;
        self.shared
            .emit(Event::Stopped {
                reason: "session_reset_failed".into(),
            })
            .await;
        let _ = respond_to.send(ResetSessionOutcome::Failed { message });
        Ok(())
    }

    /// The fresh session starts on adapter defaults, so the model, effort and
    /// mode are re-sent, best-effort, within the reset deadline. The model
    /// goes first because a switch rebuilds the option set the effort is
    /// resolved from.
    async fn reapply_defaults(
        &self,
        new_id: &SessionId,
        options: Option<&[SessionConfigOption]>,
        deadline: tokio::time::Instant,
    ) {
        let mut effort_id = options.and_then(thought_level_config_id);
        if let (Some(model), Some(option)) = (
            self.default_model.as_deref(),
            options.and_then(model_option),
        ) {
            if !option.is_current(model) {
                let config_id = SessionConfigId::new(option.id.clone());
                match self
                    .reset_set_option(new_id, config_id, model, deadline)
                    .await
                {
                    Some(Ok(options)) => effort_id = thought_level_config_id(&options),
                    Some(Err(reason)) => {
                        let event = config_option_failure_event(
                            option.id,
                            model.to_string(),
                            reason,
                            ConfigOptionDispatchPurpose::Generic,
                        );
                        self.shared.emit(event).await;
                    }
                    None => return,
                }
            }
        }
        for (value, config_id) in [
            (self.default_effort.as_deref(), effort_id),
            (
                self.default_mode.as_deref(),
                options.and_then(mode_config_id),
            ),
        ] {
            let (Some(value), Some(config_id)) = (value, config_id) else {
                debug!(
                    "post-reset config option skipped: no configured value or matching option id"
                );
                continue;
            };
            if self
                .reset_set_option(new_id, config_id, value, deadline)
                .await
                .is_none()
            {
                return;
            }
        }
    }

    /// The agent's option list or rejection reason; `None` once the reset
    /// deadline passes, which skips the remaining defaults.
    async fn reset_set_option(
        &self,
        new_id: &SessionId,
        config_id: SessionConfigId,
        value: &str,
        deadline: tokio::time::Instant,
    ) -> Option<Result<Vec<SessionConfigOption>, String>> {
        let label = &self.shared.session_label;
        let request = || {
            self.connection
                .send_request(SetSessionConfigOptionRequest::new(
                    new_id.clone(),
                    config_id,
                    SessionConfigValueId::new(value.to_string()),
                ))
                .block_task()
        };
        match await_reset_request(deadline, request).await {
            Ok(resp) => {
                if let Some(event) = config_options_event(Some(resp.config_options.clone())) {
                    self.shared.emit(event).await;
                }
                Some(Ok(resp.config_options))
            }
            Err(ResetRequestError::Acp(e)) => {
                warn!(
                    target: "acp.protocol",
                    session = %label,
                    value,
                    "re-applying structured view default after reset failed: {e}"
                );
                Some(Err(e.to_string()))
            }
            Err(ResetRequestError::TimedOut) => {
                warn!(
                    target: "acp.protocol",
                    session = %label,
                    value,
                    timeout_secs = SESSION_RESET_IN_TASK_TIMEOUT.as_secs(),
                    "post-reset config re-application timed out; skipping remaining defaults"
                );
                None
            }
        }
    }
}
