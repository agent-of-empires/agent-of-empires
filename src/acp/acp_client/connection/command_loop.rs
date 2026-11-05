//! The between-prompt command loop over an established session, including
//! the driven conversation reset.

use crate::acp::state::Event;
use agent_client_protocol::schema::v1::{
    CancelNotification, McpServer, NewSessionRequest, NewSessionResponse, SessionConfigId,
    SessionConfigOption, SessionConfigValueId, SessionId, SetSessionConfigOptionRequest,
    SetSessionModeRequest,
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
use crate::acp::acp_client::commands::{ClientCmd, QueuedClientCmd, QueuedPrompt};
use crate::acp::acp_client::config_options::{
    config_option_failure_event, config_options_event, dispatch_set_config_option,
    dispatch_set_mode, mode_config_id, model_option, modes_available_event,
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
use crate::acp::acp_client::session_identity::{spawn_native_request, OrderedRequestResult};
use crate::acp::establishment::{
    EstablishmentTarget, NativeEstablishmentReceipt, NativeOperationCorrelation,
    NativeRequestContext, NativeRequestScope,
};
use crate::acp::event_sender::EventSender;
use crate::acp::protocol::AcpOriginalRequestResult;

pub(super) struct Session {
    pub(super) connection: ConnectionTo<Agent>,
    pub(super) shared: Arc<Shared>,
    pub(super) control: Option<Arc<DaemonControlClient>>,
    pub(super) acp_session_id: SessionId,
    pub(super) channels: SessionChannels,
    pub(super) steering_capable: bool,
    pub(super) close_capable: bool,
    pub(super) source_profile: Option<String>,
    pub(super) default_effort: Option<String>,
    pub(super) default_mode: Option<String>,
    pub(super) default_model: Option<String>,
    pub(super) agent_cwd: PathBuf,
    /// Capability-filtered servers, forwarded again by a driven reset.
    pub(super) mcp_servers: Vec<McpServer>,
    pub(super) cmd_rx: mpsc::Receiver<QueuedClientCmd>,
    pub(super) lifecycle_rx: mpsc::Receiver<LifecycleEnvelope>,
    /// Steers the adapter handed back unconsumed, run as ordinary turns.
    /// Kept here rather than re-sent on the bounded channel this task drains.
    pub(super) pending_prompts: VecDeque<QueuedPrompt>,
    pub(super) pending_reset: Option<PendingReset>,
}

pub(super) struct PendingReset {
    proposal: Arc<crate::acp::supervisor::LiveFreshProposal>,
    original_scope: Option<NativeRequestContext>,
    correlation: NativeOperationCorrelation,
    text: String,
    result: oneshot::Receiver<Result<CompletedReset, ResetFailure>>,
    respond_to: Option<oneshot::Sender<ResetSessionOutcome>>,
    deadline: tokio::time::Instant,
}

pub(super) struct CompletedReset {
    response: NewSessionResponse,
    receipt: Arc<NativeEstablishmentReceipt>,
}

pub(super) enum ResetFailure {
    Busy,
    Failed(String),
}

impl From<String> for ResetFailure {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

impl From<&str> for ResetFailure {
    fn from(message: &str) -> Self {
        Self::Failed(message.into())
    }
}

pub(super) fn current_effect<T>(
    context: &Option<NativeRequestContext>,
    native: bool,
    effect: impl FnOnce() -> T,
) -> Result<T, agent_client_protocol::Error> {
    match context {
        Some(context) => {
            let _guard = context.try_effect_guard().map_err(|error| {
                acp_internal_error(format!(
                    "NotIssued: native request permission unavailable: {error}"
                ))
            })?;
            Ok(effect())
        }
        None if !native => Ok(effect()),
        None => Err(acp_internal_error(
            "NotIssued: native request context is missing".into(),
        )),
    }
}

pub(super) async fn current_effect_wait<T>(
    context: Option<&NativeRequestContext>,
    native: bool,
    state: &super::ConnectionState,
    effect: impl FnOnce() -> T,
) -> Result<T, agent_client_protocol::Error> {
    let Some(context) = context else {
        return current_effect(&None, native, effect);
    };
    let mut deadline = None;
    loop {
        let until = match context.try_effect_guard() {
            Ok(_guard) => return Ok(effect()),
            Err(error) => {
                if !error.is::<crate::process::worker_registry::RegistryBusy>() {
                    return Err(acp_internal_error(format!(
                        "NotIssued: native request permission unavailable: {error}"
                    )));
                }
                #[cfg(test)]
                if let Some(entered) = state.test_prompt_admission_busy.lock().unwrap().take() {
                    let _ = entered.send(());
                }
                let until = *deadline.get_or_insert_with(|| {
                    tokio::time::Instant::now() + std::time::Duration::from_secs(3)
                });
                if tokio::time::Instant::now() >= until {
                    return Err(acp_internal_error(format!(
                        "NotIssued: native request permission unavailable: {error}"
                    )));
                }
                until
            }
        };
        tokio::select! {
            biased;
            _ = state.wait_callback_end() => return Err(acp_internal_error("NotIssued: native prompt admission cancelled".into())),
            _ = tokio::time::sleep_until((tokio::time::Instant::now() + std::time::Duration::from_millis(10)).min(until)) => {}
        }
    }
}

pub(super) fn scoped_sender(
    shared: &Shared,
    native: bool,
    context: &Option<NativeRequestContext>,
) -> EventSender {
    if native {
        shared.event_tx.with_context(context.clone().map(Arc::new))
    } else {
        shared.event_tx.clone()
    }
}

pub(super) async fn publish_scoped(
    shared: &Shared,
    native: bool,
    context: &Option<NativeRequestContext>,
    sender: &EventSender,
    event: Event,
    available_only: bool,
) -> bool {
    let Ok(permit) = sender.reserve().await else {
        return false;
    };
    let current = context
        .as_ref()
        .map(NativeRequestContext::try_effect_guard)
        .transpose();
    let stale = current
        .as_ref()
        .is_err_and(|error| !error.is::<crate::process::worker_registry::RegistryBusy>());
    let permit = if stale || (native && context.is_none()) {
        permit.historical()
    } else {
        permit
    };
    let send = || permit.send(event);
    if available_only {
        shared.connection_state.admit(send).is_ok()
    } else {
        send();
        true
    }
}

pub(super) fn require_operation(
    context: &Option<NativeRequestContext>,
    correlation: Option<NativeOperationCorrelation>,
    native: bool,
) -> Result<(), agent_client_protocol::Error> {
    if !native {
        return Ok(());
    }
    let Some(context) = context else {
        return Err(acp_internal_error(
            "NotIssued: native operation context is missing".into(),
        ));
    };
    if correlation.is_none()
        || context.correlation() != correlation
        || !matches!(&context.scope, NativeRequestScope::Operation { .. })
    {
        return Err(acp_internal_error(
            "NotIssued: native operation correlation is missing or mismatched".into(),
        ));
    }
    Ok(())
}
impl Session {
    pub(super) async fn send_cancel(
        &self,
        scope: Option<NativeRequestContext>,
    ) -> Result<(), agent_client_protocol::Error> {
        match self.control.as_ref() {
            Some(control) => {
                let context = scope.ok_or_else(|| {
                    acp_internal_error(
                        "NotIssued: native cancel requires retained request context".into(),
                    )
                })?;
                if !matches!(
                    &context.scope,
                    NativeRequestScope::Operation { .. }
                        | NativeRequestScope::Original {
                            operation: Some(_),
                            ..
                        }
                ) {
                    return Err(acp_internal_error(
                        "NotIssued: native cancel requires operation permission".into(),
                    ));
                }
                current_effect(&Some(context.clone()), true, || ())?;
                control
                    .cancel(context.scope)
                    .await
                    .map_err(|error| acp_internal_error(error.to_string()))
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

    pub(super) async fn dispatch_config_option(
        &mut self,
        config_id: String,
        value: String,
        scope: Option<NativeRequestContext>,
    ) {
        let shared = Arc::clone(&self.shared);
        let native = self.control.is_some();
        let sender = scoped_sender(&shared, native, &scope);

        let admission = current_effect(&scope, native, || {
            if self
                .channels
                .model_option
                .as_ref()
                .is_some_and(|option| option.id == config_id)
            {
                self.default_model = Some(value.clone());
            }
        });

        if let Err(error) = admission {
            publish_scoped(
                &shared,
                native,
                &scope,
                &sender,
                config_option_failure_event(
                    config_id,
                    value,
                    public_error_message(&error),
                    ConfigOptionDispatchPurpose::Generic,
                ),
                false,
            )
            .await;
            return;
        }

        dispatch_set_config_option(
            &self.connection,
            &shared.connection_state,
            SetSessionConfigOptionRequest::new(
                self.acp_session_id.clone(),
                SessionConfigId::new(config_id),
                SessionConfigValueId::new(value),
            ),
            ConfigOptionDispatchPurpose::Generic,
            sender,
            scope,
        );
    }

    pub(super) fn dispatch_mode(
        &self,
        mode_id: String,
        while_prompting: bool,
        scope: Option<NativeRequestContext>,
    ) {
        dispatch_set_mode(
            &self.connection,
            &self.shared.connection_state,
            SetSessionModeRequest::new(self.acp_session_id.clone(), mode_id),
            &self.channels,
            scoped_sender(&self.shared, self.control.is_some(), &scope),
            while_prompting,
            scope,
        );
    }

    pub(super) fn refuse_unavailable_command(&self, command: &mut Option<QueuedClientCmd>) -> bool {
        if self.shared.connection_state.loss().is_none()
            && !self.shared.connection_state.is_stopping()
            && self.pending_reset.is_none()
        {
            return false;
        }

        let Some(QueuedClientCmd {
            command: command_value,
            scope,
            correlation,
        }) = command.take()
        else {
            return false;
        };

        match command_value {
            ClientCmd::ResetSession { respond_to, .. } => {
                let _ = respond_to.send(ResetSessionOutcome::Failed {
                    message: "ACP native input is unavailable or a reset is in progress".into(),
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
            ClientCmd::Prompt(_)
            | ClientCmd::Cancel
            | ClientCmd::SetMode(_)
            | ClientCmd::SetConfigOption { .. } => {}
            independent_or_stop => {
                *command = Some(QueuedClientCmd {
                    command: independent_or_stop,
                    scope,
                    correlation,
                });
                return false;
            }
        }
        true
    }

    pub(super) async fn run(mut self) -> Result<(), agent_client_protocol::Error> {
        let mut idle_tick = tokio::time::interval(BETWEEN_PROMPT_IDLE_CHECK_INTERVAL);
        idle_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            // Do not copy an ingress SID into current business state. A reset may
            // have settled native ingress without retaining current effect permission.
            if self.shared.ingress.current().is_none() && self.pending_reset.is_none() {
                return Err(acp_internal_error(
                    "native session is not established".into(),
                ));
            }

            let reset_deadline = self
                .pending_reset
                .as_ref()
                .filter(|pending| pending.respond_to.is_some())
                .map(|pending| pending.deadline);

            let mut queued = match self
                .shared
                .connection_state
                .loss()
                .is_none()
                .then(|| self.pending_prompts.pop_front())
                .flatten()
            {
                Some(QueuedPrompt {
                    blocks,
                    scope,
                    correlation,
                }) => Some(QueuedClientCmd {
                    command: ClientCmd::Prompt(blocks),
                    scope,
                    correlation,
                }),
                None => tokio::select! {
                    _ = self.shared.connection_state.wait_ordered_unavailable() => {
                        return Err(acp_internal_error(
                            "agent connection unavailable".into(),
                        ));
                    }
                    command = self.cmd_rx.recv() => command,
                    completed = async {
                        match self.pending_reset.as_mut() {
                            Some(pending) => (&mut pending.result).await,
                            None => std::future::pending().await,
                        }
                    } => {
                        self.finish_reset(completed).await?;
                        continue;
                    }
                    _ = async {
                        match reset_deadline {
                            Some(deadline) => tokio::time::sleep_until(deadline).await,
                            None => std::future::pending().await,
                        }
                    } => {
                        if let Some(responder) = self.pending_reset.as_mut()
                            .and_then(|pending| pending.respond_to.take())
                        {
                            let _ = responder.send(ResetSessionOutcome::Failed {
                                message: "agent did not settle session/new before the reset deadline"
                                    .into(),
                            });
                        }
                        continue;
                    }
                    _ = idle_tick.tick() => {
                        if self.pending_reset.is_none() {
                            self.between_prompt_idle_tick().await;
                        }
                        continue;
                    }
                },
            };

            if self.refuse_unavailable_command(&mut queued) {
                continue;
            }

            let Some(QueuedClientCmd {
                command,
                scope,
                correlation,
            }) = queued
            else {
                break;
            };

            match command {
                ClientCmd::Prompt(blocks) => {
                    if self
                        .run_prompt(QueuedPrompt {
                            blocks,
                            scope,
                            correlation,
                        })
                        .await?
                    {
                        break;
                    }
                }
                ClientCmd::Cancel => {
                    if let Err(error) =
                        require_operation(&scope, correlation, self.control.is_some())
                    {
                        debug!(target: "acp.protocol", %error, "cancel not issued");
                        continue;
                    }
                    if let Err(error) = self.send_cancel(scope.clone()).await {
                        warn!(target: "acp.protocol", %error, "idle cancel failed");
                        continue;
                    }
                    if self.stand_down_idle_completion(&scope) {
                        let sender = scoped_sender(&self.shared, self.control.is_some(), &scope);
                        publish_scoped(
                            &self.shared,
                            self.control.is_some(),
                            &scope,
                            &sender,
                            Event::Stopped {
                                reason: "cancelled".into(),
                            },
                            true,
                        )
                        .await;
                    }
                }
                ClientCmd::ForceStop => {
                    if require_operation(&scope, correlation, self.control.is_some()).is_err()
                        || current_effect(&scope, self.control.is_some(), || ()).is_err()
                    {
                        continue;
                    }
                    self.stand_down_idle_completion(&scope);
                    if let Err(error) = self.send_cancel(scope).await {
                        debug!(target: "acp.protocol", %error, "cancel notification failed");
                    }
                }
                ClientCmd::SetMode(mode_id) => {
                    self.dispatch_mode(mode_id, false, scope);
                }
                ClientCmd::SetConfigOption { config_id, value } => {
                    self.dispatch_config_option(config_id, value, scope).await;
                }
                ClientCmd::DeleteSession {
                    acp_session_id,
                    respond_to,
                } => handle_delete_session_cmd(
                    &self.connection,
                    &self.shared.connection_state,
                    acp_session_id,
                    respond_to,
                    scope,
                ),
                ClientCmd::CloseSession(respond_to) => {
                    crate::acp::acp_client::close::dispatch_close(
                        &self.connection,
                        &self.shared.connection_state,
                        self.acp_session_id.clone(),
                        self.close_capable,
                        respond_to,
                        scope,
                    );
                }
                ClientCmd::ResumeBackgroundTailing(launches) => {
                    let receipt = self
                        .control
                        .as_ref()
                        .and_then(|control| control.original_receipt());
                    self.shared
                        .resume_background_tailing(launches, receipt)
                        .await;
                }
                ClientCmd::ResumeTaskTailing(tasks) => {
                    let receipt = self
                        .control
                        .as_ref()
                        .and_then(|control| control.original_receipt());
                    self.shared.resume_task_tailing(tasks, receipt).await;
                }
                ClientCmd::ResetSession {
                    text,
                    proposal,
                    deadline,
                    respond_to,
                } => {
                    self.reset_session(text, proposal, deadline, respond_to, scope, correlation)
                        .await?;
                }
                #[cfg(test)]
                ClientCmd::FlushForTest(done) => {
                    let _ = done.send(());
                }
                ClientCmd::Shutdown => break,
            }
        }
        Ok(())
    }

    /// Claim an idle terminal without overriding an already-completed turn.
    fn stand_down_idle_completion(&self, scope: &Option<NativeRequestContext>) -> bool {
        current_effect(scope, self.control.is_some(), || {
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
        })
        .unwrap_or(false)
    }

    async fn between_prompt_idle_tick(&self) {
        if self.control.is_some() {
            return;
        }
        let shared = &self.shared;
        if shared.connection_state.loss().is_some() {
            return;
        }
        let Some(cost_seen) = shared.between_prompt.take_idle_fire(now_ms()) else {
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
            .emit_while_available([Event::Stopped {
                reason: reason.into(),
            }])
            .await;
    }

    /// The SDK owns original settlement even after the caller deadline.
    async fn reset_session(
        &mut self,
        text: String,
        proposal: Arc<crate::acp::supervisor::LiveFreshProposal>,
        deadline: tokio::time::Instant,
        respond_to: oneshot::Sender<ResetSessionOutcome>,
        original_scope: Option<NativeRequestContext>,
        correlation: Option<NativeOperationCorrelation>,
    ) -> Result<(), agent_client_protocol::Error> {
        let refusal = if tokio::time::Instant::now() >= deadline {
            Some("reset deadline expired before admission".to_owned())
        } else {
            require_operation(&original_scope, correlation, true)
                .and_then(|()| current_effect(&original_scope, true, || ()))
                .err()
                .map(|error| public_error_message(&error))
        };

        if let Some(message) = refusal {
            let _ = respond_to.send(ResetSessionOutcome::Failed { message });
            return Ok(());
        }

        let Some(control) = self.control.clone() else {
            let _ = respond_to.send(ResetSessionOutcome::Failed {
                message: "native establishment requires correlated authority".into(),
            });
            return Ok(());
        };
        let Some(correlation) = correlation else {
            let _ = respond_to.send(ResetSessionOutcome::Failed {
                message: "NotIssued: native reset operation correlation is missing".into(),
            });
            return Ok(());
        };

        let shared = Arc::clone(&self.shared);
        let original = Arc::clone(&proposal);
        let request =
            NewSessionRequest::new(self.agent_cwd.clone()).mcp_servers(self.mcp_servers.clone());
        let (result_tx, result) = oneshot::channel();

        if let Err(error) = self.connection.spawn(async move {
            let outcome =
                Self::drive_live_fresh(shared, control, original, request, deadline).await;
            let _ = result_tx.send(outcome);
            Ok(())
        }) {
            warn!(target: "acp.establishment", %error, "could not own live Fresh");
            let _ = respond_to.send(ResetSessionOutcome::Failed {
                message: "native reset could not be admitted".into(),
            });
            return Ok(());
        }

        debug!(target: "acp.establishment", requested = %text, "queued explicit live Fresh");
        self.pending_reset = Some(PendingReset {
            proposal,
            original_scope,
            correlation,
            text,
            result,
            respond_to: Some(respond_to),
            deadline,
        });
        Ok(())
    }

    async fn drive_live_fresh(
        shared: Arc<Shared>,
        control: Arc<DaemonControlClient>,
        proposal: Arc<crate::acp::supervisor::LiveFreshProposal>,
        request: NewSessionRequest,
        deadline: tokio::time::Instant,
    ) -> Result<CompletedReset, ResetFailure> {
        let state = &shared.connection_state;
        let write = Arc::clone(&state.callback_duration).write_owned();
        tokio::pin!(write);

        #[cfg(test)]
        let write = std::future::poll_fn(|cx| {
            let result = std::future::Future::poll(write.as_mut(), cx);
            if result.is_pending() {
                if let Some(entered) = state.test_fresh_write_pending.lock().unwrap().take() {
                    let _ = entered.send(());
                }
            }
            result
        });
        #[cfg(test)]
        tokio::pin!(write);

        let duration = tokio::select! {
            biased;
            _ = state.wait_callback_end() => return Err("native reset was cancelled".into()),
            guard = tokio::time::timeout_at(deadline, &mut write) => {
                guard.map_err(|_| {
                    "reset deadline expired while callbacks were in flight".to_owned()
                })?
            }
        };

        let reverse = control.quiesce_reverse(deadline).await.map_err(|error| {
            format!("reverse replies did not settle before native reset: {error}")
        })?;
        // A retained terminal must requalify before this reset owns native exclusion.
        // Otherwise its nonblocking guard would contend with the reset's own lock.
        if shared.native_compacting() {
            return Err(ResetFailure::Busy);
        }
        let namespace = tokio::select! {
            biased;
            _ = state.wait_callback_end() => return Err("native reset was cancelled".into()),
            namespace = proposal.namespace(deadline) => {
                namespace.map_err(|_| {
                    "reset deadline expired before native exclusion".to_owned()
                })?
            }
        };
        let identity = tokio::select! {
            biased;
            _ = state.wait_callback_end() => return Err("native reset was cancelled".into()),
            guard = tokio::time::timeout_at(deadline, shared.ingress.fence.write()) => {
                guard.map_err(|_| {
                    "reset deadline expired while notifications were in flight".to_owned()
                })?
            }
        };

        if shared.between_prompt.work_state().is_busy()
            || !shared.bg_tasks.is_empty()
            || shared.native_compacting()
        {
            return Err(ResetFailure::Busy);
        }

        let cut = proposal.projection().emission_cut();
        let processed = tokio::select! {
            biased;
            _ = state.wait_callback_end() => false,
            processed = proposal.projection().processed_through(cut, deadline) => processed,
        };
        if !processed {
            return Err("original SDK projection did not settle before the reset deadline".into());
        }

        state.usable.store(false, Ordering::Release);
        let arm = Arc::clone(&proposal).arm(namespace, deadline);
        tokio::pin!(arm);
        let mut identity = Some(identity);
        let mut duration = Some(duration);
        let mut reverse = Some(reverse);

        let armed = tokio::select! {
            result = &mut arm => result,
            _ = tokio::time::sleep_until(deadline) => {
                drop(identity.take());
                drop(duration.take());
                drop(reverse.take());
                arm.await
            }
        };
        let armed = match armed {
            Ok(armed) => armed,
            Err(error) => {
                drop(identity.take());
                drop(duration.take());
                drop(reverse.take());
                warn!(target: "acp.establishment", %error, "live Fresh arm refused");
                if proposal.old_is_current().await {
                    state.record_usable();
                }
                return Err("native Fresh authority is unconfirmed".into());
            }
        };

        let may_issue = armed.begun
            && armed.admitted
            && armed.confirmed
            && identity.is_some()
            && duration.is_some()
            && reverse.is_some()
            && tokio::time::Instant::now() < deadline
            && state.admit(|| ()).is_ok();

        if !may_issue {
            drop(identity.take());
            drop(duration.take());
            drop(reverse.take());
            if armed.begun {
                Self::settle_unissued_fresh(&shared).await;
            }
            shared.ingress.revoke_requests();
            state.decline_establishment();
            return Err(
                "native Fresh was not issued; its consumed authority remains unresolved".into(),
            );
        }

        let scope = match proposal.native_scope() {
            Ok(scope) => scope,
            Err(_) => {
                drop(identity.take());
                drop(duration.take());
                drop(reverse.take());
                Self::settle_unissued_fresh(&shared).await;
                return Err("native Fresh admission changed before emission".into());
            }
        };

        control.set_establishment_context(proposal.store(), Some(scope));
        let application = match crate::acp::acp_client::control::EstablishmentApplication::new(
            Arc::clone(&control),
        ) {
            Ok(application) => application,
            Err(_) => {
                drop(identity.take());
                drop(duration.take());
                drop(reverse.take());
                Self::settle_unissued_fresh(&shared).await;
                return Err("previous original result has not finished".into());
            }
        };

        shared.begin_native_compaction_original(proposal.owner(), proposal.attempt().id);
        shared.ingress.begin();
        if let Err(error) = control.install_original_ingress() {
            drop(identity.take());
            drop(duration.take());
            drop(reverse.take());
            Self::settle_unissued_fresh(&shared).await;
            return Err(error.to_string().into());
        }
        drop(identity.take());
        drop(duration.take());
        drop(reverse.take());

        let result = crate::acp::acp_client::control::establish_session::<NewSessionResponse>(
            &control,
            "session/new",
            &request,
        )
        .await;

        // Snapshot this invocation's receipt before another projection window can
        // become visible. It remains evidence, not permission for a replacement op.
        let receipt = control.projection.receipt_snapshot();
        let (response, id) = match result {
            Ok(response) => {
                let id = response.session_id.clone();
                (Some(response), Ok(id))
            }
            Err(failure) => {
                warn!(target: "acp.establishment", ?failure, "original live Fresh failed");
                (None, Err(failure))
            }
        };

        let settled = Self::settle_establishment(&shared, &control, id, None, false).await;
        drop(application);
        settled.map_err(|_| {
            "native Fresh or its original projection remains unconfirmed".to_owned()
        })?;

        let response = response.ok_or_else(|| "native Fresh result was not usable".to_owned())?;
        let receipt =
            receipt.ok_or_else(|| "original native Fresh receipt was not retained".to_owned())?;

        if !matches!(&receipt.attempt.target, EstablishmentTarget::New)
            || receipt.attempt.as_ref() != proposal.attempt().as_ref()
            || receipt.owner != proposal.owner()
            || !receipt.same_store(proposal.store().as_ref())
            || receipt.session_id() != Some(response.session_id.0.as_ref())
            || !receipt.replay.closed
            || receipt.replay.lost
        {
            return Err("original native Fresh receipt did not match its invocation".into());
        }

        Ok(CompletedReset { response, receipt })
    }

    async fn settle_unissued_fresh(shared: &Arc<Shared>) {
        if let Some(delivery) = shared
            .event_tx
            .close_establishment(Some(
                crate::acp::establishment::EstablishmentBlockReason::NotIssued,
            ))
            .await
        {
            delivery.settled().await;
        }
        shared.ingress.revoke_requests();
        shared.connection_state.decline_establishment();
    }

    async fn finish_reset(
        &mut self,
        completion: Result<Result<CompletedReset, ResetFailure>, oneshot::error::RecvError>,
    ) -> Result<(), agent_client_protocol::Error> {
        let Some(mut pending) = self.pending_reset.take() else {
            return Ok(());
        };

        let shared = Arc::clone(&self.shared);
        let native = self.control.is_some();
        let mut scope = pending.original_scope.clone();
        let mut sender = scoped_sender(&shared, native, &scope);

        let result = match completion.unwrap_or_else(|_| {
            Err(ResetFailure::Failed(
                "native reset continuation ended".into(),
            ))
        }) {
            Err(ResetFailure::Busy) => {
                publish_scoped(
                    &shared,
                    native,
                    &scope,
                    &sender,
                    Event::PromptRejected {
                        reason: "agent_busy".into(),
                        text: pending.text,
                    },
                    false,
                )
                .await;
                if let Some(responder) = pending.respond_to.take() {
                    let _ = responder.send(ResetSessionOutcome::Failed {
                        message: "agent work is still in flight; wait for it to finish before clearing the conversation"
                            .into(),
                    });
                }
                return Ok(());
            }
            Err(ResetFailure::Failed(message)) => Err(message),
            Ok(CompletedReset { response, receipt }) => {
                let clear_after_seq = pending
                    .proposal
                    .projection()
                    .applied_clear_after_seq(&receipt)
                    .map_err(|error| {
                        format!("original native replay boundary was not confirmed: {error}")
                    });
                // Never relabel the old permission. Construct only this original
                // reset's result continuation from its genuinely successful New.
                let continuation = NativeRequestContext::new(
                    NativeRequestScope::Original {
                        attempt: pending.proposal.attempt(),
                        owner: pending.proposal.owner(),
                        store: pending.proposal.store().map(Arc::new),
                        operation: Some(pending.correlation),
                    },
                    None,
                )
                .with_original_evidence(receipt);

                match continuation {
                    Err(error) => Err(format!(
                        "original native Fresh evidence was not confirmed: {error}"
                    )),
                    Ok(context) => {
                        scope = Some(context);
                        sender = scoped_sender(&shared, native, &scope)
                            .with_outcome(AcpOriginalRequestResult::Succeeded);

                        let projected = pending
                            .proposal
                            .projection()
                            .applied()
                            .as_ref()
                            .is_some_and(|postimage| {
                                postimage.attempt_id == pending.proposal.attempt().id
                                    && postimage.choice.acp_session_id.as_deref()
                                        == Some(response.session_id.0.as_ref())
                            });
                        let ready = self
                            .control
                            .as_ref()
                            .is_some_and(|control| control.native_session_ready());

                        let admission = if let Err(message) = &clear_after_seq {
                            Err(message.clone())
                        } else if projected && ready && pending.proposal.current().await {
                            current_effect(&scope, native, || {
                                self.acp_session_id = response.session_id.clone();
                                self.channels = SessionChannels::new(
                                    response.modes.as_ref(),
                                    response.config_options.as_deref(),
                                );
                                shared.terminal_claim.clear_failed_outcome();
                                shared.connection_state.record_usable();
                            })
                            .map_err(|error| public_error_message(&error))
                        } else {
                            Err("native reset was superseded or its channel is unavailable".into())
                        };

                        // Response history survives stale business permission.
                        if let Some(modes) = &response.modes {
                            publish_scoped(
                                &shared,
                                native,
                                &scope,
                                &sender,
                                modes_available_event(modes),
                                false,
                            )
                            .await;
                        }
                        if let Some(event) = config_options_event(response.config_options.clone()) {
                            publish_scoped(&shared, native, &scope, &sender, event, false).await;
                        }

                        match admission {
                            Err(message) => Err(message),
                            Ok(()) => {
                                let cut = clear_after_seq
                                    .expect("clear admission checked certified original boundary");
                                for event in [
                                    Event::SessionCleared,
                                    Event::SessionContextReset {
                                        reason:
                                            "conversation cleared; the agent started a fresh session"
                                                .into(),
                                    },
                                    Event::AcpSessionAssigned {
                                        acp_session_id: response.session_id.0.to_string(),
                                        connection_ready: true,
                                        native_subject: None,
                                    },
                                ] {
                                    let event_sender = if matches!(event, Event::SessionCleared) {
                                        sender.clone().with_clear_after_seq(cut)
                                    } else {
                                        sender.clone()
                                    };
                                    publish_scoped(
                                        &shared, native, &scope, &event_sender, event, false,
                                    )
                                    .await;
                                }

                                if tokio::time::Instant::now() < pending.deadline {
                                    self.reapply_defaults(
                                        &response.session_id,
                                        response.config_options.as_deref(),
                                        pending.deadline,
                                        scope.clone(),
                                    )
                                    .await;
                                }

                                publish_scoped(
                                    &shared,
                                    native,
                                    &scope,
                                    &sender,
                                    Event::Stopped {
                                        reason: "session_reset".into(),
                                    },
                                    false,
                                )
                                .await;
                                Ok(response.session_id.0.to_string())
                            }
                        }
                    }
                }
            }
        };

        let outcome = match result {
            Ok(new_acp_session_id) if tokio::time::Instant::now() < pending.deadline => {
                ResetSessionOutcome::Reset { new_acp_session_id }
            }
            Ok(_) => ResetSessionOutcome::Failed {
                message: "native reset settled after the caller deadline".into(),
            },
            Err(message) => {
                // Do not turn a genuinely positive New receipt into a negative
                // Native ACK merely because its current continuation was refused.
                publish_scoped(
                    &shared,
                    native,
                    &scope,
                    &sender,
                    Event::PromptRuntimeError {
                        message: message.clone(),
                        evidence: Some(crate::session::OperationErrorEvidence::new(
                            crate::session::ErrorEvidenceSource::LocalPromptResult,
                            crate::session::ErrorOperation::ContextReset,
                        )),
                        native_subject: None,
                    },
                    false,
                )
                .await;
                publish_scoped(
                    &shared,
                    native,
                    &scope,
                    &sender,
                    Event::Stopped {
                        reason: "session_reset_failed".into(),
                    },
                    false,
                )
                .await;
                ResetSessionOutcome::Failed { message }
            }
        };

        // No permission check or fallible early return can strand this waiter.
        if let Some(responder) = pending.respond_to.take() {
            let _ = responder.send(outcome);
        }
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
        scope: Option<NativeRequestContext>,
    ) {
        let mut effort_id = options.and_then(thought_level_config_id);
        if let (Some(model), Some(option)) = (
            self.default_model.as_deref(),
            options.and_then(model_option),
        ) {
            if !option.is_current(model) {
                match self
                    .reset_set_option(
                        new_id,
                        SessionConfigId::new(option.id.clone()),
                        model,
                        deadline,
                        true,
                        scope.clone(),
                    )
                    .await
                {
                    Some(Ok(options)) => effort_id = thought_level_config_id(&options),
                    Some(Err(_)) => {}
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
                continue;
            };
            if self
                .reset_set_option(new_id, config_id, value, deadline, false, scope.clone())
                .await
                .is_none()
            {
                return;
            }
        }
    }

    /// The SDK owns request, duration and publication beyond this waiter's cap.
    /// `None` once the reset deadline passes skips all remaining defaults.
    async fn reset_set_option(
        &self,
        new_id: &SessionId,
        config_id: SessionConfigId,
        value: &str,
        deadline: tokio::time::Instant,
        report_failure: bool,
        scope: Option<NativeRequestContext>,
    ) -> Option<Result<Vec<SessionConfigOption>, String>> {
        let request = || async {
            current_effect(&scope, self.control.is_some(), || ())?;
            let shared = Arc::clone(&self.shared);
            let native = self.control.is_some();
            let result_scope = scope.clone();
            let failure_id = config_id.0.to_string();
            let failure_value = value.to_owned();
            let (sender, receiver) = oneshot::channel();

            spawn_native_request(
                &self.connection,
                &self.shared.connection_state,
                scope.clone(),
                SetSessionConfigOptionRequest::new(
                    new_id.clone(),
                    config_id,
                    SessionConfigValueId::new(value.to_owned()),
                ),
                move |request| async move {
                    let received = request.wait().await;

                    #[cfg(test)]
                    if matches!(&received, OrderedRequestResult::Correlated(_)) {
                        let gate = shared
                            .connection_state
                            .test_default_publish
                            .lock()
                            .unwrap()
                            .take();
                        if let Some((entered, release)) = gate {
                            let _ = entered.send(());
                            tokio::select! {
                                biased;
                                _ = shared.connection_state.wait_callback_end() => {}
                                _ = release => {}
                            }
                        }
                    }

                    let event_sender = scoped_sender(&shared, native, &result_scope);
                    let result = match received {
                        OrderedRequestResult::Correlated(Ok(response)) => {
                            if let Some(event) =
                                config_options_event(Some(response.config_options.clone()))
                            {
                                publish_scoped(
                                    &shared,
                                    native,
                                    &result_scope,
                                    &event_sender,
                                    event,
                                    false,
                                )
                                .await;
                            }
                            OrderedRequestResult::Correlated(Ok(response.config_options))
                        }
                        OrderedRequestResult::Correlated(Err(error)) => {
                            OrderedRequestResult::Correlated(Err(error))
                        }
                        OrderedRequestResult::Refused(error) => {
                            OrderedRequestResult::Refused(error)
                        }
                        OrderedRequestResult::Unavailable(loss) => {
                            OrderedRequestResult::Unavailable(loss)
                        }
                        OrderedRequestResult::Abandoned => OrderedRequestResult::Abandoned,
                    };

                    if report_failure {
                        let reason = match &result {
                            OrderedRequestResult::Correlated(Err(error))
                            | OrderedRequestResult::Refused(error) => {
                                Some(public_error_message(error))
                            }
                            OrderedRequestResult::Abandoned => {
                                Some("Config response was not observed".into())
                            }
                            _ => None,
                        };
                        if let Some(reason) = reason {
                            publish_scoped(
                                &shared,
                                native,
                                &result_scope,
                                &event_sender,
                                config_option_failure_event(
                                    failure_id,
                                    failure_value,
                                    reason,
                                    ConfigOptionDispatchPurpose::Generic,
                                ),
                                false,
                            )
                            .await;
                        }
                    }

                    let _ = sender.send(result);
                },
            )
            .map_err(|error| acp_internal_error(error.to_string()))?;

            receiver
                .await
                .unwrap_or(OrderedRequestResult::Abandoned)
                .into_result()
        };

        match await_reset_request(deadline, request).await {
            Ok(options) => Some(Ok(options)),
            Err(ResetRequestError::Acp(error)) => {
                warn!(
                    target: "acp.protocol",
                    session = %self.shared.session_label,
                    value,
                    ?error,
                    "re-applying structured view default after reset failed"
                );
                Some(Err(public_error_message(&error)))
            }
            Err(ResetRequestError::TimedOut) => {
                warn!(
                    target: "acp.protocol",
                    session = %self.shared.session_label,
                    value,
                    timeout_secs = SESSION_RESET_IN_TASK_TIMEOUT.as_secs(),
                    "post-reset config re-application timed out; skipping remaining defaults"
                );
                None
            }
        }
    }
}
