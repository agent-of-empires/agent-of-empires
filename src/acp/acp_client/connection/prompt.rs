//! One in-flight prompt: driving `session/prompt` concurrently with commands,
//! lifecycle signals, the silent-orphan and cancel-escalation watchdogs, and
//! mid-turn steering.

use crate::acp::control_protocol::PromptOutcome;
use crate::acp::state::Event;
use agent_client_protocol::schema::v1::{ContentBlock, PromptRequest, PromptResponse, StopReason};
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
use crate::acp::acp_client::commands::ClientCmd;
use crate::acp::acp_client::control::prompt_outcome_to_response;
use crate::acp::acp_client::delete::handle_delete_session_cmd;
use crate::acp::acp_client::errors::acp_internal_error;
use crate::acp::acp_client::lifecycle::{LifecycleSignal, OffProtocolWorkKind};
use crate::acp::acp_client::opencode::recover_opencode_prompt_error;
use crate::acp::acp_client::rate_limit::{
    captured_rate_limit_resets_at, classify_rate_limit_error, is_unsupported_session_error,
};
use crate::acp::acp_client::reset::ResetSessionOutcome;
use crate::acp::acp_client::steer::{first_text_block, SteerOutcome, SteerRequest};
use crate::acp::acp_client::watchdog::{
    silent_orphan_check_interval, silent_orphan_fast_grace, silent_orphan_grace,
    terminal_stop_reason, SilentOrphanWatchdog, SilentOrphanWatchdogConfig,
    OFF_PROTOCOL_WORK_GRACE_FLOOR,
};

type AcpResult<T> = Result<T, agent_client_protocol::Error>;
type PromptFut = Pin<Box<dyn Future<Output = AcpResult<PromptResponse>> + Send>>;
type SteerFut =
    Pin<Box<dyn Future<Output = (Vec<ContentBlock>, AcpResult<serde_json::Value>)> + Send>>;

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
    rate_limited: bool,
    force_stopped: bool,
    prompt_orphaned: bool,
    agent_unresponsive: bool,
    /// The adapter resolved with `StopReason::Cancelled`.
    prompt_cancelled: bool,
    orphan_cancel_sent: bool,
    cancelling: bool,
}

struct Turn {
    epoch: u64,
    flags: TurnFlags,
    watchdog: SilentOrphanWatchdog,
    cfg: SilentOrphanWatchdogConfig,
    orphan_check_enabled: bool,
    orphan_check_period: Duration,
    orphan_check: Pin<Box<Sleep>>,
    cancel_grace: Pin<Box<Sleep>>,
    /// At most one steer is outstanding so racing steers cannot reorder the
    /// user's messages; later ones wait in the backlog.
    steer_fut: Option<SteerFut>,
    steer_backlog: VecDeque<Vec<ContentBlock>>,
}

impl Turn {
    fn apply(&mut self, signal: LifecycleSignal) {
        self.watchdog
            .apply_signal(signal, Instant::now(), chrono::Utc::now(), self.cfg);
    }

    fn start_cancel_grace(&mut self) {
        self.flags.cancelling = true;
        self.cancel_grace
            .as_mut()
            .reset(Instant::now() + CANCEL_ESCALATION_GRACE);
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
    /// Runs one turn and always emits its terminal `Stopped`. Returns whether
    /// the connection should shut down.
    pub(super) async fn run_prompt(&mut self, blocks: Vec<ContentBlock>) -> AcpResult<bool> {
        let shared = self.shared.clone();
        if let Some(control) = self.control.as_ref() {
            control.supersede_adopted_turn();
        }
        shared.reset_message_dedup();
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
        shared.prompt_in_flight.store(true, Ordering::Relaxed);
        shared.terminal_claim.begin_turn();
        shared.between_prompt.reset_for_prompt();
        info!(target: "acp.protocol", "sending prompt ({} content blocks)", blocks.len());
        // Bump the epoch before sending so envelopes from a handler parked on
        // the previous prompt are discarded.
        let epoch = shared.prompt_epoch.fetch_add(1, Ordering::Relaxed) + 1;
        while self.lifecycle_rx.try_recv().is_ok() {}

        let base_grace = silent_orphan_grace(self.source_profile.as_deref());
        let orphan_check_period = silent_orphan_check_interval();
        let mut turn = Turn {
            epoch,
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
                cmd = self.cmd_rx.recv() => {
                    #[cfg(test)]
                    observe_select_win(true);
                    self.on_command(&mut turn, cmd).await?
                }
                _ = turn.cancel_grace.as_mut(), if turn.flags.cancelling => {
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
                        Some(env) if env.epoch == turn.epoch => turn.apply(env.signal),
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
                    if turn.orphan_check_enabled && !turn.flags.orphan_cancel_sent =>
                {
                    self.on_orphan_check(&mut turn).await
                }
                (blocks, res) = async {
                    turn.steer_fut.as_mut().expect("guarded by the arm condition").await
                }, if turn.steer_fut.is_some() => {
                    self.on_steer_result(&mut turn, blocks, res).await;
                    Flow::Continue
                }
            };
            if let Flow::Break = flow {
                break;
            }
        }
        let shutdown = turn.flags.shutdown;
        self.finish_turn(turn, prompt_started_at_ms).await;
        Ok(shutdown)
    }

    async fn send_prompt(&self, blocks: Vec<ContentBlock>) -> PromptFut {
        let request = PromptRequest::new(self.acp_session_id.clone(), blocks);
        let Some(control) = self.control.as_ref() else {
            return Box::pin(self.connection.send_request(request).block_task());
        };
        match serde_json::to_value(request) {
            Ok(params) => {
                let rx = control.prompt(params).await;
                Box::pin(async move {
                    // A closed channel ends the turn; the dying connection
                    // surfaces the underlying failure.
                    prompt_outcome_to_response(rx.await.unwrap_or(PromptOutcome::Aborted))
                })
            }
            Err(e) => {
                Box::pin(
                    async move { Err(acp_internal_error(format!("serialize prompt params: {e}"))) },
                )
            }
        }
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
        res: AcpResult<PromptResponse>,
    ) -> AcpResult<Flow> {
        let e = match res {
            Ok(resp) => {
                turn.flags.prompt_cancelled = matches!(resp.stop_reason, StopReason::Cancelled);
                return Ok(Flow::Break);
            }
            Err(e) => e,
        };
        let label = &self.shared.session_label;
        // A rate limit parks the session rather than burning restart budget
        // on a worker that would hit it again (#1281).
        let resets_at =
            captured_rate_limit_resets_at(&self.shared.rate_limit_rejections, chrono::Utc::now());
        if let Some(info) = classify_rate_limit_error(&e, resets_at) {
            info!(
                target: "acp.protocol",
                session = %label,
                resets_at = ?info.resets_at,
                "session/prompt returned rate_limit; parking session"
            );
            self.shared.emit(Event::RateLimit { info }).await;
            turn.flags.rate_limited = true;
            turn.flags.shutdown = true;
            return Ok(Flow::Break);
        }
        // A resumed id the agent dropped: reset context so the respawn opens
        // a fresh session instead of terminating the runner (#3560).
        if self.session_from_storage && is_unsupported_session_error(&e) {
            warn!(
                target: "acp.protocol",
                session = %label,
                acp_session_id = %self.acp_session_id.0,
                "resumed ACP session rejected as unsupported; resetting context so the respawn starts fresh: {e}"
            );
            self.shared
                .emit(Event::SessionContextReset {
                    reason: format!("resumed session no longer available: {e}"),
                })
                .await;
            self.shared
                .context_reset_emitted
                .store(true, Ordering::Relaxed);
        }
        Err(e)
    }

    async fn reject_busy(&self, text: String) {
        self.shared
            .emit(Event::PromptRejected {
                reason: "agent_busy".into(),
                text,
            })
            .await;
    }

    async fn on_command(&mut self, turn: &mut Turn, cmd: Option<ClientCmd>) -> AcpResult<Flow> {
        match cmd {
            Some(ClientCmd::Cancel) => {
                info!(target: "acp.protocol", "sending session/cancel during in-flight prompt");
                self.send_cancel().await?;
                // Only the first cancel arms escalation and tells the UI.
                if !turn.flags.cancelling {
                    turn.start_cancel_grace();
                    let escalates_at = chrono::Utc::now()
                        + chrono::Duration::from_std(CANCEL_ESCALATION_GRACE)
                            .unwrap_or_else(|_| chrono::Duration::seconds(10));
                    self.shared
                        .emit(Event::CancelRequested { escalates_at })
                        .await;
                }
            }
            Some(ClientCmd::ForceStop) => {
                warn!(
                    target: "acp.protocol",
                    "force-stop requested during in-flight prompt; ending turn and restarting worker"
                );
                let _ = self.send_cancel().await;
                turn.flags.force_stopped = true;
                turn.flags.shutdown = true;
                return Ok(Flow::Break);
            }
            Some(ClientCmd::SetConfigOption { config_id, value }) => {
                self.dispatch_config_option(config_id, value)
            }
            Some(ClientCmd::SetMode(mode_id)) => self.dispatch_mode(mode_id, true),
            Some(ClientCmd::DeleteSession {
                acp_session_id,
                respond_to,
            }) => handle_delete_session_cmd(&self.connection, acp_session_id, respond_to),
            Some(ClientCmd::ResumeBackgroundTailing(launches)) => {
                self.shared.resume_background_tailing(launches)
            }
            Some(ClientCmd::Prompt(blocks)) => return Ok(self.on_follow_up(turn, blocks).await),
            Some(ClientCmd::ResetSession {
                text, respond_to, ..
            }) => {
                // Resetting now would orphan the pending turn on the old id.
                warn!(
                    target: "acp.protocol",
                    "conversation reset requested during in-flight prompt; refusing"
                );
                self.reject_busy(text).await;
                let _ = respond_to.send(ResetSessionOutcome::Failed {
                    message: "a turn is in flight; stop it before clearing the conversation".into(),
                });
            }
            #[cfg(test)]
            Some(ClientCmd::FlushForTest(done)) => {
                let _ = done.send(());
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

    async fn on_follow_up(&mut self, turn: &mut Turn, blocks: Vec<ContentBlock>) -> Flow {
        let label = self.shared.session_label.clone();
        // A follow-up while cancelling means the user force-ended and
        // re-typed: the agent is wedged. Reject first so nothing is lost.
        if turn.flags.cancelling {
            warn!(
                target: "acp.protocol",
                session = %label,
                "follow-up prompt arrived while cancel pending; escalating to runner restart"
            );
            self.reject_busy(first_text_block(&blocks)).await;
            turn.flags.agent_unresponsive = true;
            turn.flags.shutdown = true;
            return Flow::Break;
        }
        // A `/compact` turn would swallow a steer with no reply and no Retry
        // pill, so it is rejected instead (#3219).
        let compacting =
            turn.watchdog.off_protocol_work_seen() == Some(OffProtocolWorkKind::Compaction);
        if self.steering_capable && !compacting {
            self.steer_or_backlog(turn, blocks);
        } else {
            warn!(
                target: "acp.protocol",
                compacting,
                "received Prompt while one is in flight and it cannot be steered into; rejecting"
            );
            self.reject_busy(first_text_block(&blocks)).await;
        }
        Flow::Continue
    }

    fn steer_or_backlog(&self, turn: &mut Turn, blocks: Vec<ContentBlock>) {
        if turn.steer_fut.is_some() {
            turn.steer_backlog.push_back(blocks);
            return;
        }
        info!(
            target: "acp.protocol",
            session = %self.shared.session_label,
            "sending _session/steering during in-flight prompt ({} content blocks)",
            blocks.len()
        );
        let sent = self.connection.send_request(SteerRequest::new(
            self.acp_session_id.clone(),
            blocks.clone(),
        ));
        turn.steer_fut = Some(Box::pin(async move { (blocks, sent.block_task().await) }));
    }

    async fn on_orphan_check(&mut self, turn: &mut Turn) -> Flow {
        let label = self.shared.session_label.clone();
        let fire = turn.watchdog.should_fire(Instant::now(), turn.cfg);
        let off_protocol = turn.watchdog.off_protocol_work_seen();
        if fire && turn.watchdog.cost_seen() && off_protocol.is_none() {
            // The turn finished but the PromptResponse never came: end cleanly
            // as prompt_complete with no flag set (#2237).
            info!(
                target: "acp.protocol",
                session = %label,
                grace_secs = turn.watchdog.effective_grace(turn.cfg).as_secs(),
                "silent-orphan watchdog: turn wrapped up (cost-populated usage) without PromptResponse; ending cleanly as prompt_complete"
            );
            return Flow::Break;
        }
        if fire {
            warn!(
                target: "acp.protocol",
                session = %label,
                off_protocol_work = ?off_protocol,
                in_flight_tools = turn.watchdog.tool_calls_in_flight_len(),
                grace_secs = turn.watchdog.effective_grace(turn.cfg).as_secs(),
                "silent-orphan watchdog fired: no progress past grace and no in-flight tools; sending session/cancel"
            );
            turn.flags.prompt_orphaned = true;
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
            if !turn.flags.cancelling {
                turn.start_cancel_grace();
            }
        }
        let next = Instant::now() + turn.orphan_check_period;
        turn.orphan_check.as_mut().reset(next);
        Flow::Continue
    }

    async fn on_steer_result(
        &mut self,
        turn: &mut Turn,
        blocks: Vec<ContentBlock>,
        res: AcpResult<serde_json::Value>,
    ) {
        turn.steer_fut = None;
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
                self.pending_prompts.push_back(blocks);
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
                self.reject_busy(first_text_block(&blocks)).await;
            }
        }
        if let Some(next) = turn.steer_backlog.pop_front() {
            self.steer_or_backlog(turn, next);
        }
    }

    async fn finish_turn(&mut self, mut turn: Turn, prompt_started_at_ms: i64) {
        let shared = self.shared.clone();
        // A cost marker can land in the same tick the prompt resolves (#2370).
        while let Ok(env) = self.lifecycle_rx.try_recv() {
            if env.epoch == turn.epoch {
                turn.apply(env.signal);
            }
        }
        let w = &turn.watchdog;
        let finished = w.cost_seen() && w.off_protocol_work_seen().is_none();
        if shared.profile.key == "opencode" && finished && !w.saw_progress() {
            if let Some(message) =
                recover_opencode_prompt_error(&self.acp_session_id.0, prompt_started_at_ms)
            {
                shared.emit(Event::PromptRuntimeError { message }).await;
            }
        }
        let f = &turn.flags;
        let reason = terminal_stop_reason(
            f.rate_limited,
            f.force_stopped,
            f.prompt_orphaned,
            f.agent_unresponsive,
            f.shutdown,
            f.prompt_cancelled,
            f.orphan_cancel_sent && finished,
        );
        // A completion the runner reports after this is dropped.
        shared.terminal_claim.claim();
        shared
            .emit(Event::Stopped {
                reason: reason.into(),
            })
            .await;
        shared.reset_message_dedup();
        shared.prompt_in_flight.store(false, Ordering::Relaxed);
        // Missing after a Stopped, this line signals a stranded prompt (#3190).
        debug!(
            target: "acp.protocol",
            session = %shared.session_label,
            reason,
            "prompt drain complete; between-prompt idle ownership restored"
        );
    }
}
