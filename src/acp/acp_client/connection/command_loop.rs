//! The between-prompt command loop over an established session, including
//! the driven conversation reset.

use crate::acp::state::Event;
use agent_client_protocol::v1::schema::{
    CancelNotification, ContentBlock, McpServer, NewSessionRequest, NewSessionResponse,
    SessionConfigId, SessionConfigOption, SessionConfigValueId, SessionId,
    SetSessionConfigOptionRequest,
};
use agent_client_protocol::v1::ConnectionTo;
use agent_client_protocol::Agent;
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
    dispatch_set_mode, mode_config_id, model_config_values, model_option, modes_available_event,
    thought_level_config_id, ConfigOptionDispatchPurpose, SessionChannels,
};
use crate::acp::acp_client::control::DaemonControlClient;
use crate::acp::acp_client::delete::handle_delete_session_cmd;
use crate::acp::acp_client::errors::{acp_internal_error, public_error_message};
use crate::acp::acp_client::lifecycle::LifecycleEnvelope;
use crate::acp::acp_client::rate_limit::take_rate_limit_from_captured_rejection;
use crate::acp::acp_client::reset::{
    await_reset_request, ResetRequestError, ResetSessionOutcome, SESSION_RESET_IN_TASK_TIMEOUT,
};
use crate::acp::acp_client::session_identity::{
    ordered_native_request, ordered_session_request, OrderedRequestResult,
};

pub(super) struct Session {
    pub(super) connection: ConnectionTo<Agent>,
    pub(super) shared: Arc<Shared>,
    pub(super) control: Option<Arc<DaemonControlClient>>,
    /// Mirrors the ingress's committed identity, refreshed each iteration so
    /// a reset's swap cannot be missed.
    pub(super) acp_session_id: SessionId,
    pub(super) channels: SessionChannels,
    pub(super) steering_capable: bool,
    pub(super) close_capable: bool,
    pub(super) source_profile: Option<String>,
    pub(super) default_effort: Option<String>,
    pub(super) default_model_config: std::collections::BTreeMap<String, String>,
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
                .shared
                .connection_state
                .admit(|| {
                    self.connection
                        .send_notification(CancelNotification::new(self.acp_session_id.clone()))
                })
                .map_err(|_| acp_internal_error("ACP connection unavailable".into()))?,
        }
    }

    /// A subagent session only honours `session/cancel` when it announced the
    /// capability; the root turn is unaffected either way.
    pub(super) async fn cancel_child(&self, session_id: String) -> bool {
        let child = SessionId::from(session_id.clone());
        if !self
            .shared
            .ingress
            .child_route(&child)
            .is_some_and(|route| route.cancel_capable)
        {
            return false;
        }
        match self.control.as_ref() {
            Some(control) => control.cancel_session(session_id).await,
            None => self
                .shared
                .connection_state
                .admit(|| {
                    self.connection
                        .send_notification(CancelNotification::new(child))
                })
                .is_ok_and(|result| result.is_ok()),
        }
    }

    pub(super) fn dispatch_config_option(
        &mut self,
        config_id: String,
        value: String,
        remember: bool,
    ) {
        // A reset re-applies `default_model`, so it follows the live pick.
        if self
            .channels
            .model_option
            .as_ref()
            .is_some_and(|option| option.id == config_id)
        {
            self.default_model = Some(value.clone());
        }
        if remember {
            self.default_model_config
                .insert(config_id.clone(), value.clone());
        }
        dispatch_set_config_option(
            &self.connection,
            &self.shared.connection_state,
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
            &self.shared.connection_state,
            &self.acp_session_id,
            mode_id,
            &self.channels,
            self.shared.event_tx.clone(),
            while_prompting,
        );
    }

    pub(super) fn refuse_unavailable_command(&self, command: &mut Option<ClientCmd>) -> bool {
        if self.shared.connection_state.loss().is_none()
            && !self.shared.connection_state.is_stopping()
        {
            return false;
        }
        let Some(command_value) = command.take() else {
            return false;
        };
        match command_value {
            ClientCmd::ResetSession { respond_to, .. } => {
                let _ = respond_to.send(ResetSessionOutcome::Failed {
                    message: "ACP connection unavailable".into(),
                });
            }
            ClientCmd::CloseSession(respond_to) => {
                let _ =
                    respond_to.send(crate::acp::acp_client::close::CloseSessionOutcome::Failed(
                        "ACP connection unavailable".into(),
                    ));
            }
            ClientCmd::DeleteSession { respond_to, .. } => {
                let _ = respond_to.send(
                    crate::acp::acp_client::delete::DeleteSessionOutcome::Failed(
                        "ACP connection unavailable".into(),
                    ),
                );
            }
            ClientCmd::CancelChild { respond_to, .. } => {
                let _ = respond_to.send(false);
            }
            ClientCmd::Prompt(_)
            | ClientCmd::Cancel
            | ClientCmd::SetMode(_)
            | ClientCmd::SetConfigOption { .. } => {}
            independent_or_stop => {
                *command = Some(independent_or_stop);
                return false;
            }
        }
        true
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
            let mut cmd = match self
                .shared
                .connection_state
                .loss()
                .is_none()
                .then(|| self.pending_prompts.pop_front())
                .flatten()
            {
                Some(blocks) => Some(ClientCmd::Prompt(blocks)),
                None => tokio::select! {
                    _ = self.shared.connection_state.wait_ordered_unavailable() => {
                        // EOF must wake an idle established session too. The
                        // ordered boundary preserves earlier replay/outcomes;
                        // outer cleanup owns the one qualified loss event.
                        return Err(acp_internal_error("agent connection unavailable".into()));
                    }
                    cmd = self.cmd_rx.recv() => cmd,
                    _ = idle_tick.tick() => {
                        self.between_prompt_idle_tick().await;
                        continue;
                    }
                },
            };
            if self.refuse_unavailable_command(&mut cmd) {
                continue;
            }
            match cmd {
                Some(ClientCmd::Prompt(blocks)) => {
                    if self.run_prompt(blocks).await? {
                        break;
                    }
                }
                Some(ClientCmd::CancelChild {
                    session_id,
                    respond_to,
                }) => {
                    let _ = respond_to.send(self.cancel_child(session_id).await);
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
                    if self.stand_down_idle_completion() {
                        self.shared
                            .emit_while_available([Event::Stopped {
                                reason: "cancelled".into(),
                            }])
                            .await;
                    }
                }
                Some(ClientCmd::ForceStop) => {
                    // The supervisor publishes the terminal here (#1100).
                    info!(target: "acp.protocol", "force-stop requested with no prompt in flight; best-effort cancel only");
                    self.stand_down_idle_completion();
                    if let Err(error) = self.send_cancel().await {
                        debug!(target: "acp.protocol", %error, "cancel notification failed");
                    }
                }
                Some(ClientCmd::SetMode(mode_id)) => self.dispatch_mode(mode_id, false),
                Some(ClientCmd::DeleteSession {
                    acp_session_id,
                    respond_to,
                }) => handle_delete_session_cmd(
                    &self.connection,
                    &self.shared.connection_state,
                    acp_session_id,
                    respond_to,
                ),
                Some(ClientCmd::SetConfigOption {
                    config_id,
                    value,
                    remember,
                }) => self.dispatch_config_option(config_id, value, remember),
                Some(ClientCmd::ResumeBackgroundTailing(launches)) => {
                    self.shared.resume_background_tailing(launches)
                }
                Some(ClientCmd::ResumeTaskTailing(task_ids)) => {
                    self.shared
                        .resume_task_tailing(&self.acp_session_id.0, task_ids)
                        .await
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
                Some(ClientCmd::CloseSession(respond_to)) => {
                    crate::acp::acp_client::close::dispatch_close(
                        &self.connection,
                        &self.shared.connection_state,
                        self.acp_session_id.clone(),
                        self.close_capable,
                        respond_to,
                    );
                }
                Some(ClientCmd::Shutdown) | None => {
                    info!(target: "acp.protocol", "shutdown received, exiting connection loop");
                    break;
                }
            }
        }
        Ok(())
    }

    /// Claim an idle terminal without overriding an already-completed turn.
    fn stand_down_idle_completion(&self) -> bool {
        self.shared
            .connection_state
            .admit(|| {
                let claimed = self.shared.terminal_claim.claim();
                self.shared
                    .adopted_turn_active
                    .store(false, Ordering::Relaxed);
                self.shared.between_prompt.deactivate();
                claimed
            })
            .unwrap_or(false)
    }

    
    async fn between_prompt_idle_tick(&self) {
        let shared = &self.shared;
        if shared.connection_state.loss().is_some() {
            return;
        }
        let Some(cost_seen) = shared
            .between_prompt
            .take_idle_fire(now_ms(), shared.aoe.leases.working(now_ms()))
        else {
            return;
        };
        let adopted = shared.adopted_turn_active.swap(false, Ordering::Relaxed);
        let captured = take_rate_limit_from_captured_rejection(
            &shared.rate_limit_rejections,
            chrono::Utc::now(),
        );
        if let Some(info) = captured {
            if !shared.terminal_claim.claim_failed() {
                return;
            }
            shared.between_prompt.deactivate();
            shared
                .emit_while_available([
                    Event::RateLimit { info },
                    Event::Stopped {
                        reason: "rate_limited".into(),
                    },
                ])
                .await;
            return;
        }
        if !shared.terminal_claim.claim() {
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
            .emit_while_available([Event::Stopped {
                reason: reason.into(),
            }])
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
        let Ok(transition_guard) = tokio::time::timeout_at(deadline, ingress.fence.write()).await
        else {
            let _ = respond_to.send(ResetSessionOutcome::Failed {
                message: "reset deadline expired while agent callbacks were in flight".into(),
            });
            return Ok(());
        };
        if ingress.has_parked() {
            let _ = respond_to.send(ResetSessionOutcome::Failed {
                message: "an approval is waiting for an answer; resolve it before clearing the conversation".into(),
            });
            return Ok(());
        }
        if self.shared.has_active_subagent() {
            self.shared
                .emit(Event::PromptRejected {
                    reason: "agent_busy".into(),
                    text,
                })
                .await;
            let _ = respond_to.send(ResetSessionOutcome::Failed {
                message: "a subagent is still running; wait for it to finish before clearing the conversation".into(),
            });
            return Ok(());
        }
        let work = self.shared.between_prompt.work_state();
        if work.is_busy()
            || !self.shared.bg_tasks.is_empty()
            || self.shared.aoe.leases.any_live(now_ms())
        {
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
                &self.shared.connection_state,
                generation,
                req,
                |resp: &NewSessionResponse| resp.session_id.clone(),
            )
        })
        .await;
        // A failed reset keeps the old session, so its buffered updates are
        // replayed onto it rather than dropped.
        let commit_guard = ingress.fence.write().await;
        let retained_id = result
            .as_ref()
            .map(|resp| resp.session_id.clone())
            .unwrap_or_else(|_| self.acp_session_id.clone());
        let mut replay = ingress.finish(Some(retained_id.clone()))?;
        if retained_id == self.acp_session_id {
            self.shared.apply_replay(std::mem::take(&mut replay)).await;
        }
        if result.is_err() && self.shared.connection_state.loss().is_some() {
            drop(commit_guard);
            let _ = respond_to.send(ResetSessionOutcome::Failed {
                message: "ACP connection unavailable".into(),
            });
            return Ok(());
        }
        let message = match result {
            Ok(new_session) if new_session.session_id.0 != self.acp_session_id.0 => {
                let new_id = new_session.session_id.clone();
                // session/new is the irreversible commit; the loop picks the
                // id back up from the ingress on its next iteration.
                self.shared.aoe.session_reset();
                self.shared.settle_discarded().await;
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
                self.shared.terminal_claim.clear_failed_outcome();
                self.shared.emit(Event::SessionCleared).await;
                self.shared
                    .emit(Event::SessionContextReset {
                        reason: "conversation cleared; the agent started a fresh session".into(),
                        fork_rejection: None,
                    })
                    .await;
                self.shared
                    .emit(Event::AcpSessionAssigned {
                        acp_session_id: new_id.0.to_string(),
                        connection_ready: true,
                        native_subject: None,
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
            Err(ResetRequestError::Acp(e)) => {
                warn!(target: "acp.protocol", error = ?e, "session/new request failed");
                format!(
                    "session/new failed: {}",
                    super::super::errors::public_error_message(&e)
                )
            }
            Err(ResetRequestError::TimedOut) => {
                "agent did not answer session/new before the reset deadline".to_string()
            }
        };
        warn!(target: "acp.protocol", session = %label, "conversation reset failed: {message}");
        self.shared
            .emit(Event::PromptRuntimeError {
                message: message.clone(),
                evidence: Some(crate::session::OperationErrorEvidence::new(
                    crate::session::ErrorEvidenceSource::LocalPromptResult,
                    crate::session::ErrorOperation::ContextReset,
                )),
                native_subject: None,
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
    /// mode and model_config picks are re-sent, best-effort, within the reset
    /// deadline. The model goes first because a switch rebuilds the option
    /// set the effort and the toggles are resolved from.
    async fn reapply_defaults(
        &self,
        new_id: &SessionId,
        options: Option<&[SessionConfigOption]>,
        deadline: tokio::time::Instant,
    ) {
        let mut effort_id = options.and_then(thought_level_config_id);
        let mut toggles = options.map(model_config_values).unwrap_or_default();
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
                    Some(Ok(options)) => {
                        effort_id = thought_level_config_id(&options);
                        toggles = model_config_values(&options);
                    }
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
        for (id, value) in &self.default_model_config {
            if toggles.get(id).is_none_or(|current| current == value) {
                continue;
            }
            match self
                .reset_set_option(new_id, SessionConfigId::new(id.clone()), value, deadline)
                .await
            {
                Some(Ok(options)) => toggles = model_config_values(&options),
                Some(Err(reason)) => {
                    debug!(target: "acp.protocol", "post-reset {id} rejected: {reason}")
                }
                None => return,
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
        let request = || async {
            let request = ordered_native_request(
                &self.connection,
                &self.shared.connection_state,
                SetSessionConfigOptionRequest::new(
                    new_id.clone(),
                    config_id,
                    SessionConfigValueId::new(value.to_string()),
                ),
            )
            .map_err(|_| acp_internal_error("ACP connection unavailable".into()))?;
            match request.wait().await {
                OrderedRequestResult::Correlated(result) => result,
                OrderedRequestResult::Refused(error) => Err(error),
                OrderedRequestResult::Unavailable(_) => {
                    Err(acp_internal_error("ACP connection unavailable".into()))
                }
                OrderedRequestResult::Abandoned => Err(acp_internal_error(
                    "Config response was not observed".into(),
                )),
            }
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
                    error = ?e, "re-applying structured view default after reset failed"
                );
                Some(Err(public_error_message(&e)))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::acp_client::lifecycle::TerminalClaim;
    use crate::acp::acp_client::session_identity::{RequestPolicy, SessionIngress};
    use agent_client_protocol::{ByteStreams, Client};
    use futures_util::FutureExt as _;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::sync::Mutex;
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    #[tokio::test]
    async fn reset_write_fence_releases_or_expires_with_an_actual_read_admission() {
        for expire in [false, true] {
            let (daemon_write, agent_read) = tokio::io::duplex(64 * 1024);
            let (mut agent_write, daemon_read) = tokio::io::duplex(64 * 1024);
            let received = Arc::new(Mutex::new(Vec::new()));
            let received_frames = received.clone();
            let agent = tokio::spawn(async move {
                let mut lines = BufReader::new(agent_read).lines();
                let mut new_sessions = 0;
                while let Some(line) = lines.next_line().await.unwrap() {
                    let message: serde_json::Value = serde_json::from_str(&line).unwrap();
                    received_frames.lock().await.push(message.clone());
                    assert_eq!(message["method"], "session/new");
                    let session_id = if new_sessions == 0 {
                        "s-fence-reset".to_string()
                    } else {
                        format!("s-fence-reset-reset-{new_sessions}")
                    };
                    new_sessions += 1;
                    let response = serde_json::json!({
                        "jsonrpc":"2.0","id":message["id"],"result":{"sessionId":session_id}
                    })
                    .to_string();
                    agent_write.write_all(response.as_bytes()).await.unwrap();
                    agent_write.write_all(b"\n").await.unwrap();
                    agent_write.flush().await.unwrap();
                }
            });
            let transport = ByteStreams::new(daemon_write.compat_write(), daemon_read.compat());
            tokio::time::timeout(Duration::from_secs(10), Client.v1().name("reset-fence-test").connect_with(
                transport,
                move |connection: ConnectionTo<Agent>| async move {
                    let cwd = tempfile::tempdir().unwrap();
                    let (event_tx, mut event_rx) = mpsc::channel(64);
                    let (lifecycle_tx, lifecycle_rx) = mpsc::channel(128);
                    let (_cmd_tx, cmd_rx) = mpsc::channel(1);
                    let ingress = Arc::new(SessionIngress::default());
                    let shared = Arc::new(Shared::new(
                        event_tx,
                        "reset-fence-test".into(),
                        &crate::acp::agent_profiles::GEMINI,
                        lifecycle_tx,
                        Arc::new(TerminalClaim::new()),
                        Arc::new(AtomicBool::new(false)),
                        None,
                        ingress.clone(),
                        None,
                        Arc::default(),
                        None,
                        Arc::default(),
                    ));
                    let initial = connection.send_request(NewSessionRequest::new(cwd.path().to_path_buf()))
                        .block_task().await?;
                    let old = initial.session_id;
                    assert_eq!(old.0.as_ref(), "s-fence-reset");
                    shared.commit_session(old.clone(), true).await?;
                    assert!(matches!(event_rx.recv().await,
                        Some(Event::AcpSessionAssigned { acp_session_id, connection_ready: true, .. }) if acp_session_id == "s-fence-reset"));
                    let mut session = Session {
                        connection,
                        shared,
                        control: None,
                        acp_session_id: old.clone(),
                        channels: SessionChannels::default(),
                        steering_capable: false,
                        close_capable: false,
                        source_profile: None,
                        default_effort: None,
                        default_model_config: Default::default(),
                        default_mode: None,
                        default_model: None,
                        agent_cwd: cwd.path().to_path_buf(),
                        mcp_servers: Vec::new(),
                        cmd_rx,
                        lifecycle_rx,
                        pending_prompts: VecDeque::new(),
                    };
                    let notification = || serde_json::from_value(serde_json::json!({
                        "sessionId":"s-fence-reset","update":{
                            "sessionUpdate":"agent_message_chunk",
                            "content":{"type":"text","text":"non-parked progress"}
                        }
                    })).unwrap();
                    let mut held = Some(ingress.request(&old, RequestPolicy::ParentOnly).await?);
                    assert!(!ingress.has_parked());
                    drop(ingress.request(&old, RequestPolicy::ParentOnly)
                        .now_or_never().expect("admitted callbacks share progress")?);
                    drop(ingress.notification(notification(), None)
                        .now_or_never().expect("old-session progress shares admission")?
                        .expect("old-session notification admitted"));
                    if expire {
                        tokio::time::pause();
                    }
                    let bound = if expire { Duration::from_millis(100) } else { Duration::from_secs(10) };
                    let (respond_to, response) = oneshot::channel();
                    {
                        let reset = session.reset_session(
                            "/clear".into(), tokio::time::Instant::now() + bound, respond_to,
                        );
                        tokio::pin!(reset);
                        assert!(futures_util::poll!(reset.as_mut()).is_pending());
                        assert_eq!(ingress.current(), Some(old.clone()));
                        assert_eq!(received.lock().await.len(), 1);
                        assert!(matches!(event_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
                        let mut later = std::pin::pin!(ingress.request(&old, RequestPolicy::ParentOnly));
                        assert!(futures_util::poll!(later.as_mut()).is_pending());
                        if expire {
                            tokio::time::advance(bound).await;
                            reset.as_mut().await?;
                            tokio::time::resume();
                            assert!(matches!(response.await.unwrap(),
                                ResetSessionOutcome::Failed { message }
                                if message == "reset deadline expired while agent callbacks were in flight"));
                            assert!(held.is_some());
                            drop(later.as_mut().await?);
                            drop(ingress.notification(notification(), None)
                                .now_or_never().expect("expired writer releases notification progress")?
                                .expect("original notification remains admitted"));
                            assert_eq!(ingress.current(), Some(old.clone()));
                            assert_eq!(received.lock().await.len(), 1);
                            assert!(matches!(event_rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
                        } else {
                            drop(held.take());
                            let (reset_result, later_result) = futures_util::join!(reset.as_mut(), later.as_mut());
                            reset_result?;
                            assert!(matches!(response.await.unwrap(),
                                ResetSessionOutcome::Reset { new_acp_session_id }
                                if new_acp_session_id == "s-fence-reset-reset-1"));
                            let stale = later_result.map(|_| ()).unwrap_err();
                            assert_eq!(stale.message, "sessionId does not identify the active native session");
                        }
                    }
                    drop(held.take());
                    if expire {
                        let (respond_to, response) = oneshot::channel();
                        session.reset_session(
                            "/clear".into(), tokio::time::Instant::now() + Duration::from_secs(10), respond_to,
                        ).await?;
                        assert!(matches!(response.await.unwrap(),
                            ResetSessionOutcome::Reset { new_acp_session_id }
                            if new_acp_session_id == "s-fence-reset-reset-1"));
                    }
                    let fresh = SessionId::new("s-fence-reset-reset-1");
                    assert_eq!(ingress.current(), Some(fresh.clone()));
                    drop(ingress.request(&fresh, RequestPolicy::ParentOnly).await?);
                    let mut events = Vec::new();
                    while let Ok(event) = event_rx.try_recv() {
                        events.push(event);
                    }
                    assert!(matches!(events.as_slice(), [
                        Event::SessionCleared,
                        Event::SessionContextReset { .. },
                        Event::AcpSessionAssigned { acp_session_id },
                        Event::Stopped { reason },
                    ] if acp_session_id == "s-fence-reset-reset-1" && reason == "session_reset"), "{events:?}");
                    let received = received.lock().await;
                    assert_eq!(received.len(), 2);
                    assert!(received.iter().all(|frame| frame["method"] == "session/new"));
                    assert_ne!(received[0]["id"], received[1]["id"]);
                    Ok(())
                },
            )).await.expect("real reset fixture completes").expect("SDK connection completes");
            agent.abort();
            let _ = agent.await;
        }
    }
}
