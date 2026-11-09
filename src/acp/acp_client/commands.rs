//! The command channel the connection task pumps, and how each connection
//! attempt reaches its agent.

use agent_client_protocol::v1::schema::ContentBlock;
use tokio::sync::oneshot;
use tokio::time::Duration;

const QUEUED_PROMPT_PUBLICATION_TIMEOUT: Duration = Duration::from_secs(30);

use super::delete::DeleteSessionOutcome;
use super::reset::ResetSessionOutcome;

/// A worker reservation. Dropping it cancels delivery; committing it follows
/// the durable sent event, so agent output cannot overtake that event.
#[derive(Debug)]
pub(crate) struct QueuedPromptPermit(pub(crate) oneshot::Sender<()>);

impl QueuedPromptPermit {
    pub(crate) fn commit(self) -> Result<(), super::AcpError> {
        self.0.send(()).map_err(|_| {
            super::AcpError::Protocol("queued prompt reservation expired or was cancelled".into())
        })
    }

    pub(crate) async fn publish<T>(
        &mut self,
        publication: impl std::future::Future<Output = T>,
    ) -> Result<T, super::AcpError> {
        // The worker must not expire a reservation during a synchronous durable write.
        tokio::select! {
            biased;
            _ = self.0.closed() => Err(super::AcpError::Protocol(
                "queued prompt publication was cancelled".into(),
            )),
            result = tokio::time::timeout(QUEUED_PROMPT_PUBLICATION_TIMEOUT, publication) => {
                result.map_err(|_| super::AcpError::Protocol(
                    "queued prompt publication timed out".into(),
                ))
            }
        }
    }
}

pub(super) struct QueuedPromptReservation {
    pub(super) blocks: Vec<ContentBlock>,
    commit: oneshot::Receiver<()>,
    pub(super) committed: bool,
}

impl QueuedPromptReservation {
    pub(super) fn new(
        blocks: Vec<ContentBlock>,
        ready: oneshot::Sender<Option<QueuedPromptPermit>>,
    ) -> Option<Self> {
        let (publish, commit) = oneshot::channel();
        ready.send(Some(QueuedPromptPermit(publish))).ok()?;
        Some(Self {
            blocks,
            commit,
            committed: false,
        })
    }

    pub(super) async fn wait_for_commit(&mut self) -> bool {
        (&mut self.commit).await.is_ok()
    }
}

#[cfg(test)]
pub(super) async fn reserve_queued_prompt(
    ready: oneshot::Sender<Option<QueuedPromptPermit>>,
) -> bool {
    let Some(mut reservation) = QueuedPromptReservation::new(Vec::new(), ready) else {
        return false;
    };
    reservation.wait_for_commit().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn queued_publication_cannot_expire_during_a_durable_write() {
        let (ready, reserved) = oneshot::channel();
        let mut reservation = QueuedPromptReservation::new(Vec::new(), ready).unwrap();
        let mut permit = reserved.await.unwrap().unwrap();
        let (probe, observed_probe) = oneshot::channel();
        let (polled, observed_poll) = oneshot::channel();
        let worker = tokio::spawn(async move {
            tokio::select! {
                biased;
                committed = reservation.wait_for_commit() => return committed,
                _ = observed_probe => { let _ = polled.send(()); }
            }
            reservation.wait_for_commit().await
        });
        let (entered, entered_write) = oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let runtime = tokio::runtime::Handle::current();
        let publisher = tokio::task::spawn_blocking(move || {
            runtime.block_on(async move {
                permit
                    .publish(async {
                        let _ = entered.send(());
                        released.recv().expect("release synchronous publication");
                    })
                    .await?;
                permit.commit()
            })
        });
        entered_write.await.unwrap();
        tokio::time::advance(Duration::from_secs(60)).await;
        probe.send(()).unwrap();
        let worker_retained = observed_poll.await;
        release.send(()).unwrap();
        let published = publisher.await.unwrap();
        worker_retained.expect("the worker must retain the publishing reservation");
        published.unwrap();
        assert!(worker.await.unwrap());
    }
}

/// Commands sent from `AcpClient` methods to the background connection task.
pub(super) enum ClientCmd {
    /// The fully-built prompt content blocks (text first, then any
    /// attachments). Built in `send_prompt` so the connection task just
    /// forwards them to `session/prompt`. See #1000.
    Prompt(Vec<ContentBlock>),
    /// A durable queue keeps its rows until this task admits the content.
    QueuedPrompt {
        blocks: Vec<ContentBlock>,
        ready: oneshot::Sender<Option<QueuedPromptPermit>>,
    },
    Cancel,
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
    },
}
