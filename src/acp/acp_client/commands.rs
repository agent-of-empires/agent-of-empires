//! The command channel the connection task pumps, and how each connection
//! attempt reaches its agent.

use agent_client_protocol::v1::schema::ContentBlock;
use tokio::sync::oneshot;

use crate::daemon::PromptAttachmentRef;

use super::delete::DeleteSessionOutcome;
use super::reset::ResetSessionOutcome;

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
    Cancel,
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
        /// Absolute deadline created by the caller. Starting it before the
        /// command is queued prevents a delayed command from resetting the
        /// session after the caller has already timed out.
        deadline: tokio::time::Instant,
        respond_to: oneshot::Sender<ResetSessionOutcome>,
    },
    /// Spawn a tailer for each `(agent_id, output_file)` pair: sub-agents
    /// launched by a previous daemon that survived the restart. Issued by
    /// `Supervisor::attach` once, right after the connection is up, so
    /// tracking resumes instead of the panel showing them as detached.
    ResumeBackgroundTailing(Vec<(String, String)>),
    /// Re-tail the exact background task launches a previous daemon left open.
    ResumeTaskTailing(Vec<crate::acp::state::BackgroundTaskLaunch>),
    #[cfg(test)]
    FlushForTest(oneshot::Sender<()>),
    CloseSession(oneshot::Sender<super::close::CloseSessionOutcome>),
    Shutdown,
}

/// Handshake mode. `Fresh` loads or creates a session. `Resume` attaches to a
/// runner that survived the daemon, reusing its session id so an in-flight turn
/// and its context are not split onto a new session.
#[derive(Debug, Clone)]
pub(super) enum ConnectMode {
    Fresh {
        stored_acp_session_id: Option<String>,
        /// Seed the event store from the `session/load` history replay
        /// instead of suppressing it (imported session, empty store). See
        /// #2276.
        seed_history_replay: bool,
        /// Parent ACP session id to fork from. When set and the agent
        /// advertises the fork capability, the handshake sends
        /// `session/fork` instead of `session/new` / `session/load`.
        fork_from: Option<String>,
        /// A conversation existed before this launch, even without a stored id.
        prior_conversation: bool,
    },
    Resume {
        acp_session_id: String,
        in_flight_turn: bool,
        /// Confirmation deadline for recovery of the same verified runner.
        /// Once confirmed, replay and silence belong to the runner's live turn.
        live_recovery: Option<tokio::time::Instant>,
    },
}
