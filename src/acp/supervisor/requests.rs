//! Request-path forwarders from the API to a session's live client.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::info;

use super::launch::{apply_mode, set_spawn_model};
use super::{
    lock_recover, BroadcastSink, Supervisor, SupervisorError, WorkerKind, WORKER_READY_TIMEOUT,
};
use crate::acp::acp_client::{AcpClient, AcpError, ResetSessionOutcome};
use crate::acp::approvals::{ApprovalDecision, Nonce};
use crate::acp::elicitations::ElicitationResolution;
use crate::acp::event_store::AttachmentBlob;
use crate::acp::runner_lifecycle::WorkerPhase;
use crate::acp::state::Event;

impl<S: BroadcastSink> Supervisor<S> {
    /// Wait until the session's worker is installed; false once no resume is
    /// pending or `deadline` elapses.
    pub(super) async fn wait_for_worker(&self, session_id: &str, deadline: Duration) -> bool {
        let started = Instant::now();
        loop {
            let notified = self.worker_notify.notified();
            tokio::pin!(notified);
            if self.workers.lock().await.contains_key(session_id) {
                return true;
            }
            if lock_recover(&self.lifecycle).phase(session_id) != WorkerPhase::Resuming {
                return false;
            }
            let remaining = deadline.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return false;
            }
            #[cfg(test)]
            let notified = {
                let mut reported = false;
                std::future::poll_fn(move |cx| {
                    let result = std::future::Future::poll(notified.as_mut(), cx);
                    if result.is_pending() && !reported {
                        reported = true;
                        let _ = self.worker_waits.send(session_id.to_owned());
                    }
                    result
                })
            };
            tokio::pin!(notified);
            if tokio::time::timeout(remaining, &mut notified)
                .await
                .is_err()
            {
                return false;
            }
        }
    }

    pub(super) async fn client_for_session(
        &self,
        session_id: &str,
    ) -> Result<Arc<AcpClient>, SupervisorError> {
        self.workers
            .lock()
            .await
            .get(session_id)
            .map(|h| Arc::clone(&h.client))
            .ok_or_else(|| SupervisorError::UnknownSession(session_id.into()))
    }

    /// The session's client, after waiting out a mid-resume worker.
    async fn ready_client(&self, session_id: &str) -> Result<Arc<AcpClient>, SupervisorError> {
        self.wait_for_worker(session_id, WORKER_READY_TIMEOUT).await;
        self.client_for_session(session_id).await
    }

    /// Await worker readiness so a caller can gate a durable side effect on it.
    pub async fn wait_until_ready(&self, session_id: &str) -> Result<(), SupervisorError> {
        self.ready_client(session_id).await.map(|_| ())
    }

    pub async fn send_prompt(
        &self,
        session_id: &str,
        text: &str,
        attachments: &[AttachmentBlob],
    ) -> Result<(), SupervisorError> {
        let client = self.ready_client(session_id).await?;
        client.send_prompt(text, attachments).await?;
        Ok(())
    }

    /// Drive a conversation reset (a fresh `session/new`) for a clear command,
    /// then re-assert the session's mode.
    pub async fn reset_session_context(
        &self,
        session_id: &str,
        text: &str,
        acp_mode_id: Option<&str>,
        yolo_mode: bool,
    ) -> Result<(), SupervisorError> {
        let client = self.ready_client(session_id).await?;
        match client.reset_session(text).await? {
            ResetSessionOutcome::Reset { new_acp_session_id } => {
                info!(
                    target: "acp.supervisor",
                    session = %session_id,
                    new_acp_session_id = %new_acp_session_id,
                    "conversation reset: fresh session/new swapped the ACP session id"
                );
            }
            ResetSessionOutcome::Failed { message } => {
                return Err(SupervisorError::Acp(AcpError::ResetFailed(message)));
            }
        }
        // An explicit persisted mode wins over the yolo bool, as on spawn.
        let mode_id = match (acp_mode_id, yolo_mode) {
            (Some(id), _) => Some(id.to_string()),
            (None, true) => {
                let agent_key = self.agent_key_for_session(session_id).await;
                crate::acp::agent_profiles::resolve(&agent_key)
                    .yolo_mode_id
                    .map(str::to_string)
            }
            (None, false) => None,
        };
        apply_mode(
            &client,
            session_id,
            mode_id.as_deref(),
            "conversation reset",
        )
        .await;
        Ok(())
    }

    /// Cancel the current turn; an already-exited agent has no turn to cancel.
    pub async fn cancel_prompt(&self, session_id: &str) -> Result<(), SupervisorError> {
        let client = self.ready_client(session_id).await?;
        match client.cancel_prompt().await {
            Ok(()) | Err(AcpError::AgentExited) => Ok(()),
            Err(e) => Err(SupervisorError::Acp(e)),
        }
    }

    /// User-initiated "Force stop".
    pub async fn force_end_turn(&self, session_id: &str) {
        if let Ok(client) = self.client_for_session(session_id).await {
            let _ = client.force_cancel().await;
        }
        self.publish_next(
            session_id,
            &Event::Stopped {
                reason: "user_forced".into(),
            },
        );
    }

    pub async fn set_mode(&self, session_id: &str, mode_id: &str) -> Result<(), SupervisorError> {
        let client = self.ready_client(session_id).await?;
        client.set_mode(mode_id).await?;
        Ok(())
    }

    pub async fn set_config_option(
        &self,
        session_id: &str,
        config_id: &str,
        value: &str,
    ) -> Result<(), SupervisorError> {
        let client = self.ready_client(session_id).await?;
        client.set_config_option(config_id, value).await?;
        Ok(())
    }

    /// A watchdog respawn clones the cached spawn config, which the daemon's
    /// persisted model pick never reaches.
    pub async fn refresh_cached_model(&self, session_id: &str, model: &str) {
        if let Some(WorkerKind::Runner { spawn_config }) = self
            .workers
            .lock()
            .await
            .get_mut(session_id)
            .map(|h| &mut h.kind)
        {
            set_spawn_model(spawn_config, Some(model.to_string()));
        }
    }

    pub async fn resolve_permission(
        &self,
        session_id: &str,
        nonce: Nonce,
        decision: ApprovalDecision,
        option_id: Option<String>,
    ) -> Result<(), SupervisorError> {
        let client = self.client_for_session(session_id).await?;
        client
            .resolve_permission(nonce, decision, option_id)
            .await?;
        Ok(())
    }

    /// The user dismissed the approval card without answering it.
    pub async fn cancel_permission(
        &self,
        session_id: &str,
        nonce: Nonce,
    ) -> Result<(), SupervisorError> {
        let client = self.client_for_session(session_id).await?;
        client.cancel_permission(nonce).await?;
        Ok(())
    }

    pub async fn resolve_elicitation(
        &self,
        session_id: &str,
        nonce: Nonce,
        resolution: ElicitationResolution,
    ) -> Result<(), SupervisorError> {
        let client = self.client_for_session(session_id).await?;
        client.resolve_elicitation(nonce, resolution).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::{ResumeKind, ResumeReservationOutcome, WorkerKind};
    use super::*;
    use crate::acp::state::AcpSessionId;
    use crate::daemon::AcpWorkerState;

    #[tokio::test(start_paused = true)]
    async fn wait_for_worker_blocks_on_a_reservation_until_it_drops() {
        let sup = Arc::new(Supervisor::new(VecSink::new()));
        assert!(
            !sup.wait_for_worker("s-1748", Duration::from_secs(60)).await,
            "with no reservation, wait_for_worker fails fast"
        );

        let reservation = reserve(sup.begin_resume("s-1748", ResumeKind::Spawn).await);
        assert!(
            sup.is_running("s-1748").await,
            "a reservation counts as running so the reconciler skips it"
        );
        assert_eq!(sup.worker_state("s-1748").await, AcpWorkerState::Resuming);
        assert!(matches!(
            sup.begin_resume("s-1748", ResumeKind::Spawn).await.unwrap(),
            ResumeReservationOutcome::AlreadyPresent
        ));

        let mut entered = sup.watch_worker_waits();
        let waiter = {
            let sup = Arc::clone(&sup);
            tokio::spawn(
                async move { sup.wait_for_worker("s-1748", Duration::from_secs(60)).await },
            )
        };
        assert_eq!(entered.recv().await.unwrap(), "s-1748");
        assert!(!waiter.is_finished());
        let before = tokio::time::Instant::now();
        drop(reservation);
        let woke = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("reservation drop must notify the waiter")
            .unwrap();
        assert!(!woke);
        assert_eq!(
            tokio::time::Instant::now(),
            before,
            "the wakeup must not depend on a poll timer"
        );
        assert_eq!(sup.worker_state("s-1748").await, AcpWorkerState::Absent);
    }

    #[tokio::test]
    async fn requests_racing_a_force_stop_teardown_are_not_faults() {
        let sup = Supervisor::new(VecSink::new());
        sup.test_install_handle(
            "s-3401",
            AcpClient::fake_for_test_dead_connection(AcpSessionId("acp-3401".into())),
            WorkerKind::Stdio,
            None,
        )
        .await;

        assert!(
            sup.cancel_prompt("s-3401").await.is_ok(),
            "the turn a cancel would have ended is already over"
        );
        assert!(
            matches!(
                sup.send_prompt("s-3401", "hi", &[]).await,
                Err(SupervisorError::Acp(AcpError::AgentExited))
            ),
            "a prompt that did not land keeps its typed reason"
        );
    }

    #[tokio::test]
    async fn reset_session_context_resets_then_reasserts_mode_without_clearing_on_failure() {
        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        let cmds = sup.test_insert_worker_cmd_recording("s-reset").await;

        sup.reset_session_context("s-reset", "/new", None, false)
            .await
            .expect("reset ok");
        assert_eq!(
            cmds.lock().unwrap().clone(),
            vec!["reset_session"],
            "the clear alias drives a reset, not a prompt forward"
        );
        cmds.lock().unwrap().clear();
        sup.reset_session_context("s-reset", "/new", Some("plan"), false)
            .await
            .expect("reset ok");
        let deadline = Instant::now() + Duration::from_secs(2);
        while cmds.lock().unwrap().len() < 2 && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            cmds.lock().unwrap().clone(),
            vec!["reset_session", "set_mode"],
            "an explicit persisted mode is re-asserted after the reset"
        );

        let (client, _tx) = AcpClient::fake_for_test_reset_failure(
            AcpSessionId("s-reset-busy".into()),
            "a turn is in flight; stop it before clearing the conversation",
        );
        sup.test_install_handle("s-reset-busy", client, WorkerKind::Stdio, None)
            .await;
        let error = sup
            .reset_session_context("s-reset-busy", "/new", None, false)
            .await
            .expect_err("busy reset must be rejected");
        assert!(
            matches!(
                &error,
                SupervisorError::Acp(AcpError::ResetFailed(message))
                    if message.contains("turn is in flight")
            ),
            "the busy classification must remain visible, got {error:?}"
        );
        assert!(
            !sink
                .frames
                .lock()
                .unwrap()
                .iter()
                .any(|(_, _, event)| matches!(event, Event::SessionCleared)),
            "a rejected reset must not publish SessionCleared"
        );
    }
}
