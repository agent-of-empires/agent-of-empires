//! The v2 runner control socket: connecting, establishing a session, and
//! relaying prompts over it.

use crate::acp::control_protocol::{self, ControlBody};
use crate::acp::state::Event;
use agent_client_protocol::schema::v1::PromptResponse;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::Ordering as AtomicOrdering;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

use super::errors::{acp_error_from_value, acp_internal_error, AcpError};
use super::lifecycle::TerminalClaim;

/// Cancel a socket handshake if its constructor is dropped before completion.
/// Closing the exact runner control channel cancels only that runner.
pub(super) struct ShutdownControlOnDrop(pub(super) Option<Arc<DaemonControlClient>>);

impl Drop for ShutdownControlOnDrop {
    fn drop(&mut self) {
        if let Some(control) = self.0.take() {
            control.shutdown();
        }
    }
}

/// Daemon side of the runner's v2 control channel (see #2976): the
/// daemon drives `initialize` / `session/*` / `session/prompt` /
/// `session/cancel` over this channel and receives the typed results,
/// rather than speaking those methods over the byte relay.
///
/// `initialize` / `session/*` responses arrive sequentially on
/// `handshake_rx`. A turn's `PromptCompleted` lands in `completion_rx`
/// while a prompt is in flight, else (an adopted turn on a mid-flight
/// resume, where this daemon never issued the prompt) the reader
/// CAS-claims the terminal guard and fires `Stopped` so the UI clears.
pub(super) struct DaemonControlClient {
    write: Mutex<tokio::net::unix::OwnedWriteHalf>,
    handshake_rx: Mutex<mpsc::Receiver<ControlBody>>,
    /// Outcomes of turns this daemon issued. Persistent rather than a
    /// per-prompt oneshot so the reader always has somewhere to deliver
    /// while a prompt is in flight; a completion can never find "no
    /// waiter" and strand the prompt future (#3203).
    completion_rx: Mutex<mpsc::Receiver<control_protocol::PromptOutcome>>,
    raw_fd: RawFd,
}

impl DaemonControlClient {
    pub(super) fn shutdown(&self) {
        // SAFETY: `self` keeps this exact socket alive for the call.
        unsafe { libc::shutdown(self.raw_fd, libc::SHUT_RDWR) };
    }

    async fn send(&self, body: ControlBody) -> Result<(), AcpError> {
        let mut w = self.write.lock().await;
        control_protocol::write_frame(&mut *w, &body)
            .await
            .map_err(|e| AcpError::Spawn(format!("control write failed: {e}")))
    }

    /// Run the ACP `initialize` the runner owns; returns the raw result
    /// value to deserialize into `InitializeResponse`. A `HandshakeFailed`
    /// is surfaced as the reconstructed crate error so the caller propagates
    /// the same `AgentStartupError` (with `data.details`) the relay path did.
    pub(super) async fn initialize(
        &self,
        request: serde_json::Value,
    ) -> Result<serde_json::Value, agent_client_protocol::Error> {
        self.send(ControlBody::Initialize { request })
            .await
            .map_err(|e| acp_internal_error(format!("control write failed: {e}")))?;
        match self.handshake_rx.lock().await.recv().await {
            Some(ControlBody::Initialized { result }) => Ok(result),
            Some(ControlBody::HandshakeFailed { error }) => Err(acp_error_from_value(error)),
            _ => Err(acp_internal_error(
                "control channel closed during initialize".into(),
            )),
        }
    }

    /// Run the session-creation request the runner owns; returns
    /// `(acp_session_id, raw result)` to deserialize into the matching
    /// session response, or the reconstructed crate error on failure.
    async fn establish_session(
        &self,
        method: &str,
        request: serde_json::Value,
    ) -> Result<(String, serde_json::Value), agent_client_protocol::Error> {
        self.send(ControlBody::EstablishSession {
            method: method.to_string(),
            request,
        })
        .await
        .map_err(|e| acp_internal_error(format!("control write failed: {e}")))?;
        match self.handshake_rx.lock().await.recv().await {
            Some(ControlBody::SessionReady {
                acp_session_id,
                result,
            }) => Ok((acp_session_id, result)),
            Some(ControlBody::HandshakeFailed { error }) => Err(acp_error_from_value(error)),
            _ => Err(acp_internal_error(
                "control channel closed during session establishment".into(),
            )),
        }
    }

    /// Issue a turn and wait for the runner's `PromptCompleted`. An outcome
    /// left over from a turn the caller stopped waiting on is drained first;
    /// one that arrives only after this prompt was issued is still taken as
    /// this turn's, as before, since the daemon does not track the runner's
    /// prompt request ids. A failed write resolves as `Aborted` at once.
    pub(super) async fn prompt(
        &self,
        request: serde_json::Value,
    ) -> control_protocol::PromptOutcome {
        let mut rx = self.completion_rx.lock().await;
        while rx.try_recv().is_ok() {
            debug!(
                target: "acp.protocol",
                "discarding a prompt outcome nothing was waiting on"
            );
        }
        if self.send(ControlBody::Prompt { request }).await.is_err() {
            return control_protocol::PromptOutcome::Aborted;
        }
        debug!(target: "acp.protocol", "prompt issued; awaiting its completion");
        rx.recv()
            .await
            .unwrap_or(control_protocol::PromptOutcome::Aborted)
    }

    pub(super) async fn cancel(&self) {
        let _ = self.send(ControlBody::Cancel).await;
    }
}

/// Dial the runner's sibling control socket and, if it speaks control
/// protocol v2 (#2976), return a [`DaemonControlClient`] and spawn its
/// reader. Returns None for an absent or older (v1) runner, whose caller
/// falls back to the byte-relay handshake plus the resume-idle watchdog.
pub(super) async fn connect_runner_control_v2(
    main_socket: &std::path::Path,
    event_tx: mpsc::Sender<Event>,
    session_label: String,
    terminal_claim: Arc<TerminalClaim>,
    prompt_in_flight: Arc<std::sync::atomic::AtomicBool>,
) -> Option<Arc<DaemonControlClient>> {
    let control_path = crate::process::worker::control_socket_sibling(main_socket);
    // A single deadline covers connect plus the Hello read. The runner
    // binds the control socket before the main relay socket the caller
    // already waited for, so both steps are effectively immediate; the
    // bound only caps a wedged runner that bound the socket but never
    // greets. An old socketless runner fails connect at once and falls
    // back to the byte-relay handshake.
    let bound = std::time::Duration::from_secs(2);
    let dial = async {
        let stream = tokio::net::UnixStream::connect(&control_path).await.ok()?;
        let (mut read_half, mut write_half) = stream.into_split();
        match control_protocol::read_frame(&mut read_half).await {
            Ok(Some(ControlBody::Hello {
                control_protocol_version,
                ..
            })) if control_protocol_version == control_protocol::CONTROL_PROTOCOL_VERSION => {}
            _ => return None,
        }
        control_protocol::write_frame(
            &mut write_half,
            &ControlBody::Attach {
                control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
            },
        )
        .await
        .ok()?;
        Some((read_half, write_half))
    };
    let (mut read_half, write_half) = match tokio::time::timeout(bound, dial).await {
        Ok(Some(halves)) => halves,
        _ => {
            debug!(
                target: "acp.protocol",
                session = %session_label,
                "no usable v2 runner control socket; using byte-relay handshake + watchdog"
            );
            return None;
        }
    };

    info!(
        target: "acp.protocol",
        session = %session_label,
        "runner control channel v2 attached; runner owns handshake + turn"
    );

    let reader_prompt_in_flight = prompt_in_flight.clone();
    let (hs_tx, hs_rx) = mpsc::channel::<ControlBody>(8);
    // Capacity one: a turn has one completion. A second for the same turn
    // is a runner bug and is dropped with a warning rather than queued to
    // end the next turn.
    let (completion_tx, completion_rx) = mpsc::channel::<control_protocol::PromptOutcome>(1);
    let reader_session = session_label.clone();
    tokio::spawn(async move {
        loop {
            match control_protocol::read_frame(&mut read_half).await {
                Ok(Some(
                    frame @ (ControlBody::Initialized { .. }
                    | ControlBody::SessionReady { .. }
                    | ControlBody::HandshakeFailed { .. }),
                )) => {
                    if hs_tx.send(frame).await.is_err() {
                        return;
                    }
                }
                Ok(Some(ControlBody::PromptCompleted { outcome, .. })) => {
                    if reader_prompt_in_flight.load(AtomicOrdering::Relaxed) {
                        // The prompt arm set `prompt_in_flight` before it
                        // issued the prompt, so it is awaiting this outcome
                        // (or will drain it before its next prompt).
                        match completion_tx.try_send(outcome) {
                            Ok(()) => {}
                            Err(mpsc::error::TrySendError::Full(_)) => warn!(
                                target: "acp.protocol",
                                session = %reader_session,
                                "runner reported a second PromptCompleted for the in-flight turn; dropping it"
                            ),
                            Err(mpsc::error::TrySendError::Closed(_)) => return,
                        }
                        continue;
                    }
                    // No prompt of ours is in flight: an adopted turn from a
                    // mid-flight resume, whose completion this connection
                    // still has to surface.
                    if terminal_claim.claim() {
                        debug!(
                            target: "acp.protocol",
                            session = %reader_session,
                            "runner reported PromptCompleted for an adopted turn; surfacing as Stopped"
                        );
                        let reason = control_outcome_reason(&outcome);
                        let _ = event_tx.send(Event::Stopped { reason }).await;
                    } else {
                        warn!(
                            target: "acp.protocol",
                            session = %reader_session,
                            "runner reported PromptCompleted with no prompt in flight and the turn's terminal already claimed"
                        );
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) => return,
                Err(e) => {
                    debug!(
                        target: "acp.protocol",
                        session = %reader_session,
                        "runner control read ended: {e}"
                    );
                    return;
                }
            }
        }
    });

    let raw_fd = write_half.as_ref().as_raw_fd();
    Some(Arc::new(DaemonControlClient {
        write: Mutex::new(write_half),
        handshake_rx: Mutex::new(hs_rx),
        completion_rx: Mutex::new(completion_rx),
        raw_fd,
    }))
}

/// Map a runner-reported prompt outcome to an `Event::Stopped` reason. A
/// completed turn renders as Idle regardless of stop reason, so the
/// default is `prompt_complete`; the one reason with special downstream
/// handling (`rate_limited`) is preserved when the agent reports it. An
/// agent error-envelope or an aborted turn also renders Idle, so they map
/// to `prompt_complete` as well; the turn is over either way.
///
/// The other ACP stop reasons (`cancelled`, `max_tokens`, `refusal`,
/// `max_turn_requests`) collapse into `prompt_complete`, since they all
/// render Idle today. Preserving their identity for the UI is tracked as a
/// follow-up on the Phase C runner-terminator work (#2977).
pub(super) fn control_outcome_reason(
    outcome: &crate::acp::control_protocol::PromptOutcome,
) -> String {
    use crate::acp::control_protocol::PromptOutcome;
    match outcome {
        PromptOutcome::Completed {
            stop_reason: Some(r),
        } if r == "rate_limited" || r == "rate_limit" => "rate_limited".to_string(),
        _ => "prompt_complete".to_string(),
    }
}

/// Drive a session-creation request over the v2 control channel and
/// deserialize the runner's cached result into the crate response type,
/// so each `session/new|load|fork` site's `Result<Resp, Error>` matches
/// the crate `send_request` path it replaces (including the failure path:
/// the runner-forwarded agent error propagates verbatim).
pub(super) async fn establish_session_v2<Resp: serde::de::DeserializeOwned>(
    control: &DaemonControlClient,
    method: &str,
    request: &impl serde::Serialize,
) -> Result<Resp, agent_client_protocol::Error> {
    let params = serde_json::to_value(request)
        .map_err(|e| acp_internal_error(format!("serialize {method} params: {e}")))?;
    let (_id, result) = control.establish_session(method, params).await?;
    serde_json::from_value(result)
        .map_err(|e| acp_internal_error(format!("deserialize {method} result: {e}")))
}

/// Adapt a runner-reported [`PromptOutcome`](control_protocol::PromptOutcome)
/// into the `Result<PromptResponse, Error>` the prompt loop already
/// consumes, so the loop body is identical for the v2 control path and the
/// legacy crate path. A completed turn maps to its `StopReason`; an agent
/// error-envelope reconstructs a crate `Error` (preserving `data` so
/// `classify_rate_limit_error` still recognizes a rate limit); an aborted
/// turn (runner lost the agent) ends the turn cleanly as `EndTurn`.
pub(super) fn prompt_outcome_to_response(
    outcome: control_protocol::PromptOutcome,
) -> Result<PromptResponse, agent_client_protocol::Error> {
    use control_protocol::PromptOutcome;
    // `PromptResponse` is `#[non_exhaustive]`, so build it by deserializing
    // the ACP `stopReason` string the runner forwarded verbatim (e.g.
    // "cancelled" / "max_tokens" / "end_turn") rather than a struct literal.
    let build = |stop: &str| {
        serde_json::from_value::<PromptResponse>(serde_json::json!({ "stopReason": stop }))
            .map_err(|e| acp_internal_error(format!("build prompt response: {e}")))
    };
    match outcome {
        PromptOutcome::Completed { stop_reason } => {
            build(stop_reason.as_deref().unwrap_or("end_turn"))
        }
        // The runner lost the agent before it answered; end the turn.
        PromptOutcome::Aborted => build("end_turn"),
        // Reconstruct the crate error, preserving `data` so
        // `classify_rate_limit_error` still recognizes a rate limit. The
        // numeric `code` is informational and dropped (the crate `code` is
        // a typed `ErrorCode`); message + data carry the signal.
        PromptOutcome::Error {
            code: _,
            message,
            data,
        } => {
            let mut err = agent_client_protocol::Error::internal_error();
            err.message = message;
            err.data = data;
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Daemon-side control consumer (#2976 Phase B): a v2 runner that greets
    /// with a matching `Hello` and then reports `PromptCompleted` for an
    /// adopted turn (no prompt awaiting on this daemon) drives an
    /// `Event::Stopped { reason: "prompt_complete" }` and claims the shared
    /// terminal guard so the resume-idle watchdog stands down.
    #[tokio::test]
    async fn runner_control_native_completion_fires_stopped() {
        use crate::acp::control_protocol::{self, ControlBody, PromptOutcome};
        use tokio::net::UnixListener;

        let tmp = tempfile::tempdir().unwrap();
        let main_socket = tmp.path().join("s.sock");
        let control = crate::process::worker::control_socket_sibling(&main_socket);

        let listener = UnixListener::bind(&control).unwrap();
        let fake = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut r, mut w) = stream.into_split();
            control_protocol::write_frame(
                &mut w,
                &ControlBody::Hello {
                    control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
                    session_id: "s".into(),
                },
            )
            .await
            .unwrap();
            // Drain the daemon's Attach ack, then report completion.
            let _ = control_protocol::read_frame(&mut r).await;
            control_protocol::write_frame(
                &mut w,
                &ControlBody::PromptCompleted {
                    prompt_req_id: 5,
                    outcome: PromptOutcome::Completed {
                        stop_reason: Some("end_turn".into()),
                    },
                },
            )
            .await
            .unwrap();
            // Hold the socket open so the reader delivers before EOF.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        });

        let (event_tx, mut event_rx) = mpsc::channel::<Event>(8);
        let guard = Arc::new(TerminalClaim::new());
        // Adopted turn: this daemon never issued the prompt.
        let prompt_in_flight = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let client = connect_runner_control_v2(
            &main_socket,
            event_tx,
            "s".into(),
            guard.clone(),
            prompt_in_flight.clone(),
        )
        .await
        .expect("v2 control client");

        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), event_rx.recv())
            .await
            .expect("timed out waiting for Stopped")
            .expect("event channel closed");
        assert!(matches!(ev, Event::Stopped { reason } if reason == "prompt_complete"));
        assert!(guard.claimed(), "the turn's terminal must be claimed");
        drop(client);
        let _ = fake.await;
    }

    /// A fake runner that completes the handshake, then answers every
    /// `Prompt` frame with the outcomes given, in order. `unsolicited` is
    /// sent before any prompt arrives.
    fn scripted_runner(
        control: std::path::PathBuf,
        unsolicited: Vec<crate::acp::control_protocol::PromptOutcome>,
        replies: Vec<crate::acp::control_protocol::PromptOutcome>,
    ) -> tokio::task::JoinHandle<()> {
        use crate::acp::control_protocol::{self, ControlBody};
        use tokio::net::UnixListener;
        let listener = UnixListener::bind(&control).unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut r, mut w) = stream.into_split();
            control_protocol::write_frame(
                &mut w,
                &ControlBody::Hello {
                    control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
                    session_id: "s".into(),
                },
            )
            .await
            .unwrap();
            let _ = control_protocol::read_frame(&mut r).await;
            for (i, outcome) in unsolicited.into_iter().enumerate() {
                control_protocol::write_frame(
                    &mut w,
                    &ControlBody::PromptCompleted {
                        prompt_req_id: i as i64,
                        outcome,
                    },
                )
                .await
                .unwrap();
            }
            for (i, outcome) in replies.into_iter().enumerate() {
                loop {
                    match control_protocol::read_frame(&mut r).await {
                        Ok(Some(ControlBody::Prompt { .. })) => break,
                        Ok(Some(_)) => continue,
                        _ => return,
                    }
                }
                control_protocol::write_frame(
                    &mut w,
                    &ControlBody::PromptCompleted {
                        prompt_req_id: 100 + i as i64,
                        outcome,
                    },
                )
                .await
                .unwrap();
            }
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        })
    }

    /// The cure for #3203: a completion for the prompt this daemon issued
    /// resolves that prompt, never a `Stopped` beside a still-parked future.
    #[tokio::test]
    async fn runner_control_completion_resolves_the_in_flight_prompt() {
        use crate::acp::control_protocol::PromptOutcome;

        let tmp = tempfile::tempdir().unwrap();
        let main_socket = tmp.path().join("s.sock");
        let control = crate::process::worker::control_socket_sibling(&main_socket);
        let fake = scripted_runner(
            control,
            vec![],
            vec![PromptOutcome::Completed {
                stop_reason: Some("end_turn".into()),
            }],
        );

        let (event_tx, mut event_rx) = mpsc::channel::<Event>(8);
        let guard = Arc::new(TerminalClaim::new());
        let prompt_in_flight = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let client = connect_runner_control_v2(
            &main_socket,
            event_tx,
            "s".into(),
            guard.clone(),
            prompt_in_flight.clone(),
        )
        .await
        .expect("v2 control client");

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.prompt(serde_json::json!({ "prompt": [] })),
        )
        .await
        .expect("the prompt must resolve when the runner reports completion");
        assert!(matches!(
            outcome,
            PromptOutcome::Completed { stop_reason: Some(ref r) } if r == "end_turn"
        ));
        assert!(
            !guard.claimed(),
            "the prompt arm owns the terminal for a turn it issued"
        );
        assert!(
            event_rx.try_recv().is_err(),
            "no synthetic Stopped may be emitted beside the resolved prompt"
        );
        drop(client);
        let _ = fake.await;
    }

    /// An outcome that arrived while nothing was awaiting (the arm had
    /// already moved on) is drained before the next prompt, so it cannot end
    /// the new turn early.
    #[tokio::test]
    async fn runner_control_drains_a_stale_outcome_before_the_next_prompt() {
        use crate::acp::control_protocol::PromptOutcome;

        let tmp = tempfile::tempdir().unwrap();
        let main_socket = tmp.path().join("s.sock");
        let control = crate::process::worker::control_socket_sibling(&main_socket);
        let fake = scripted_runner(
            control,
            vec![PromptOutcome::Completed {
                stop_reason: Some("stale".into()),
            }],
            vec![PromptOutcome::Completed {
                stop_reason: Some("fresh".into()),
            }],
        );

        let (event_tx, mut event_rx) = mpsc::channel::<Event>(8);
        let guard = Arc::new(TerminalClaim::new());
        let prompt_in_flight = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let client = connect_runner_control_v2(
            &main_socket,
            event_tx,
            "s".into(),
            guard.clone(),
            prompt_in_flight.clone(),
        )
        .await
        .expect("v2 control client");
        // Let the unsolicited completion land in the channel first.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            client.prompt(serde_json::json!({ "prompt": [] })),
        )
        .await
        .expect("the prompt must resolve with its own completion");
        assert!(
            matches!(
                outcome,
                PromptOutcome::Completed { stop_reason: Some(ref r) } if r == "fresh"
            ),
            "got {outcome:?}"
        );
        assert!(event_rx.try_recv().is_err());
        drop(client);
        let _ = fake.await;
    }

    /// A runner whose `Hello` advertises an unknown control-protocol version
    /// is not trusted: no `Stopped` is emitted and the guard stays unclaimed
    /// so the legacy resume-idle watchdog still fires.
    #[tokio::test]
    async fn runner_control_version_mismatch_leaves_guard_unclaimed() {
        use crate::acp::control_protocol::{self, ControlBody};
        use tokio::net::UnixListener;

        let tmp = tempfile::tempdir().unwrap();
        let main_socket = tmp.path().join("s.sock");
        let control = crate::process::worker::control_socket_sibling(&main_socket);

        let listener = UnixListener::bind(&control).unwrap();
        let fake = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (_r, mut w) = stream.into_split();
            let _ = control_protocol::write_frame(
                &mut w,
                &ControlBody::Hello {
                    control_protocol_version: 999,
                    session_id: "s".into(),
                },
            )
            .await;
        });

        let (event_tx, mut event_rx) = mpsc::channel::<Event>(8);
        let guard = Arc::new(TerminalClaim::new());
        let client = connect_runner_control_v2(
            &main_socket,
            event_tx,
            "s".into(),
            guard.clone(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await;

        assert!(
            client.is_none(),
            "unknown control version must not yield a v2 client"
        );
        assert!(
            !guard.claimed(),
            "unknown control version must not claim the terminal"
        );
        assert!(
            event_rx.try_recv().is_err(),
            "no Stopped emitted on version mismatch"
        );
        let _ = fake.await;
    }

    /// The load-bearing backward-compat path: an old runner that never binds
    /// the control socket leaves the guard unclaimed, so the resume-idle
    /// watchdog remains the terminal authority.
    #[tokio::test]
    async fn runner_control_absent_socket_leaves_guard_unclaimed() {
        let tmp = tempfile::tempdir().unwrap();
        // No control listener is bound at the sibling path.
        let main_socket = tmp.path().join("s.sock");

        let (event_tx, mut event_rx) = mpsc::channel::<Event>(8);
        let guard = Arc::new(TerminalClaim::new());
        let client = connect_runner_control_v2(
            &main_socket,
            event_tx,
            "s".into(),
            guard.clone(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await;

        assert!(
            client.is_none(),
            "absent control socket must fall back to the watchdog"
        );
        assert!(
            !guard.claimed(),
            "absent control socket must fall back to the watchdog"
        );
        assert!(event_rx.try_recv().is_err());
    }
}
