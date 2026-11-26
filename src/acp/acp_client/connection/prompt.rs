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

use super::command_loop::{
    current_effect, current_effect_wait, publish_scoped, require_operation, scoped_sender, Session,
};
use super::notifications::now_ms;
use super::CANCEL_ESCALATION_GRACE;
use crate::acp::acp_client::commands::{ClientCmd, QueuedClientCmd, QueuedPrompt};
use crate::acp::acp_client::control::{prompt_outcome_to_response, ControlPromptResult};
use crate::acp::acp_client::delete::handle_delete_session_cmd;
use crate::acp::acp_client::errors::{acp_internal_error, prompt_runtime_error, AcpError};
use crate::acp::acp_client::lifecycle::{LifecycleSignal, OffProtocolWorkKind};
use crate::acp::acp_client::opencode::recover_opencode_prompt_error;
use crate::acp::acp_client::rate_limit::{
    captured_rate_limit_reset, classify_rate_limit_error, clear_captured_rejections,
    take_rate_limit_from_captured_rejection,
};
use crate::acp::acp_client::reset::ResetSessionOutcome;
use crate::acp::acp_client::session_identity::{ordered_native_request, OrderedRequestResult};
use crate::acp::acp_client::steer::{SteerOutcome, SteerRequest};
use crate::acp::acp_client::watchdog::{
    silent_orphan_check_interval, silent_orphan_fast_grace, silent_orphan_grace,
    terminal_stop_reason, SilentOrphanWatchdog, SilentOrphanWatchdogConfig,
    OFF_PROTOCOL_WORK_GRACE_FLOOR,
};
use crate::acp::control_protocol::NativePromptCompletionEvidence;
use crate::acp::establishment::{NativeOperationCorrelation, NativeRequestContext};
use crate::acp::event_sender::EventSender;
use crate::acp::protocol::AcpOriginalRequestResult;

type AcpResult<T> = Result<T, agent_client_protocol::Error>;
type PromptResult = OrderedRequestResult<PromptResponse>;
type PromptFut = Pin<
    Box<dyn Future<Output = (PromptResult, Option<Box<NativePromptCompletionEvidence>>)> + Send>,
>;
type SteerFut =
    Pin<Box<dyn Future<Output = (QueuedPrompt, OrderedRequestResult<serde_json::Value>)> + Send>>;

async fn initial_prompt_cancelled(cancellation: Option<&tokio_util::sync::CancellationToken>) {
    match cancellation {
        Some(cancellation) => cancellation.cancelled().await,
        None => std::future::pending::<()>().await,
    }
}

struct LocalPromptScope {
    shared: std::sync::Arc<super::notifications::Shared>,
    terminal_epoch: u64,
    correlation: Option<NativeOperationCorrelation>,
}

impl Drop for LocalPromptScope {
    fn drop(&mut self) {
        self.shared
            .terminal_claim
            .with_current_epoch(self.terminal_epoch, || {
                self.shared.prompt_in_flight.store(false, Ordering::Release);
                self.shared.local_prompt_settled.notify_waiters();
                self.shared
                    .connection_state
                    .finish_initial_prompt_operation(self.correlation);
            });
    }
}

#[cfg(test)]
pub(crate) struct SelectProbe {
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
    pub(crate) static SELECT_PROBE: std::cell::RefCell<SelectProbe>;
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
    rejected: bool,
}

struct Turn {
    epoch: u64,
    terminal_epoch: u64,
    scope: Option<NativeRequestContext>,
    correlation: Option<NativeOperationCorrelation>,
    sender: EventSender,
    result_current: bool,
    flags: TurnFlags,
    preceding: Vec<Event>,
    retry_text: String,
    retry_attachments: Vec<crate::daemon::PromptAttachmentRef>,
    watchdog: SilentOrphanWatchdog,
    cfg: SilentOrphanWatchdogConfig,
    orphan_check_enabled: bool,
    orphan_check_period: Duration,
    orphan_check: Pin<Box<Sleep>>,
    cancel_grace: Pin<Box<Sleep>>,
    steer_fut: Option<SteerFut>,
    steer_backlog: VecDeque<QueuedPrompt>,
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
    async fn settle_initial_prompt(
        &self,
        scope: &Option<NativeRequestContext>,
        correlation: Option<NativeOperationCorrelation>,
        admitted: bool,
    ) {
        let event = Event::Stopped {
            reason: "user_stopped".into(),
        };
        if admitted {
            let native = self.control.is_some();
            let sender = scoped_sender(&self.shared, native, scope);
            publish_scoped(&self.shared, native, scope, &sender, event, true).await;
        } else {
            let _ = self
                .shared
                .event_tx
                .with_metadata(crate::acp::event_sender::EventMetadata::host(&event))
                .send(event)
                .await;
        }
        self.shared
            .connection_state
            .finish_initial_prompt_operation(correlation);
    }

    async fn reject_prompt_not_issued(
        &self,
        message: String,
        scope: &Option<NativeRequestContext>,
    ) {
        let native = self.control.is_some();
        let sender = scoped_sender(&self.shared, native, scope);
        for event in [
            Event::PromptRuntimeError {
                message,
                evidence: Some(crate::session::OperationErrorEvidence::new(
                    crate::session::ErrorEvidenceSource::LocalPromptResult,
                    crate::session::ErrorOperation::Prompt,
                )),
                native_subject: None,
            },
            Event::Stopped {
                reason: "prompt_failed".into(),
            },
        ] {
            publish_scoped(&self.shared, native, scope, &sender, event, true).await;
        }
    }
    /// Runs one operation. A connection-loss event is itself terminal; it does
    /// not manufacture a second `Stopped` or a peer prompt failure.
    pub(super) async fn run_prompt(&mut self, prompt: QueuedPrompt) -> AcpResult<bool> {
        let QueuedPrompt {
            input,
            scope,
            correlation,
        } = prompt;
        let retry_text = input.text();
        let super::super::commands::PromptInput {
            blocks,
            attachments,
        } = input;
        let shared = self.shared.clone();
        let native = self.control.is_some();
        let cancellation = shared
            .connection_state
            .initial_prompt_cancellation(correlation);

        if let Err(error) = require_operation(&scope, correlation, native) {
            self.reject_prompt_not_issued(error.to_string(), &scope)
                .await;
            return Ok(false);
        }
        let sender = scoped_sender(&shared, native, &scope);
        let (started, journaled) = if native {
            let permit = tokio::select! {
                biased;
                _ = initial_prompt_cancelled(cancellation.as_ref()) => {
                    self.settle_initial_prompt(&scope, correlation, false).await;
                    return Ok(false);
                }
                permit = sender.reserve() => match permit {
                    Ok(permit) => permit,
                    Err(_) => return Ok(false),
                },
            };
            let (confirmation, journaled) = tokio::sync::oneshot::channel();
            (Some((permit, confirmation)), Some(journaled))
        } else {
            (None, None)
        };

        #[cfg(test)]
        {
            let gate = shared
                .connection_state
                .test_prompt_admission
                .lock()
                .unwrap()
                .take();
            if let Some((entered, release)) = gate {
                let _ = entered.send(());
                tokio::select! {
                    biased;
                    _ = initial_prompt_cancelled(cancellation.as_ref()) => {
                        self.settle_initial_prompt(&scope, correlation, false).await;
                        return Ok(false);
                    }
                    _ = shared.connection_state.wait_callback_end() => return Ok(false),
                    _ = release => {}
                }
            }
        }

        let publication = shared.state.publication.lock().await;
        let admission = tokio::select! {
            biased;
            _ = initial_prompt_cancelled(cancellation.as_ref()) => {
                drop(publication);
                self.settle_initial_prompt(&scope, correlation, false).await;
                return Ok(false);
            }
            admitted = current_effect_wait(scope.as_ref(), native, &shared.connection_state, || {
                let _admission = shared
                    .connection_state
                    .admission
                    .lock()
                    .expect("connection admission lock");
                if shared.connection_state.loss().is_some() || shared.connection_state.is_stopping()
                {
                    return None;
                }
                if cancellation.as_ref().is_some_and(|cancellation| cancellation.is_cancelled()) {
                    return None;
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
                        .filter_map(|block| match block {
                            ContentBlock::Text(text) => Some(text.text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                        .trim()
                        .to_owned(),
                    0,
                );
                let terminal_epoch = shared.terminal_claim.begin_turn();
                *shared.cancel_deadline.lock().unwrap() = None;
                shared.prompt_in_flight.store(true, Ordering::Release);
                if !native {
                    shared.between_prompt.reset_for_prompt();
                }
                clear_captured_rejections(&shared.rate_limit_rejections);
                let epoch = shared.prompt_epoch.fetch_add(1, Ordering::Relaxed) + 1;
                if let Some((started, confirmation)) = started {
                    started.send_logical_admission(confirmation);
                }
                Some((epoch, terminal_epoch))
            }) => admitted,
        };
        drop(publication);

        let (epoch, terminal_epoch) = match admission {
            Ok(Some(admitted)) => admitted,
            Ok(None) => {
                if cancellation
                    .as_ref()
                    .is_some_and(|cancellation| cancellation.is_cancelled())
                {
                    self.settle_initial_prompt(&scope, correlation, false).await;
                }
                return Ok(false);
            }
            Err(error) => {
                self.reject_prompt_not_issued(error.to_string(), &scope)
                    .await;
                return Ok(false);
            }
        };

        // Installed immediately after the infallible synchronous admission block.
        // Every later return, including SDK/control failure, releases this slot.
        let _local_scope = LocalPromptScope {
            shared: shared.clone(),
            terminal_epoch,
            correlation,
        };
        while self.lifecycle_rx.try_recv().is_ok() {}

        let base_grace = silent_orphan_grace(self.source_profile.as_deref());
        let orphan_check_period = silent_orphan_check_interval();
        let mut turn = Turn {
            epoch,
            terminal_epoch,
            scope: scope.clone(),
            correlation,
            sender,
            result_current: true,
            flags: TurnFlags::default(),
            preceding: Vec::new(),
            retry_text,
            retry_attachments: attachments,
            watchdog: SilentOrphanWatchdog::default(),
            cfg: SilentOrphanWatchdogConfig {
                base_grace,
                fast_grace: silent_orphan_fast_grace(),
                off_protocol_grace_floor: OFF_PROTOCOL_WORK_GRACE_FLOOR,
            },
            orphan_check_enabled: !native && base_grace > Duration::ZERO,
            orphan_check_period,
            orphan_check: Box::pin(tokio::time::sleep(orphan_check_period)),
            cancel_grace: Box::pin(tokio::time::sleep(CANCEL_ESCALATION_GRACE)),
            steer_fut: None,
            steer_backlog: VecDeque::new(),
        };
        let prompt_started_at_ms = now_ms();
        if let Some(journaled) = journaled {
            let durable = tokio::select! {
                biased;
                _ = initial_prompt_cancelled(cancellation.as_ref()) => {
                    self.settle_initial_prompt(&scope, correlation, true).await;
                    return Ok(false);
                }
                _ = shared.connection_state.wait_callback_end() => false,
                result = journaled => matches!(result, Ok(true)),
            };
            if !durable {
                if let Some(observation) = shared.connection_state.loss() {
                    self.on_prompt_result(&mut turn, PromptResult::Unavailable(observation), None)
                        .await?;
                    if turn.flags.connection_lost {
                        return Ok(false);
                    }
                } else if shared.connection_state.is_stopping() {
                    turn.flags.shutdown = true;
                } else {
                    self.on_prompt_result(&mut turn, PromptResult::Refused(acp_internal_error(
                        "local prompt admission was not durably journaled; Native request was not issued".into(),
                    )), None).await?;
                }
                return Ok(self.finish_turn(turn, prompt_started_at_ms).await);
            }
        }
        if cancellation
            .as_ref()
            .is_some_and(|cancellation| cancellation.is_cancelled())
        {
            self.settle_initial_prompt(&scope, correlation, true).await;
            return Ok(false);
        }
        let mut prompt_fut = self.send_prompt(blocks, scope, correlation).await;
        if cancellation
            .as_ref()
            .is_some_and(|cancellation| cancellation.is_cancelled())
            && !shared.connection_state.initial_prompt_issued(correlation)
        {
            self.settle_initial_prompt(&turn.scope, correlation, true)
                .await;
            return Ok(false);
        }
        let simulate_orphan = !native && simulate_orphan(&shared.session_label);

        loop {
            #[cfg(test)]
            self.select_probe_gate().await;

            let flow = tokio::select! {
                biased;
                (result, evidence) = &mut prompt_fut, if !simulate_orphan => {
                    self.on_prompt_result(&mut turn, result, evidence).await?
                }
                observation = shared.connection_state.wait_ordered_unavailable() => {
                    self.on_prompt_result(
                        &mut turn,
                        PromptResult::Unavailable(observation),
                        None,
                    ).await?
                }
                command = self.cmd_rx.recv() => {
                    #[cfg(test)]
                    observe_select_win(true);
                    self.on_command(&mut turn, command).await?
                }
                _ = turn.cancel_grace.as_mut(),
                    if turn.flags.cancelling
                        && shared.connection_state.loss().is_none() =>
                {
                    if current_effect(&turn.scope, native, || ()).is_ok() {
                        turn.flags.agent_unresponsive = true;
                        turn.flags.shutdown = true;
                        Flow::Break
                    } else {
                        // Losing business permission does not cancel ownership of
                        // the already-admitted original completion waiter.
                        turn.flags.cancelling = false;
                        Flow::Continue
                    }
                }
                envelope = self.lifecycle_rx.recv() => {
                    #[cfg(test)]
                    observe_select_win(false);
                    match envelope {
                        Some(envelope)
                            if !native && envelope.epoch == turn.epoch
                                && shared.connection_state.loss().is_none() =>
                        {
                            turn.apply(envelope.signal);
                        }
                        Some(envelope) => trace!(
                            target: "acp.protocol",
                            envelope_epoch = envelope.epoch,
                            current_epoch = turn.epoch,
                            "discarding stale lifecycle envelope across prompt boundary"
                        ),
                        None => {}
                    }
                    Flow::Continue
                }
                _ = turn.orphan_check.as_mut(),
                    if turn.orphan_check_enabled
                        && !turn.flags.orphan_cancel_sent
                        && shared.connection_state.loss().is_none() =>
                {
                    self.on_orphan_check(&mut turn).await
                }
                (prompt, result) = async {
                    turn.steer_fut
                        .as_mut()
                        .expect("guarded by the arm condition")
                        .await
                }, if turn.steer_fut.is_some() => {

                    self.on_steer_result(&mut turn, prompt, result).await;
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

    async fn send_prompt(
        &self,
        blocks: Vec<ContentBlock>,
        scope: Option<NativeRequestContext>,
        correlation: Option<NativeOperationCorrelation>,
    ) -> PromptFut {
        if let Err(error) = require_operation(&scope, correlation, self.control.is_some()) {
            return Box::pin(std::future::ready((PromptResult::Refused(error), None)));
        }

        let request = PromptRequest::new(self.acp_session_id.clone(), blocks);
        let Some(control) = self.control.as_ref() else {
            let request = ordered_native_request(
                &self.connection,
                &self.shared.connection_state,
                scope,
                request,
            );
            return Box::pin(async move {
                let result = match request {
                    Ok(request) => request.wait().await,
                    Err(AcpError::ConnectionUnavailable(observation)) => {
                        PromptResult::Unavailable(observation)
                    }
                    Err(error) => PromptResult::Refused(acp_internal_error(error.to_string())),
                };
                (result, None)
            });
        };

        let Some(context) = scope else {
            return Box::pin(std::future::ready((
                PromptResult::Refused(acp_internal_error(
                    "NotIssued: native prompt requires retained request context".into(),
                )),
                None,
            )));
        };

        let params = match serde_json::to_value(request) {
            Ok(params) => params,
            Err(error) => {
                return Box::pin(std::future::ready((
                    PromptResult::Refused(acp_internal_error(format!(
                        "serialize prompt params: {error}"
                    ))),
                    None,
                )));
            }
        };
        let state = self.shared.connection_state.clone();
        let receiver = control.prompt(params, context.scope).await;

        Box::pin(async move {
            match receiver {
                Ok(receiver) => match receiver.await {
                    Ok(ControlPromptResult::Outcome {
                        outcome: crate::acp::control_protocol::PromptOutcome::Aborted,
                        evidence,
                    }) => (PromptResult::Abandoned, evidence),
                    Ok(ControlPromptResult::Outcome { outcome, evidence }) => (
                        PromptResult::Correlated(prompt_outcome_to_response(outcome)),
                        evidence,
                    ),
                    Ok(ControlPromptResult::NativeLost) => (
                        PromptResult::Unavailable(
                            state.loss().expect("ordered native loss observed"),
                        ),
                        None,
                    ),
                    Err(_) if state.loss().is_some() => (
                        PromptResult::Unavailable(state.loss().expect("control loss observed")),
                        None,
                    ),
                    Err(_) => (PromptResult::Abandoned, None),
                },
                Err(AcpError::ConnectionUnavailable(observation)) => {
                    (PromptResult::Unavailable(observation), None)
                }
                Err(error) => (
                    PromptResult::Refused(acp_internal_error(error.to_string())),
                    None,
                ),
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

    async fn on_prompt_result(
        &mut self,
        turn: &mut Turn,
        result: PromptResult,
        evidence: Option<Box<NativePromptCompletionEvidence>>,
    ) -> AcpResult<Flow> {
        let native = self.control.is_some();
        turn.flags.result_observed = !matches!(&result, PromptResult::Unavailable(_));

        let native_correlated = matches!(&result, PromptResult::Correlated(_));
        let evidence_matches = evidence.as_ref().is_some_and(|evidence| {
            turn.scope.as_ref().is_some_and(|context| {
                evidence.scope == context.scope
                    && context.origin() == Some(evidence.origin)
                    && evidence.scope.correlation() == turn.correlation
                    && turn.correlation.is_some()
            })
        });

        if native {
            turn.result_current = native_correlated && evidence_matches;
            if turn.result_current {
                turn.sender
                    .set_native_completion(evidence.as_ref().expect("matching Native evidence"));
            }
            if !turn.result_current {
                turn.sender = turn.sender.historical();
            }
        }
        let original_capture = if native && native_correlated && evidence_matches {
            self.shared.ingress.take_original_rate_limit_reset(
                evidence.as_deref().expect("matching Native evidence"),
                crate::time::Timestamp::now(),
            )
        } else {
            None
        };

        let (result, correlated) = match result {
            PromptResult::Correlated(result) => {
                if !native || evidence_matches {
                    turn.sender = turn.sender.with_outcome(if result.is_ok() {
                        AcpOriginalRequestResult::Succeeded
                    } else {
                        AcpOriginalRequestResult::Failed
                    });
                }
                (result, true)
            }
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

        let error = match result {
            Ok(response) => {
                turn.flags.prompt_cancelled = matches!(response.stop_reason, StopReason::Cancelled);
                turn.flags.agent_stop_reason = Some(response.stop_reason);
                if !native && !turn.flags.prompt_cancelled && turn.result_current {
                    let parking = current_effect(&turn.scope, native, || {
                        self.shared
                            .terminal_claim
                            .with_current_epoch(turn.terminal_epoch, || {
                                take_rate_limit_from_captured_rejection(
                                    &self.shared.rate_limit_rejections,
                                    crate::time::Timestamp::now(),
                                )
                            })
                            .flatten()
                    })
                    .ok()
                    .flatten();
                    if let Some(info) = parking {
                        turn.flags.rate_limit = Some(info);
                    }
                }
                return Ok(Flow::Break);
            }
            Err(error) => error,
        };

        let now = crate::time::Timestamp::now();
        let captured = if native {
            original_capture
        } else if turn.result_current {
            current_effect(&turn.scope, false, || {
                self.shared
                    .terminal_claim
                    .with_current_epoch(turn.terminal_epoch, || {
                        captured_rate_limit_reset(&self.shared.rate_limit_rejections, now)
                    })
                    .flatten()
            })
            .ok()
            .flatten()
        } else {
            None
        };

        if let Some(info) = correlated
            .then(|| classify_rate_limit_error(&error, captured.as_ref(), now))
            .flatten()
        {
            turn.flags.rate_limit = Some(info);
            return Ok(Flow::Break);
        }
        if correlated && (turn.flags.cancelling || turn.flags.orphan_cancel_sent) {
            turn.flags.prompt_cancelled = true;
            return Ok(Flow::Break);
        }

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
        if correlated {
            turn.flags.rejected = true;
            turn.preceding.push(Event::PromptRejected {
                reason: "prompt_failed".into(),
                text: std::mem::take(&mut turn.retry_text),
                attachments: std::mem::take(&mut turn.retry_attachments),
            });
            if !native && turn.result_current {
                let _ = self.send_cancel(turn.scope.clone()).await;
            }
        } else {
            turn.flags.prompt_failed = true;
        }
        Ok(Flow::Break)
    }

    /// Starts the cancel escalation grace once and returns its deadline; a
    /// repeated cancel keeps the original deadline.
    pub(super) async fn request_cancel(
        &self,
        scope: &Option<NativeRequestContext>,
        turn: u64,
    ) -> AcpResult<(Instant, bool)> {
        current_effect_wait(
            scope.as_ref(),
            self.control.is_some(),
            &self.shared.connection_state,
            || {
                self.shared.terminal_claim.with_current_epoch(turn, || {
                    let mut current = self.shared.cancel_deadline.lock().unwrap();
                    match current.as_ref() {
                        Some(grace) if grace.turn == turn => Some((grace.deadline, false)),
                        Some(_) => None,
                        None => {
                            let deadline = Instant::now() + CANCEL_ESCALATION_GRACE;
                            *current = Some(crate::acp::acp_client::CancelGrace {
                                deadline,
                                turn,
                                scope: scope.clone(),
                            });
                            Some((deadline, true))
                        }
                    }
                })
            },
        )
        .await?
        .flatten()
        .ok_or_else(|| acp_internal_error("NotIssued: cancellation turn was superseded".into()))
    }

    async fn reject_busy(
        &self,
        text: String,
        attachments: Vec<crate::daemon::PromptAttachmentRef>,
        scope: &Option<NativeRequestContext>,
    ) {
        let native = self.control.is_some();
        let sender = scoped_sender(&self.shared, native, scope);
        publish_scoped(
            &self.shared,
            native,
            scope,
            &sender,
            Event::PromptRejected {
                reason: "agent_busy".into(),
                text,
                attachments,
            },
            false,
        )
        .await;
    }

    async fn on_command(
        &mut self,
        turn: &mut Turn,
        command: Option<QueuedClientCmd>,
    ) -> AcpResult<Flow> {
        let mut command = command;
        if matches!(
            command.as_ref().map(|queued| &queued.command),
            Some(ClientCmd::InitialPromptIntent(_))
        ) {
            command = self
                .resolve_initial_command(command.take().expect("initial host intent"))
                .await;
            if command.is_none() {
                return Ok(Flow::Continue);
            }
        }
        if self.refuse_unavailable_command(&mut command) {
            return Ok(Flow::Continue);
        }
        let Some(QueuedClientCmd {
            command,
            scope,
            correlation,
        }) = command
        else {
            turn.flags.shutdown = true;
            return Ok(Flow::Break);
        };
        let native = self.control.is_some();

        if let ClientCmd::CancelInitialPrompt { id, expected } = &command {
            if turn.correlation != Some(*expected)
                || !self
                    .shared
                    .connection_state
                    .initial_prompt_matches(*id, *expected)
                || current_effect(&turn.scope, native, || ()).is_err()
            {
                return Ok(Flow::Continue);
            }
        }
        match command {
            ClientCmd::CancelChild {
                session_id,
                respond_to,
            } => {
                let sent = if require_operation(&scope, correlation, native).is_ok() {
                    self.cancel_child(session_id, scope).await
                } else {
                    false
                };
                let _ = respond_to.send(sent);
            }
            ClientCmd::Cancel | ClientCmd::CancelInitialPrompt { .. } => {
                if let Err(error) = require_operation(&scope, correlation, native) {
                    debug!(target: "acp.protocol", %error, "cancel not issued");
                    return Ok(Flow::Continue);
                }
                if current_effect_wait(
                    turn.scope.as_ref(),
                    native,
                    &self.shared.connection_state,
                    || (),
                )
                .await
                .is_err()
                {
                    return Ok(Flow::Continue);
                }
                let (deadline, newly_armed) =
                    match self.request_cancel(&scope, turn.terminal_epoch).await {
                        Ok(armed) => armed,
                        Err(_) => return Ok(Flow::Continue),
                    };
                turn.start_cancel_grace(deadline);
                if let Err(error) = self.send_cancel(scope.clone()).await {
                    // A stale/refused Cancel must not drop the admitted prompt
                    // future or leave its original SDK slot unsettled.
                    warn!(target: "acp.protocol", %error, "cancel not issued");
                    return Ok(Flow::Continue);
                }
                if current_effect_wait(scope.as_ref(), native, &self.shared.connection_state, || ())
                    .await
                    .is_err()
                    || current_effect_wait(
                        turn.scope.as_ref(),
                        native,
                        &self.shared.connection_state,
                        || (),
                    )
                    .await
                    .is_err()
                {
                    return Ok(Flow::Continue);
                }
                if newly_armed {
                    let escalates_at = crate::time::Timestamp::now()
                        + crate::time::Duration::try_from(
                            deadline.saturating_duration_since(Instant::now()),
                        )
                        .unwrap_or_default();
                    let sender = scoped_sender(&self.shared, native, &scope);
                    publish_scoped(
                        &self.shared,
                        native,
                        &scope,
                        &sender,
                        Event::CancelRequested { escalates_at },
                        true,
                    )
                    .await;
                }
            }
            ClientCmd::ForceStop => {
                if require_operation(&scope, correlation, native).is_err()
                    || current_effect_wait(
                        scope.as_ref(),
                        native,
                        &self.shared.connection_state,
                        || (),
                    )
                    .await
                    .is_err()
                    || current_effect_wait(
                        turn.scope.as_ref(),
                        native,
                        &self.shared.connection_state,
                        || (),
                    )
                    .await
                    .is_err()
                {
                    return Ok(Flow::Continue);
                }
                if let Err(error) = self.send_cancel(scope).await {
                    debug!(target: "acp.protocol", %error, "cancel notification failed");
                }
                turn.flags.force_stopped = true;
                turn.flags.shutdown = true;
                return Ok(Flow::Break);
            }
            ClientCmd::SetConfigOption {
                config_id,
                value,
                remember,
            } => {
                self.dispatch_config_option(config_id, value, remember, scope)
                    .await;
            }
            ClientCmd::SetMode(mode_id) => self.dispatch_mode(mode_id, true, scope),
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
            ClientCmd::Prompt(input) => {
                return Ok(self
                    .on_follow_up(
                        turn,
                        QueuedPrompt {
                            input,
                            scope,
                            correlation,
                        },
                    )
                    .await);
            }
            ClientCmd::InitialPromptIntent(_) => {
                unreachable!("host intent resolved before Native dispatch");
            }
            ClientCmd::ResetSession {
                text, respond_to, ..
            } => {
                self.reject_busy(text, Vec::new(), &scope).await;
                let _ = respond_to.send(ResetSessionOutcome::Failed {
                    message: "a turn is in flight; stop it before clearing the conversation".into(),
                });
            }
            #[cfg(test)]
            ClientCmd::FlushForTest(done) => {
                let _ = done.send(());
            }
            ClientCmd::Shutdown => {
                turn.flags.shutdown = true;
                return Ok(Flow::Break);
            }
        }
        Ok(Flow::Continue)
    }

    async fn on_follow_up(&mut self, turn: &mut Turn, prompt: QueuedPrompt) -> Flow {
        let native = self.control.is_some();
        let admission = match require_operation(&prompt.scope, prompt.correlation, native) {
            Ok(()) => {
                current_effect_wait(
                    prompt.scope.as_ref(),
                    native,
                    &self.shared.connection_state,
                    || (),
                )
                .await
            }
            Err(error) => Err(error),
        };
        if let Err(error) = admission {
            self.reject_prompt_not_issued(error.to_string(), &prompt.scope)
                .await;
            return Flow::Continue;
        }

        if turn.flags.cancelling {
            self.reject_busy(prompt.input.text(), prompt.input.attachments, &prompt.scope)
                .await;
            if current_effect(&turn.scope, native, || ()).is_ok() {
                turn.flags.agent_unresponsive = true;
                turn.flags.shutdown = true;
                return Flow::Break;
            }
            return Flow::Continue;
        }

        let compacting = if native {
            self.shared.native_compacting()
        } else {
            turn.watchdog.off_protocol_work_seen() == Some(OffProtocolWorkKind::Compaction)
        };
        if self.steering_capable && !compacting {
            self.steer_or_backlog(turn, prompt);
        } else {
            self.reject_busy(prompt.input.text(), prompt.input.attachments, &prompt.scope)
                .await;
        }
        Flow::Continue
    }

    fn steer_or_backlog(&self, turn: &mut Turn, prompt: QueuedPrompt) {
        if turn.steer_fut.is_some() {
            turn.steer_backlog.push_back(prompt);
            return;
        }
        let request = ordered_native_request(
            &self.connection,
            &self.shared.connection_state,
            prompt.scope.clone(),
            SteerRequest::new(self.acp_session_id.clone(), prompt.input.blocks.clone()),
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
        let next = Instant::now() + turn.orphan_check_period;
        turn.orphan_check.as_mut().reset(next);

        if current_effect(&turn.scope, self.control.is_some(), || ()).is_err() {
            // Stale business permission cannot authorize watchdog cancellation or
            // restart. Keep waiting for the original admitted completion.
            return Flow::Continue;
        }

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
                session = %self.shared.session_label,
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
        if !fire {
            return Flow::Continue;
        }

        turn.flags.prompt_orphaned = true;
        let Ok((deadline, _)) = self.request_cancel(&turn.scope, turn.terminal_epoch).await else {
            turn.flags.prompt_orphaned = false;
            return Flow::Continue;
        };
        turn.start_cancel_grace(deadline);
        if let Err(error) = self.send_cancel(turn.scope.clone()).await {
            // Permission may have expired during the await. That is not evidence
            // that the native agent ignored a cancellation.
            if current_effect(&turn.scope, self.control.is_some(), || ()).is_err() {
                turn.flags.prompt_orphaned = false;
                return Flow::Continue;
            }
            warn!(target: "acp.protocol", %error, "silent-orphan cancel failed");
            turn.flags.shutdown = true;
            return Flow::Break;
        }

        turn.flags.orphan_cancel_sent = true;
        Flow::Continue
    }

    async fn on_steer_result(
        &mut self,
        turn: &mut Turn,
        prompt: QueuedPrompt,
        result: OrderedRequestResult<serde_json::Value>,
    ) {
        turn.steer_fut = None;
        let result = match result {
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

        match result.as_ref().map(SteerOutcome::from_response) {
            Ok(SteerOutcome::Injected) => {
                if self.control.is_none()
                    && current_effect(&prompt.scope, false, || ()).is_ok()
                    && current_effect(&turn.scope, false, || ()).is_ok()
                {
                    turn.apply(LifecycleSignal::Progress);
                }
            }
            Ok(SteerOutcome::PromptRequired) => {
                // Preserve the entire original envelope, including its UUID and
                // nonce. Running this fallback never captures a replacement scope.
                self.pending_prompts.push_back(prompt);
                self.pending_prompts.extend(turn.steer_backlog.drain(..));
            }
            Ok(outcome @ (SteerOutcome::StartedNewTurn | SteerOutcome::Unknown)) => {
                warn!(
                    target: "acp.protocol",
                    ?outcome,
                    "_session/steering consumed the message without an owning request"
                );
            }
            Err(error) => {
                warn!(target: "acp.protocol", %error, "_session/steering failed");
                self.reject_busy(prompt.input.text(), prompt.input.attachments, &prompt.scope)
                    .await;
            }
        }

        if let Some(next) = turn.steer_backlog.pop_front() {
            self.steer_or_backlog(turn, next);
        }
    }

    async fn finish_turn(&mut self, mut turn: Turn, prompt_started_at_ms: i64) -> bool {
        let shared = self.shared.clone();
        let native = self.control.is_some();
        if !turn.flags.result_observed && shared.connection_state.loss().is_some() {
            return false;
        }

        while let Ok(envelope) = self.lifecycle_rx.try_recv() {
            if !native && envelope.epoch == turn.epoch {
                turn.apply(envelope.signal);
            }
        }

        let finished = !native
            && turn.watchdog.cost_seen()
            && turn.watchdog.off_protocol_work_seen().is_none();

        if !native
            && !turn.flags.prompt_failed
            && !turn.flags.rejected
            && shared.profile.key == "opencode"
            && finished
            && !turn.watchdog.saw_progress()
            && turn.result_current
            && current_effect(&turn.scope, native, || ()).is_ok()
        {
            if let Some(message) =
                recover_opencode_prompt_error(&self.acp_session_id.0, prompt_started_at_ms)
            {
                turn.preceding.push(Event::PromptRuntimeError {
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

        let rate_limited = turn.flags.rate_limit.is_some();
        let flags = &turn.flags;
        let reason = if flags.rejected {
            "prompt_rejected"
        } else if flags.prompt_failed {
            "prompt_failed"
        } else {
            terminal_stop_reason(
                rate_limited,
                flags.force_stopped,
                flags.prompt_orphaned,
                flags.agent_unresponsive,
                flags.shutdown,
                flags.prompt_cancelled,
                flags.orphan_cancel_sent && finished,
            )
        };
        let failed = flags.prompt_failed || flags.rejected || rate_limited;

        // Original local slot settlement is independent of Native row generation.
        // A later SDK terminal epoch still prevents settling somebody else's slot.
        let claimed = shared
            .terminal_claim
            .claim_epoch(turn.terminal_epoch, failed);

        if claimed {
            if let Some(info) = turn.flags.rate_limit.take() {
                turn.preceding.push(Event::RateLimit { info });
            }
            if turn.result_current {
                let _ = current_effect(&turn.scope, native, || {
                    shared
                        .terminal_claim
                        .with_current_epoch(turn.terminal_epoch, || {
                            if failed {
                                shared.between_prompt.deactivate();
                            }
                            shared.reset_message_dedup();
                            shared
                                .last_prompt
                                .lock()
                                .expect("last prompt mutex poisoned")
                                .1 = now_ms();
                        });
                });
            }

            if !shared
                .publish_terminal(
                    turn.terminal_epoch,
                    turn.sender,
                    turn.scope.clone(),
                    reason.into(),
                    std::mem::take(&mut turn.preceding),
                    turn.flags.result_observed,
                )
                .await
            {
                return false;
            }
        }
        if claimed
            && !native
            && turn.result_current
            && current_effect(&turn.scope, native, || ()).is_ok()
            && shared.subagent_working(now_ms())
        {
            shared.arm_for_subagent_activity().await;
        }
        info!(
            target: "acp.protocol",
            session = %shared.session_label,
            reason,
            agent_stop_reason = ?turn.flags.agent_stop_reason,
            duration_ms = now_ms() - prompt_started_at_ms,
            "turn finished; original local prompt slot settled"
        );

        turn.flags.shutdown && current_effect(&turn.scope, native, || ()).is_ok()
    }
}
