//! The command channel the connection task pumps, and how each connection
//! attempt reaches its agent.

use agent_client_protocol::v1::schema::ContentBlock;
use tokio::sync::oneshot;

use crate::daemon::PromptAttachmentRef;

use super::delete::DeleteSessionOutcome;
use super::reset::ResetSessionOutcome;

/// The actual command lane retains admission across every queue wait.
pub(super) struct QueuedClientCmd {
    pub(super) command: ClientCmd,
    pub(super) scope: Option<crate::acp::establishment::NativeRequestContext>,
    pub(super) correlation: Option<crate::acp::establishment::NativeOperationCorrelation>,
}

/// Steering fallback retains the same operation, not a freshly admitted turn.
pub(super) struct QueuedPrompt {
    pub(super) input: PromptInput,
    pub(super) scope: Option<crate::acp::establishment::NativeRequestContext>,
    pub(super) correlation: Option<crate::acp::establishment::NativeOperationCorrelation>,
}

pub(super) struct InitialPromptState {
    pub(super) id: uuid::Uuid,
    pub(super) cancellation: tokio_util::sync::CancellationToken,
    pub(super) seq: Option<u64>,
    pub(super) correlation: Option<crate::acp::establishment::NativeOperationCorrelation>,
    pub(super) issued: bool,
}

/// Host intent only. No Native command or permission exists before completion.
pub(super) struct InitialPromptIntent {
    pub(super) id: uuid::Uuid,
    pub(super) client: std::sync::Weak<super::AcpClient>,
    pub(super) expected: std::sync::Arc<crate::acp::establishment::EstablishmentAttempt>,
    pub(super) owner: crate::acp::runner_lifecycle::RunnerIdentity,
    pub(super) cancellation: tokio_util::sync::CancellationToken,
    pub(super) input: PromptInput,
}

pub(crate) enum InitialPromptCancellation {
    Missing,
    BeforePublication,
    Pending { seq: u64 },
    ConsumerWillSettle,
    Issued,
}

/// Reserves the existing bounded actor lane before the host row is published.
pub(crate) struct PreparedInitialPrompt {
    pub(super) client: std::sync::Arc<super::AcpClient>,
    pub(super) id: uuid::Uuid,
    pub(super) permit: Option<tokio::sync::mpsc::OwnedPermit<QueuedClientCmd>>,
    pub(super) queued: Option<QueuedClientCmd>,
}

impl PreparedInitialPrompt {
    pub(crate) fn commit(mut self, seq: u64) -> Result<(), super::errors::AcpError> {
        let queued = self.queued.take().expect("reserved initial intent");
        let permit = self.permit.take().expect("reserved actor capacity");
        let result = self.client.connection_state.admit(|| {
            let mut slot = self.client.connection_state.initial_prompt.lock().unwrap();
            let pending = slot
                .as_mut()
                .filter(|pending| pending.id == self.id)
                .ok_or(super::errors::AcpError::NotRunning)?;
            if pending.cancellation.is_cancelled() {
                return Err(super::errors::AcpError::NotRunning);
            }
            pending.seq = Some(seq);
            drop(permit.send(queued));
            Ok(())
        });
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(error)) | Err(error) => {
                self.client.clear_initial_prompt(self.id);
                Err(error)
            }
        }
    }
}

impl Drop for PreparedInitialPrompt {
    fn drop(&mut self) {
        if self.queued.is_some() {
            let mut slot = self.client.connection_state.initial_prompt.lock().unwrap();
            if slot.as_ref().is_some_and(|pending| pending.id == self.id) {
                if let Some(pending) = slot.take() {
                    pending.cancellation.cancel();
                }
            }
        }
    }
}

impl InitialPromptIntent {
    pub(super) async fn prepare_native(
        &mut self,
    ) -> Result<QueuedClientCmd, super::errors::AcpError> {
        use crate::acp::establishment::{
            EstablishmentAttempt, EstablishmentRefusal, NativeOperationCorrelation,
        };
        let Some(client) = self.client.upgrade() else {
            return Err(super::errors::AcpError::NotRunning);
        };
        let result = async {
            tokio::select! {
                biased;
                _ = self.cancellation.cancelled() => return Err(super::errors::AcpError::NotRunning),
                ready = client.connection_state.wait_until_usable() => ready?,
            }
            client.ensure_established()?;
            let projection = client
                .establishment_projection()
                .ok_or(EstablishmentRefusal::Unconfirmed)?;
            let receipt = projection
                .receipt_snapshot()
                .ok_or(EstablishmentRefusal::Unconfirmed)?;
            if client.connected_runner != Some(self.owner)
                || !EstablishmentAttempt::eq(&receipt.attempt, &self.expected)
            {
                return Err(EstablishmentRefusal::Superseded.into());
            }
            let correlation = NativeOperationCorrelation::new(Some(self.id));
            let scope = tokio::select! {
                biased;
                _ = self.cancellation.cancelled() => return Err(super::errors::AcpError::NotRunning),
                scope = client.capture_operation_context(
                    correlation,
                    tokio::time::Instant::now() + crate::acp::supervisor::WORKER_READY_TIMEOUT,
                    Some(&self.expected),
                ) => scope?,
            }
            .ok_or(EstablishmentRefusal::Unconfirmed)?;
            if self.cancellation.is_cancelled()
                || !projection.receipt_snapshot().as_ref().is_some_and(|current| {
                    std::sync::Arc::ptr_eq(current, &receipt)
                })
            {
                return Err(EstablishmentRefusal::Superseded.into());
            }
            let mut slot = client.connection_state.initial_prompt.lock().unwrap();
            let pending = slot
                .as_mut()
                .filter(|pending| pending.id == self.id)
                .ok_or(super::errors::AcpError::NotRunning)?;
            if pending.cancellation.is_cancelled() {
                return Err(super::errors::AcpError::NotRunning);
            }
            pending.correlation = Some(correlation);
            Ok((scope, correlation))
        }
        .await;
        match result {
            Ok((scope, correlation)) => Ok(QueuedClientCmd {
                command: ClientCmd::Prompt(std::mem::take(&mut self.input)),
                scope: Some(scope),
                correlation: Some(correlation),
            }),
            Err(error) => {
                if !self.cancellation.is_cancelled() {
                    client.clear_initial_prompt(self.id);
                }
                Err(error)
            }
        }
    }
}

/// A user prompt as `send_prompt` built it (#1000): the blocks the agent
/// receives, and refs to the stored attachments among them, which a refusal
/// hands back so Retry resends the same payload.
#[derive(Debug, Clone, Default)]
pub(super) struct PromptInput {
    pub(super) blocks: Vec<ContentBlock>,
    pub(super) attachments: Vec<PromptAttachmentRef>,
}

impl PromptInput {
    /// The text the retry pill shows and resends.
    pub(super) fn text(&self) -> String {
        super::steer::first_text_block(&self.blocks)
    }
}

#[cfg(test)]
impl From<Vec<ContentBlock>> for PromptInput {
    fn from(blocks: Vec<ContentBlock>) -> Self {
        Self {
            blocks,
            attachments: Vec::new(),
        }
    }
}

/// Commands sent from `AcpClient` methods to the background connection task.
pub(super) enum ClientCmd {
    Prompt(PromptInput),
    InitialPromptIntent(InitialPromptIntent),
    Cancel,
    CancelInitialPrompt {
        id: uuid::Uuid,
        expected: crate::acp::establishment::NativeOperationCorrelation,
    },
    /// `session/cancel` for one subagent session. Answers whether it was sent:
    /// false when the child is unknown or did not advertise `cancel`.
    CancelChild {
        session_id: String,
        respond_to: oneshot::Sender<bool>,
    },
    /// Force-stop now: end the in-flight turn with `user_forced` and let
    /// the drain task kill the worker process group + respawn. See #1727.
    ForceStop,
    SetMode(String),
    /// Send `session/set_config_option` for the given (`config_id`,
    /// `value`) pair. The connection task fires the request detached so
    /// the cmd_rx loop keeps polling for Cancel during the round-trip.
    /// See #1403.
    SetConfigOption {
        config_id: String,
        value: String,
        /// A `model_config` pick that a conversation reset must re-send.
        remember: bool,
    },
    /// Send the experimental `session/delete` RPC for the given ACP
    /// session id and report the outcome via `respond_to`. Issued by
    /// the supervisor before the existing shutdown path during structured view
    /// session deletion. See #1404.
    DeleteSession {
        acp_session_id: String,
        respond_to: oneshot::Sender<DeleteSessionOutcome>,
    },
    /// Drive a real conversation reset: issue a fresh `session/new` on
    /// the live connection, swap the task's ACP session id to the new
    /// one, and emit `SessionCleared` + `SessionContextReset` +
    /// `AcpSessionAssigned` + a terminal `Stopped` so bookkeeping and
    /// the UI follow. Issued by the supervisor when a clear command hits
    /// a profile whose adapter cannot give AoE a durable post-reset id
    /// (codex `/new` has no native reset; claude `/clear` has one but
    /// withholds the new conversation id). `text`
    /// carries the user's original clear invocation, used only by the
    /// mid-turn refusal's `PromptRejected` so the retry pill shows what
    /// was typed. See #2979.
    ResetSession {
        text: String,
        proposal: std::sync::Arc<crate::acp::supervisor::LiveFreshProposal>,
        /// Absolute deadline created by the caller. Starting it before the
        /// command is queued prevents a delayed command from resetting the
        /// session after the caller has already timed out.
        deadline: tokio::time::Instant,
        respond_to: oneshot::Sender<ResetSessionOutcome>,
    },
    /// Restore tracking with its original persisted allocation evidence.
    ResumeBackgroundTailing(Vec<crate::acp::event_store::UnresolvedBackgroundAgentLaunch>),
    /// Re-tail the exact background task launches a previous daemon left open.
    ResumeTaskTailing(Vec<crate::acp::state::BackgroundTaskLaunch>),
    #[cfg(test)]
    FlushForTest(oneshot::Sender<()>),
    CloseSession(oneshot::Sender<super::close::CloseSessionOutcome>),
    Shutdown,
}

impl ClientCmd {
    pub(super) fn requires_operation(&self) -> bool {
        match self {
            Self::Shutdown
            | Self::InitialPromptIntent(_)
            | Self::ResumeBackgroundTailing(_)
            | Self::ResumeTaskTailing(_) => false,
            #[cfg(test)]
            Self::FlushForTest(_) => false,
            _ => true,
        }
    }
}

/// Transport mode; the original lifecycle attempt owns conversation selection.
#[derive(Debug, Clone)]
pub(super) enum ConnectMode {
    Fresh,
    Resume {
        acp_session_id: String,
        in_flight_turn: bool,
        unsuccessful_main: bool,
        /// Confirmation deadline for recovery of the same verified runner.
        /// Once confirmed, replay and silence belong to the runner's live turn.
        live_recovery: Option<tokio::time::Instant>,
    },
}
