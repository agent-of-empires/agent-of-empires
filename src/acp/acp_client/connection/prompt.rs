//! One in-flight prompt: driving `session/prompt` concurrently with commands,
//! lifecycle signals, the silent-orphan and cancel-escalation watchdogs, and
//! mid-turn steering.

use crate::acp::state::{AcpConnectionSource, Event, RateLimitInfo};
use agent_client_protocol::v1::schema::{ContentBlock, PromptRequest, PromptResponse, StopReason};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::time::{Instant, Sleep};
use tracing::{debug, info, trace, warn};

use super::command_loop::Session;
use super::notifications::now_ms;
use super::CANCEL_ESCALATION_GRACE;
use crate::acp::acp_client::commands::{ClientCmd, PromptInput};
use crate::acp::acp_client::control::{prompt_outcome_to_response, ControlPromptResult};
use crate::acp::acp_client::delete::handle_delete_session_cmd;
use crate::acp::acp_client::errors::{acp_internal_error, prompt_runtime_error, AcpError};
use crate::acp::acp_client::lifecycle::{LifecycleSignal, OffProtocolWorkKind};
use crate::acp::acp_client::opencode::recover_opencode_prompt_error;
use crate::acp::acp_client::rate_limit::{
    captured_rate_limit_reset, classify_rate_limit_error, classify_rate_limit_from_message,
    clear_captured_rejections, take_rate_limit_from_captured_rejection,
};
use crate::acp::acp_client::reset::ResetSessionOutcome;
use crate::acp::acp_client::session_identity::{ordered_native_request, OrderedRequestResult};
use crate::acp::acp_client::steer::{first_text_block, SteerOutcome, SteerRequest};
use crate::acp::acp_client::watchdog::{
    silent_orphan_check_interval, silent_orphan_fast_grace, silent_orphan_grace,
    terminal_stop_reason, SilentOrphanWatchdog, SilentOrphanWatchdogConfig,
    OFF_PROTOCOL_WORK_GRACE_FLOOR,
};
use crate::daemon::PromptAttachmentRef;

type AcpResult<T> = Result<T, agent_client_protocol::Error>;
type PromptFut = Pin<Box<dyn Future<Output = PromptResult> + Send>>;
type SteerFut =
    Pin<Box<dyn Future<Output = (PromptInput, OrderedRequestResult<serde_json::Value>)> + Send>>;
type PromptResult = OrderedRequestResult<PromptResponse>;

struct LocalPromptScope(std::sync::Arc<super::notifications::Shared>);

impl Drop for LocalPromptScope {
    fn drop(&mut self) {
        self.0.prompt_in_flight.store(false, Ordering::Release);
        self.0.local_prompt_settled.notify_waiters();
    }
}

#[cfg(test)]
pub(super) struct SelectProbe {
    pub(super) gate: Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
    pub(super) armed: bool,
    pub(super) winner: Option<tokio::sync::oneshot::Sender<bool>>,
}

#[cfg(test)]
tokio::task_local! {
    // Scoped to this connection future, not inherited by spawned tasks.
    pub(super) static SELECT_PROBE: std::cell::RefCell<SelectProbe>;
}

#[cfg(test)]
fn observe_select_win(command: bool) {
    let _ = SELECT_PROBE.try_with(|probe| {
        let mut probe = probe.borrow_mut();
        if probe.armed {
            if let Some(winner) = probe.winner.take() {
                let _ = winner.send(command);
            }
        }
    });
}

enum Flow {
    Continue,
    Break,
}

/// End-of-turn flags; precedence lives in `terminal_stop_reason`.
#[derive(Default)]
struct TurnFlags {
    shutdown: bool,
    rate_limit: Option<RateLimitInfo>,
    force_stopped: bool,
    prompt_orphaned: bool,
    agent_unresponsive: bool,
    /// The adapter resolved with `StopReason::Cancelled`.
    prompt_cancelled: bool,
    prompt_failed: bool,
    orphan_cancel_sent: bool,
    cancelling: bool,
    connection_lost: bool,
    result_observed: bool,
    /// What the adapter reported; `None` when no `PromptResponse` arrived.
    agent_stop_reason: Option<StopReason>,
    /// The error the agent answered `session/prompt` with.
    rejection: Option<agent_client_protocol::Error>,
}

struct Turn {
    epoch: u64,
    /// Handed back on a rejection so the user can retry it.
    prompt_text: String,
    prompt_attachments: Vec<PromptAttachmentRef>,
    flags: TurnFlags,
    preceding: Vec<Event>,
    watchdog: SilentOrphanWatchdog,
    cfg: SilentOrphanWatchdogConfig,
    orphan_check_enabled: bool,
    orphan_check_period: Duration,
    orphan_check: Pin<Box<Sleep>>,
    cancel_grace: Pin<Box<Sleep>>,
    /// At most one steer is outstanding so racing steers cannot reorder the
    /// user's messages; later ones wait in the backlog.
    steer_fut: Option<SteerFut>,
    steer_backlog: VecDeque<PromptInput>,
}

impl Turn {
    fn apply(&mut self, signal: LifecycleSignal) {
        self.watchdog.apply_signal(
            signal,
            Instant::now(),
            crate::time::Timestamp::now(),
            self.cfg,
        );
    }

    fn start_cancel_grace(&mut self, deadline: Instant) {
        self.flags.cancelling = true;
        self.cancel_grace.as_mut().reset(deadline);
    }
}

/// Debug-only fault injection: suppress the prompt response once so the
/// silent-orphan watchdog must break the loop (#1240).
fn simulate_orphan(session_label: &str) -> bool {
    if !cfg!(debug_assertions)
        || std::env::var("AOE_ACP_SIMULATE_ORPHAN_NEXT_PROMPT")
            .ok()
            .as_deref()
            != Some("1")
    {
        return false;
    }
    warn!(
        target: "acp.protocol",
        session = %session_label,
        "AOE_ACP_SIMULATE_ORPHAN_NEXT_PROMPT set; suppressing prompt_fut completion to trigger silent-orphan watchdog"
    );
    std::env::remove_var("AOE_ACP_SIMULATE_ORPHAN_NEXT_PROMPT");
    true
}

impl Session {
    /// Runs one operation. A connection-loss event is itself terminal; it does
    /// not manufacture a second `Stopped` or a peer prompt failure.
    pub(super) async fn run_prompt(&mut self, prompt: PromptInput) -> AcpResult<bool> {
        let PromptInput {
            blocks,
            attachments,
        } = prompt;
        let shared = self.shared.clone();
        let publication = shared.state.publication.lock().await;
        let epoch = {
            let _admission = shared
                .connection_state
                .admission
                .lock()
                .expect("connection admission lock");
            if shared.connection_state.loss().is_some() || shared.connection_state.is_stopping() {
                return Ok(false);
            }
            if let Some(control) = self.control.as_ref() {
                control.supersede_adopted_turn();
            }
            shared.reset_message_dedup();
            shared.seed_user_replay.store(false, Ordering::Relaxed);
            if shared
                .suppress_history_replay
                .swap(false, Ordering::Relaxed)
            {
                info!(
                    target: "acp.protocol",
                    session = %shared.session_label,
                    "first user prompt after session/load; resuming notification pump"
                );
            }
            // This prompt owns the next terminal; every idle-completion path for
            // an adopted or agent-initiated turn stands down.
            shared
                .prompt_sent_since_attach
                .store(true, Ordering::Relaxed);
            shared.adopted_turn_active.store(false, Ordering::Relaxed);
            *shared
                .last_prompt
                .lock()
                .expect("last prompt mutex poisoned") = (
                blocks
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text(t) => Some(t.text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
                    .trim()
                    .to_owned(),
                0,
            );
            shared.prompt_in_flight.store(true, Ordering::Relaxed);
            shared.terminal_claim.begin_turn();
            shared.between_prompt.reset_for_prompt();
            clear_captured_rejections(&shared.rate_limit_rejections);
            *shared.cancel_deadline.lock().unwrap() = None;
            info!(target: "acp.protocol", "sending prompt ({} content blocks)", blocks.len());
            // Bump the epoch before sending so envelopes from a handler parked on
            // the previous prompt are discarded.
            shared.prompt_epoch.fetch_add(1, Ordering::Relaxed) + 1
        };
        drop(publication);
        let _scope = LocalPromptScope(shared.clone());
        while self.lifecycle_rx.try_recv().is_ok() {}

        let base_grace = silent_orphan_grace(self.source_profile.as_deref());
        let orphan_check_period = silent_orphan_check_interval();
        let mut turn = Turn {
            epoch,
            preceding: Vec::new(),
            prompt_text: first_text_block(&blocks),
            prompt_attachments: attachments,
            flags: TurnFlags::default(),
            watchdog: SilentOrphanWatchdog::default(),
            cfg: SilentOrphanWatchdogConfig {
                base_grace,
                fast_grace: silent_orphan_fast_grace(),
                off_protocol_grace_floor: OFF_PROTOCOL_WORK_GRACE_FLOOR,
            },
            orphan_check_enabled: base_grace > Duration::ZERO,
            orphan_check_period,
            orphan_check: Box::pin(tokio::time::sleep(orphan_check_period)),
            cancel_grace: Box::pin(tokio::time::sleep(CANCEL_ESCALATION_GRACE)),
            steer_fut: None,
            steer_backlog: VecDeque::new(),
        };
        let prompt_started_at_ms = now_ms();
        let mut prompt_fut = self.send_prompt(blocks).await;
        let simulate_orphan = simulate_orphan(&shared.session_label);

        loop {
            #[cfg(test)]
            self.select_probe_gate().await;
            let flow = tokio::select! {
                // The prompt first, then commands and their deadline ahead of
                // lifecycle traffic, which a busy agent keeps always ready.
                biased;
                res = &mut prompt_fut, if !simulate_orphan => {
                    self.on_prompt_result(&mut turn, res).await?
                }
                observation = shared.connection_state.wait_ordered_unavailable() => {
                    self.on_prompt_result(&mut turn, PromptResult::Unavailable(observation)).await?
                }
                cmd = self.cmd_rx.recv() => {
                    #[cfg(test)]
                    observe_select_win(true);
                    self.on_command(&mut turn, cmd).await?
                }
                _ = turn.cancel_grace.as_mut(),
                    if turn.flags.cancelling && shared.connection_state.loss().is_none() => {
                    warn!(
                        target: "acp.protocol",
                        session = %shared.session_label,
                        grace_secs = CANCEL_ESCALATION_GRACE.as_secs(),
                        "agent ignored session/cancel past grace window; escalating to runner restart"
                    );
                    turn.flags.agent_unresponsive = true;
                    turn.flags.shutdown = true;
                    Flow::Break
                }
                env = self.lifecycle_rx.recv() => {
                    #[cfg(test)]
                    observe_select_win(false);
                    // None: the handler dropped; another arm ends the loop.
                    match env {
                        Some(env) if env.epoch == turn.epoch && shared.connection_state.loss().is_none() => turn.apply(env.signal),
                        Some(env) => trace!(
                            target: "acp.protocol",
                            session = %shared.session_label,
                            envelope_epoch = env.epoch,
                            current_epoch = turn.epoch,
                            "discarding stale lifecycle envelope across prompt boundary"
                        ),
                        None => {}
                    }
                    Flow::Continue
                }
                _ = turn.orphan_check.as_mut(),
                    if turn.orphan_check_enabled && !turn.flags.orphan_cancel_sent
                        && shared.connection_state.loss().is_none() =>
                {
                    self.on_orphan_check(&mut turn).await
                }
                (prompt, res) = async {
                    turn.steer_fut.as_mut().expect("guarded by the arm condition").await
                }, if turn.steer_fut.is_some() => {
                    self.on_steer_result(&mut turn, prompt, res).await;
                    Flow::Continue
                }
            };
            if let Flow::Break = flow {
                break;
            }
        }
        if turn.flags.connection_lost {
            return Ok(false);
        }
        Ok(self.finish_turn(turn, prompt_started_at_ms).await)
    }

    async fn send_prompt(&self, blocks: Vec<ContentBlock>) -> PromptFut {
        let request = PromptRequest::new(self.acp_session_id.clone(), blocks);
        let Some(control) = self.control.as_ref() else {
            let request =
                ordered_native_request(&self.connection, &self.shared.connection_state, request);
            return Box::pin(async move {
                match request {
                    Ok(request) => request.wait().await,
                    Err(AcpError::ConnectionUnavailable(observation)) => {
                        PromptResult::Unavailable(observation)
                    }
                    Err(error) => PromptResult::Refused(acp_internal_error(error.to_string())),
                }
            });
        };
        let params = match serde_json::to_value(request) {
            Ok(params) => params,
            Err(error) => {
                return Box::pin(std::future::ready(PromptResult::Refused(
                    acp_internal_error(format!("serialize prompt params: {error}")),
                )));
            }
        };
        let state = self.shared.connection_state.clone();
        let receiver = control.prompt(params).await;
        Box::pin(async move {
            match receiver {
                Ok(rx) => match rx.await {
                    Ok(ControlPromptResult::Outcome(
                        crate::acp::control_protocol::PromptOutcome::Aborted,
                    )) => PromptResult::Abandoned,
                    Ok(ControlPromptResult::Outcome(outcome)) => {
                        PromptResult::Correlated(prompt_outcome_to_response(outcome))
                    }
                    Ok(ControlPromptResult::NativeLost) => PromptResult::Unavailable(
                        state.loss().expect("ordered native loss observed"),
                    ),
                    Err(_) if state.loss().is_some() => {
                        PromptResult::Unavailable(state.loss().expect("control loss observed"))
                    }
                    Err(_) => PromptResult::Abandoned,
                },
                Err(AcpError::ConnectionUnavailable(observation)) => {
                    PromptResult::Unavailable(observation)
                }
                Err(error) => PromptResult::Refused(acp_internal_error(error.to_string())),
            }
        })
    }

    #[cfg(test)]
    async fn select_probe_gate(&mut self) {
        if self.lifecycle_rx.is_empty() {
            return;
        }
        let gate = SELECT_PROBE
            .try_with(|probe| probe.borrow_mut().gate.take())
            .ok()
            .flatten();
        if let Some((ready, resume)) = gate {
            ready.send(()).expect("test receives readiness");
            resume.await.expect("test queues Cancel before resuming");
            assert!(!self.cmd_rx.is_empty());
            assert!(!self.lifecycle_rx.is_empty());
            SELECT_PROBE.with(|probe| probe.borrow_mut().armed = true);
        }
    }

    async fn on_prompt_result(&mut self, turn: &mut Turn, result: PromptResult) -> AcpResult<Flow> {
        turn.flags.result_observed = !matches!(&result, PromptResult::Unavailable(_));
        let (result, correlated) = match result {
            PromptResult::Correlated(result) => (result, true),
            PromptResult::Refused(error) => (Err(error), false),
            PromptResult::Unavailable(observation) => {
                if observation.source == AcpConnectionSource::Native {
                    turn.flags.connection_lost = true;
                    return Ok(Flow::Break);
                }
                return Err(acp_internal_error("ACP connection unavailable".into()));
            }
            PromptResult::Abandoned => {
                turn.preceding.push(Event::PromptRuntimeError {
                    message: "Prompt completion was not observed".into(),
                    evidence: Some(crate::session::OperationErrorEvidence::new(
                        crate::session::ErrorEvidenceSource::LocalPromptResult,
                        crate::session::ErrorOperation::Prompt,
                    )),
                    native_subject: None,
                });
                turn.flags.prompt_failed = true;
                return Ok(Flow::Break);
            }
        };
        if let Ok(response) = &result {
            turn.flags.prompt_cancelled = matches!(response.stop_reason, StopReason::Cancelled);
            turn.flags.agent_stop_reason = Some(response.stop_reason);
        }
        if correlated {
            turn.flags.rate_limit = self.completion_rate_limit(&result);
            if turn.flags.rate_limit.is_some() {
                return Ok(Flow::Break);
            }
        }
        let error = match result {
            Ok(_) => return Ok(Flow::Break),
            Err(error) => error,
        };
        if correlated {
            if turn.flags.cancelling || turn.flags.orphan_cancel_sent {
                turn.flags.prompt_cancelled = true;
            } else {
                turn.flags.rejection = Some(error);
                if let Err(error) = self.send_cancel().await {
                    debug!(target: "acp.protocol", %error, "cancel after a rejected prompt failed");
                }
            }
            return Ok(Flow::Break);
        }
        warn!(target: "acp.protocol", session = %self.shared.session_label, ?error, "prompt operation failed; preserving connection and conversation");
        let mut diagnostic = prompt_runtime_error(
            &error,
            crate::session::ErrorEvidenceSource::LocalPromptResult,
        );
        if !correlated {
            if let Event::PromptRuntimeError {
                evidence: Some(evidence),
                ..
            } = &mut diagnostic
            {
                evidence.reported_code = None;
            }
        }
        turn.preceding.push(diagnostic);
        turn.flags.prompt_failed = true;
        Ok(Flow::Break)
    }

    /// The adapter can end a limited turn normally, so a still-future rejected
    /// window captured during it is the only evidence; park like the error path.
    pub(super) fn completion_rate_limit(
        &self,
        result: &AcpResult<PromptResponse>,
    ) -> Option<RateLimitInfo> {
        let shared = &self.shared;
        match result {
            Err(error) => {
                let captured = captured_rate_limit_reset(
                    &shared.rate_limit_rejections,
                    crate::time::Timestamp::now(),
                );
                return classify_rate_limit_error(error, captured.as_ref()).or_else(|| {
                    classify_rate_limit_from_message(&error.message, captured.as_ref()).map(
                        |mut info| {
                            info.status = super::super::errors::public_error_message(error);
                            info
                        },
                    )
                });
            }
            Ok(response) if matches!(response.stop_reason, StopReason::Cancelled) => return None,
            Ok(_) => {}
        }
        let info = take_rate_limit_from_captured_rejection(
            &shared.rate_limit_rejections,
            crate::time::Timestamp::now(),
        )?;
        info!(
            target: "acp.protocol",
            session = %shared.session_label,
            resets_at = ?info.resets_at,
            "prompted turn ended on a rejected rate-limit window; parking session"
        );
        Some(info)
    }

    /// Starts the cancel escalation grace once and returns its deadline; a
    /// repeated cancel keeps the original deadline.
    pub(super) async fn request_cancel(&self) -> Instant {
        let deadline = Instant::now() + CANCEL_ESCALATION_GRACE;
        {
            let mut current = self.shared.cancel_deadline.lock().unwrap();
            if let Some(deadline) = *current {
                return deadline;
            }
            *current = Some(deadline);
        }
        let escalates_at = crate::time::Timestamp::now()
            + crate::time::Duration::try_from(CANCEL_ESCALATION_GRACE).unwrap();
        self.shared
            .emit(Event::CancelRequested { escalates_at })
            .await;
        deadline
    }

    async fn reject_busy(&self, text: String, attachments: Vec<PromptAttachmentRef>) {
        self.shared
            .emit(Event::PromptRejected {
                reason: "agent_busy".into(),
                text,
                attachments,
            })
            .await;
    }

    async fn on_command(&mut self, turn: &mut Turn, cmd: Option<ClientCmd>) -> AcpResult<Flow> {
        let mut cmd = cmd;
        if self.refuse_unavailable_command(&mut cmd) {
            return Ok(Flow::Continue);
        }
        match cmd {
            Some(ClientCmd::CancelChild {
                session_id,
                respond_to,
            }) => {
                let _ = respond_to.send(self.cancel_child(session_id).await);
            }
            Some(ClientCmd::Cancel) => {
                info!(target: "acp.protocol", "sending session/cancel during in-flight prompt");
                // Only the first cancel arms escalation and tells the UI. Arm it
                // before the send, so a control loss during it keeps the deadline.
                if !turn.flags.cancelling {
                    turn.start_cancel_grace(self.request_cancel().await);
                }
                self.send_cancel().await?;
            }
            Some(ClientCmd::ForceStop) => {
                warn!(
                    target: "acp.protocol",
                    "force-stop requested during in-flight prompt; ending turn and restarting worker"
                );
                if let Err(error) = self.send_cancel().await {
                    debug!(target: "acp.protocol", %error, "cancel notification failed");
                }
                turn.flags.force_stopped = true;
                turn.flags.shutdown = true;
                return Ok(Flow::Break);
            }
            Some(ClientCmd::SetConfigOption {
                config_id,
                value,
                remember,
            }) => self.dispatch_config_option(config_id, value, remember),
            Some(ClientCmd::SetMode(mode_id)) => self.dispatch_mode(mode_id, true),
            Some(ClientCmd::DeleteSession {
                acp_session_id,
                respond_to,
            }) => handle_delete_session_cmd(
                &self.connection,
                &self.shared.connection_state,
                acp_session_id,
                respond_to,
            ),
            Some(ClientCmd::ResumeBackgroundTailing(launches)) => {
                self.shared.resume_background_tailing(launches)
            }
            Some(ClientCmd::ResumeTaskTailing(task_ids)) => {
                self.shared
                    .resume_task_tailing(&self.acp_session_id.0, task_ids)
                    .await
            }
            Some(ClientCmd::Prompt(prompt)) => return Ok(self.on_follow_up(turn, prompt).await),
            Some(ClientCmd::ResetSession {
                text, respond_to, ..
            }) => {
                // Resetting now would orphan the pending turn on the old id.
                warn!(
                    target: "acp.protocol",
                    "conversation reset requested during in-flight prompt; refusing"
                );
                self.reject_busy(text, Vec::new()).await;
                let _ = respond_to.send(ResetSessionOutcome::Failed {
                    message: "a turn is in flight; stop it before clearing the conversation".into(),
                });
            }
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
                info!(
                    target: "acp.protocol",
                    "shutdown received during in-flight prompt; aborting turn"
                );
                turn.flags.shutdown = true;
                return Ok(Flow::Break);
            }
        }
        Ok(Flow::Continue)
    }

    async fn on_follow_up(&mut self, turn: &mut Turn, prompt: PromptInput) -> Flow {
        let label = self.shared.session_label.clone();
        // A follow-up while cancelling means the user force-ended and
        // re-typed: the agent is wedged. Reject first so nothing is lost.
        if turn.flags.cancelling {
            warn!(
                target: "acp.protocol",
                session = %label,
                "follow-up prompt arrived while cancel pending; escalating to runner restart"
            );
            self.reject_busy(prompt.text(), prompt.attachments).await;
            turn.flags.agent_unresponsive = true;
            turn.flags.shutdown = true;
            return Flow::Break;
        }
        // A `/compact` turn would swallow a steer with no reply and no Retry
        // pill, so it is rejected instead (#3219).
        let compacting =
            turn.watchdog.off_protocol_work_seen() == Some(OffProtocolWorkKind::Compaction);
        if self.steering_capable && !compacting {
            self.steer_or_backlog(turn, prompt);
        } else {
            warn!(
                target: "acp.protocol",
                compacting,
                "received Prompt while one is in flight and it cannot be steered into; rejecting"
            );
            self.reject_busy(prompt.text(), prompt.attachments).await;
        }
        Flow::Continue
    }

    fn steer_or_backlog(&self, turn: &mut Turn, prompt: PromptInput) {
        if turn.steer_fut.is_some() {
            turn.steer_backlog.push_back(prompt);
            return;
        }
        info!(
            target: "acp.protocol",
            session = %self.shared.session_label,
            "sending _session/steering during in-flight prompt ({} content blocks)",
            prompt.blocks.len()
        );
        let request = ordered_native_request(
            &self.connection,
            &self.shared.connection_state,
            SteerRequest::new(self.acp_session_id.clone(), prompt.blocks.clone()),
        );
        turn.steer_fut = Some(Box::pin(async move {
            let result = match request {
                Ok(request) => request.wait().await,
                Err(AcpError::ConnectionUnavailable(observation)) => {
                    OrderedRequestResult::Unavailable(observation)
                }
                Err(error) => OrderedRequestResult::Refused(acp_internal_error(error.to_string())),
            };
            (prompt, result)
        }));
    }

    async fn on_orphan_check(&mut self, turn: &mut Turn) -> Flow {
        let label = self.shared.session_label.clone();
        let subagents_active = self.shared.subagent_working(now_ms());
        let fire = turn
            .watchdog
            .should_fire(Instant::now(), turn.cfg, subagents_active)
            && !self.shared.aoe.leases.working(now_ms());
        let off_protocol = turn.watchdog.off_protocol_work_seen();
        let grace_secs = turn
            .watchdog
            .effective_grace(turn.cfg, subagents_active)
            .as_secs();
        if fire && turn.watchdog.cost_seen() && off_protocol.is_none() && !subagents_active {
            // The turn finished but the PromptResponse never came: end cleanly
            // as prompt_complete with no flag set (#2237).
            info!(
                target: "acp.protocol",
                session = %label,
                grace_secs,
                "silent-orphan watchdog: turn wrapped up (cost-populated usage) without PromptResponse; ending cleanly as prompt_complete"
            );
            // The runner may still be running the turn; without this the next
            // local prompt hits the stale waiter and kills the connection.
            if let Some(control) = self.control.as_ref() {
                control.release_local_prompt();
            }
            return Flow::Break;
        }
        if fire {
            warn!(
                target: "acp.protocol",
                session = %label,
                off_protocol_work = ?off_protocol,
                in_flight_tools = turn.watchdog.tool_calls_in_flight_len(),
                subagents_active,
                grace_secs,
                "silent-orphan watchdog fired: no progress past grace and no in-flight tools; sending session/cancel"
            );
            turn.flags.prompt_orphaned = true;
            // Armed before the send, like a user cancel, without telling the UI.
            if !turn.flags.cancelling {
                let deadline = Instant::now() + CANCEL_ESCALATION_GRACE;
                *self.shared.cancel_deadline.lock().unwrap() = Some(deadline);
                turn.start_cancel_grace(deadline);
            }
            if let Err(err) = self.send_cancel().await {
                warn!(
                    target: "acp.protocol",
                    session = %label,
                    error = %err,
                    "silent-orphan: session/cancel send failed; escalating immediately"
                );
                turn.flags.shutdown = true;
                return Flow::Break;
            }
            turn.flags.orphan_cancel_sent = true;
        }
        let next = Instant::now() + turn.orphan_check_period;
        turn.orphan_check.as_mut().reset(next);
        Flow::Continue
    }

    async fn on_steer_result(
        &mut self,
        turn: &mut Turn,
        prompt: PromptInput,
        res: OrderedRequestResult<serde_json::Value>,
    ) {
        turn.steer_fut = None;
        let res = match res {
            OrderedRequestResult::Correlated(result) => result,
            OrderedRequestResult::Refused(error) => Err(error),
            OrderedRequestResult::Unavailable(_) => {
                turn.steer_backlog.clear();
                return;
            }
            OrderedRequestResult::Abandoned => Err(acp_internal_error(
                "Steering response was not observed".into(),
            )),
        };
        let label = self.shared.session_label.clone();
        match res.as_ref().map(SteerOutcome::from_response) {
            Ok(SteerOutcome::Injected) => {
                info!(target: "acp.protocol", session = %label, "_session/steering injected into the running turn");
                // The running turn still owns its Stopped. Injection can
                // swallow an update the watchdog awaited, so it counts as
                // progress.
                turn.apply(LifecycleSignal::Progress);
            }
            Ok(SteerOutcome::PromptRequired) => {
                // The turn settled first and the content is untouched: run it
                // and anything parked behind it as ordinary turns, in order.
                info!(
                    target: "acp.protocol",
                    session = %label,
                    "_session/steering raced the turn's end; re-dispatching as a normal prompt"
                );
                self.pending_prompts.push_back(prompt);
                self.pending_prompts.extend(turn.steer_backlog.drain(..));
            }
            Ok(outcome @ (SteerOutcome::StartedNewTurn | SteerOutcome::Unknown)) => {
                // Consumed into a detached turn: resending would duplicate it.
                warn!(
                    target: "acp.protocol",
                    session = %label,
                    ?outcome,
                    "_session/steering returned an outcome that consumed the message without an owning request; the between-prompt watchdog will close the detached turn"
                );
            }
            Err(e) => {
                warn!(
                    target: "acp.protocol",
                    session = %label,
                    error = %e,
                    "_session/steering failed; falling back to agent_busy rejection"
                );
                self.reject_busy(prompt.text(), prompt.attachments).await;
            }
        }
        if let Some(next) = turn.steer_backlog.pop_front() {
            self.steer_or_backlog(turn, next);
        }
    }

    async fn finish_turn(&mut self, mut turn: Turn, prompt_started_at_ms: i64) -> bool {
        let shared = self.shared.clone();
        if !turn.flags.result_observed && shared.connection_state.loss().is_some() {
            return false;
        }
        let mut preceding = std::mem::take(&mut turn.preceding);
        // A cost marker can land in the same tick the prompt resolves (#2370).
        while let Ok(env) = self.lifecycle_rx.try_recv() {
            if env.epoch == turn.epoch {
                turn.apply(env.signal);
            }
        }
        let w = &turn.watchdog;
        let finished = w.cost_seen() && w.off_protocol_work_seen().is_none();
        if !turn.flags.prompt_failed
            && turn.flags.rejection.is_none()
            && shared.profile.key == "opencode"
            && finished
            && !w.saw_progress()
        {
            if let Some(message) =
                recover_opencode_prompt_error(&self.acp_session_id.0, prompt_started_at_ms)
            {
                preceding.push(Event::PromptRuntimeError {
                    message,
                    evidence: Some(crate::session::OperationErrorEvidence::new(
                        crate::session::ErrorEvidenceSource::TranscriptRecovery,
                        crate::session::ErrorOperation::Prompt,
                    )),
                    native_subject: None,
                });
                turn.flags.prompt_failed = true;
            }
        }
        let rejected = turn.flags.rejection.is_some();
        if let Some(error) = turn.flags.rejection.take() {
            preceding.push(prompt_runtime_error(
                &error,
                crate::session::ErrorEvidenceSource::LocalPromptResult,
            ));
            preceding.push(Event::PromptRejected {
                reason: "prompt_failed".into(),
                text: std::mem::take(&mut turn.prompt_text),
                attachments: std::mem::take(&mut turn.prompt_attachments),
            });
        }
        let f = &mut turn.flags;
        let reason = if rejected {
            "prompt_rejected"
        } else if f.rate_limit.is_none() && f.prompt_failed {
            "prompt_failed"
        } else {
            terminal_stop_reason(
                f.rate_limit.is_some(),
                f.force_stopped,
                f.prompt_orphaned,
                f.agent_unresponsive,
                f.shutdown,
                f.prompt_cancelled,
                f.orphan_cancel_sent && finished,
            )
        };
        if let Some(info) = f.rate_limit.take() {
            preceding.push(Event::RateLimit { info });
        }
        let terminal_at = now_ms();
        let failed = matches!(reason, "prompt_failed" | "prompt_rejected" | "rate_limited");
        let claimed = if failed {
            shared.terminal_claim.claim_failed()
        } else {
            shared.terminal_claim.claim()
        };
        if claimed {
            if failed {
                shared.between_prompt.deactivate();
            }
            if f.result_observed {
                shared.publish_terminal(reason.into(), preceding).await;
            } else {
                preceding.push(Event::Stopped {
                    reason: reason.into(),
                });
                if !shared.emit_while_available(preceding).await {
                    return false;
                }
            }
        }
        shared.reset_message_dedup();
        shared
            .last_prompt
            .lock()
            .expect("last prompt mutex poisoned")
            .1 = now_ms();
        *shared.cancel_deadline.lock().unwrap() = None;
        shared.prompt_in_flight.store(false, Ordering::Release);
        if shared.subagent_activity_at.load(Ordering::Relaxed) >= terminal_at
            || shared.subagent_working(now_ms())
        {
            shared.arm_for_subagent_activity().await;
        }
        // Missing after a Stopped, this line signals a stranded prompt (#3190).
        info!(
            target: "acp.protocol",
            session = %shared.session_label,
            reason,
            agent_stop_reason = ?f.agent_stop_reason,
            duration_ms = now_ms() - prompt_started_at_ms,
            "turn finished; between-prompt idle ownership restored"
        );
        turn.flags.shutdown
    }
}
