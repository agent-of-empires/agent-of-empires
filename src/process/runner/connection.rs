//! One daemon control-channel attachment: attach handshake, reader loop, and writer task.

use super::jsonrpc::read_frame_bounded;
use super::shared::{RunnerShared, CONTROL_ATTACH_TIMEOUT, CONTROL_WRITE_TIMEOUT};
use super::STDOUT_READ_BUF;
use crate::acp::control_protocol::{self, ControlBody, NativeChannelLossCause};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::sync::{mpsc, watch, Mutex};
use tracing::{debug, warn};

pub(super) async fn fanout_agent_stdout(
    stdout: tokio::process::ChildStdout,
    shared: Arc<RunnerShared>,
    agent_stdin: Arc<Mutex<tokio::process::ChildStdin>>,
    session_id: String,
) {
    let mut reader = BufReader::with_capacity(STDOUT_READ_BUF, stdout);
    let mut line = Vec::with_capacity(4096);
    let mut retain_reader = false;
    loop {
        let frame = tokio::select! {
            biased;
            _ = shared.wait_native_loss() => {
                retain_reader = true;
                break;
            }
            frame = read_frame_bounded(&mut reader, &mut line) => frame,
        };
        match frame {
            Ok(0) => {
                debug!(target: "acp.runner", session = %session_id, "agent stdout EOF");
                shared.observe_native_loss(NativeChannelLossCause::StdoutEof);
                break;
            }
            Ok(_) => {
                shared.deliver_line(&line, &agent_stdin).await;
            }
            Err(e) => {
                warn!(target: "acp.runner", session = %session_id, "stdout read error: {e}");
                retain_reader = true;
                shared.observe_native_loss(if e.kind() == std::io::ErrorKind::InvalidData {
                    NativeChannelLossCause::ProtocolEnded
                } else {
                    NativeChannelLossCause::ReadFailed
                });
                break;
            }
        }
    }
    shared.retire_native_waiters().await;
    if retain_reader {
        // Closing a still-live native stdout here could SIGPIPE the child.
        // Retain only its descriptor until this runner-owned task is aborted
        // by the existing explicit shutdown/process-exit lifecycle.
        drop(line);
        let native_stdout = reader.into_inner();
        std::future::pending::<()>().await;
        drop(native_stdout);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ControlWriteCause {
    TimedOut,
    Io(std::io::ErrorKind),
}

/// Payload-free evidence for the write that ended an attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ControlWriteFailure {
    pub(super) cause: ControlWriteCause,
    pub(super) frame_bytes: usize,
    pub(super) queued_frames: usize,
    pub(super) queued_bytes: usize,
}

/// The queue keeps the front entry until a full write and flush succeeds.
pub(super) async fn run_control_writer(
    mut out: tokio::net::unix::OwnedWriteHalf,
    shared: Arc<RunnerShared>,
    attachment_id: u64,
    mut shutdown: watch::Receiver<bool>,
) -> Option<ControlWriteFailure> {
    loop {
        if *shutdown.borrow() {
            return None;
        }
        while let Some((entry_id, wire)) = shared.next_outbound(attachment_id).await {
            let written = tokio::select! {
                biased;
                _ = shutdown.changed() => {
                    shared.release_outbound(entry_id).await;
                    return None;
                }
                result = tokio::time::timeout(
                    CONTROL_WRITE_TIMEOUT,
                    control_protocol::write_encoded_frame(&mut out, &wire),
                ) => result,
            };
            let cause = match written {
                Ok(Ok(())) => {
                    shared.commit_outbound(attachment_id, entry_id).await;
                    continue;
                }
                Ok(Err(error)) => ControlWriteCause::Io(
                    error
                        .downcast_ref::<std::io::Error>()
                        .map_or(std::io::ErrorKind::Other, std::io::Error::kind),
                ),
                Err(_) => ControlWriteCause::TimedOut,
            };
            shared.release_outbound(entry_id).await;
            let (queued_frames, queued_bytes) = shared.control_queue_depth().await;
            return Some(ControlWriteFailure {
                cause,
                frame_bytes: wire.len(),
                queued_frames,
                queued_bytes,
            });
        }
        tokio::select! {
            _ = shared.control_wake.notified() => {}
            _ = shutdown.changed() => return None,
        }
    }
}

pub(super) enum HandshakeCommand {
    Initialize(serde_json::Value),
    ResumeSession,
    EstablishSession {
        method: String,
        request: serde_json::Value,
        attempt: Arc<crate::acp::establishment::EstablishmentAttempt>,
        store: Option<crate::session::ExecutionBinding>,
    },
}

pub(super) async fn await_handshake_or_control_loss<T>(
    control_closed: &mut watch::Receiver<bool>,
    handshake: impl std::future::Future<Output = T>,
) -> Option<T> {
    if *control_closed.borrow() {
        return None;
    }
    tokio::pin!(handshake);
    tokio::select! {
        biased;
        _ = control_closed.changed() => None,
        result = &mut handshake => Some(result),
    }
}

pub(super) async fn handle_control_connection(
    stream: UnixStream,
    shared: Arc<RunnerShared>,
    agent_stdin: Arc<Mutex<tokio::process::ChildStdin>>,
    session_id: String,
) -> bool {
    let (mut read_half, write_half) = stream.into_split();
    let mut write_half = Some(write_half);
    let (prompt_head, cached_completion, autonomous) = shared.prompt_attachment_snapshot().await;
    if !shared
        .install_control(&mut write_half, &session_id, prompt_head, autonomous)
        .await
    {
        return false;
    }

    let attach = tokio::time::timeout(
        CONTROL_ATTACH_TIMEOUT,
        control_protocol::read_frame(&mut read_half),
    )
    .await;
    match attach {
        Ok(Ok(Some(ControlBody::Attach {
            control_protocol_version,
        }))) if control_protocol_version == control_protocol::CONTROL_PROTOCOL_VERSION => {}
        Ok(Ok(Some(ControlBody::Attach {
            control_protocol_version,
        }))) => {
            warn!(
                target: "acp.runner",
                session = %session_id,
                daemon_version = control_protocol_version,
                runner_version = control_protocol::CONTROL_PROTOCOL_VERSION,
                "daemon attached with a mismatched control version; refusing the connection"
            );
            return false;
        }
        Ok(Ok(Some(_))) => {
            warn!(target: "acp.runner", session = %session_id, "first daemon frame was not Attach");
            return false;
        }
        Ok(Ok(None)) => return false,
        Ok(Err(error)) => {
            warn!(target: "acp.runner", session = %session_id, "failed to read Attach: {error}");
            return false;
        }
        Err(_) => {
            warn!(target: "acp.runner", session = %session_id, "timed out waiting for Attach");
            return false;
        }
    }

    if let Some(wire) = cached_completion {
        let replayed = matches!(
            tokio::time::timeout(
                CONTROL_WRITE_TIMEOUT,
                control_protocol::write_encoded_frame(
                    write_half.as_mut().expect("write half present"),
                    &wire,
                ),
            )
            .await,
            Ok(Ok(()))
        );
        if !replayed {
            return false;
        }
    }

    let attachment_id = shared.begin_attachment().await;
    #[cfg(test)]
    let _ = super::test_support::managed_runner::OBSERVE_CONTROL.try_with(|observer| {
        use std::os::fd::AsFd;
        let socket = read_half
            .as_ref()
            .as_fd()
            .try_clone_to_owned()
            .expect("private actual socket observation");
        let _ = observer.send((attachment_id, socket));
    });
    let (writer_shutdown_tx, writer_shutdown_rx) = watch::channel(false);
    let mut writer = tokio::spawn(run_control_writer(
        write_half
            .take()
            .expect("write half present after greeting"),
        Arc::clone(&shared),
        attachment_id,
        writer_shutdown_rx,
    ));
    let mut writer_finished = false;

    let (control_closed_tx, control_closed_rx) = watch::channel(false);
    let handshake_complete = Arc::new(std::sync::atomic::AtomicBool::new(
        shared.acp_session_id().await.is_some(),
    ));
    let (handshake_tx, mut handshake_rx) = mpsc::channel(8);
    let handshake_shared = Arc::clone(&shared);
    let handshake_stdin = Arc::clone(&agent_stdin);
    let handshake_done = Arc::clone(&handshake_complete);
    let handshake_session = session_id.clone();
    let handshake_worker = tokio::spawn(async move {
        let mut control_closed = control_closed_rx;
        while let Some(command) = handshake_rx.recv().await {
            let command = match command {
                HandshakeCommand::EstablishSession {
                    method,
                    request,
                    attempt,
                    store,
                } => {
                    // Once accepted, collection belongs to the runner even if
                    // this attachment disappears before the native reply.
                    handshake_done.store(true, Ordering::Release);
                    handshake_shared
                        .start_or_replay_session(
                            Arc::clone(&handshake_stdin),
                            attachment_id,
                            method,
                            request,
                            attempt,
                            store,
                        )
                        .await;
                    continue;
                }
                command => command,
            };
            let mut dispatch_release = None;
            let handshake = async {
                match command {
                    HandshakeCommand::Initialize(request) => match handshake_shared
                        .run_or_replay_initialize(&handshake_stdin, request, &mut dispatch_release)
                        .await
                    {
                        Ok(result) => ControlBody::Initialized { result },
                        Err(error) => {
                            warn!(target: "acp.runner", session = %handshake_session, ?error, "initialize failed");
                            error.into_frame("initialize")
                        }
                    },
                    HandshakeCommand::ResumeSession => {
                        match handshake_shared.resume_session(attachment_id).await {
                            Some((cached, original_delivery)) => cached
                                .frame(handshake_shared.native_availability(), original_delivery),
                            None => control_protocol::HandshakeFailure::new(
                                control_protocol::HandshakeFailureOrigin::LocalState,
                                super::shared::transport_error(
                                    "runner has no original establishment result",
                                ),
                            )
                            .into_frame("resume_session"),
                        }
                    }
                    HandshakeCommand::EstablishSession { .. } => {
                        unreachable!("runner-owned establishment handled before attachment wait")
                    }
                }
            };
            let Some(frame) = await_handshake_or_control_loss(&mut control_closed, handshake).await
            else {
                return;
            };
            handshake_shared
                .enqueue_handshake(attachment_id, frame, dispatch_release.is_some())
                .await;
            drop(dispatch_release);
        }
    });

    let terminate_runner = 'connection: loop {
        let body = tokio::select! {
            result = &mut writer => {
                writer_finished = true;
                log_writer_exit(result, &session_id);
                break 'connection false;
            }
            result = control_protocol::read_frame(&mut read_half) => match result {
                Ok(Some(frame)) => frame,
                Ok(None) => break 'connection shared.native_loss().is_none()
                    && !handshake_complete.load(Ordering::Acquire),
                Err(error) => {
                    warn!(target: "acp.runner", session = %session_id, "control read error: {error}");
                    break 'connection shared.native_loss().is_none()
                        && !handshake_complete.load(Ordering::Acquire);
                }
            }
        };
        let command = match body {
            ControlBody::Initialize { request } => HandshakeCommand::Initialize(request),
            ControlBody::ResumeSession => HandshakeCommand::ResumeSession,
            ControlBody::EstablishSession {
                method,
                request,
                attempt,
                store,
            } => {
                handshake_complete.store(true, Ordering::Release);
                HandshakeCommand::EstablishSession {
                    method,
                    request,
                    attempt,
                    store,
                }
            }
            ControlBody::Prompt { request, scope } => {
                let prompt = shared.agent_prompt(&agent_stdin, attachment_id, request, scope);
                tokio::pin!(prompt);
                let prompt_id = tokio::select! {
                    result = &mut writer => {
                        writer_finished = true;
                        log_writer_exit(result, &session_id);
                        // Release admission backpressure without cancelling an in-progress write to the agent.
                        shared.control.lock().await.purge_attachment(attachment_id);
                        shared.control_space.notify_waiters();
                        let _ = prompt.await;
                        break 'connection false;
                    }
                    result = &mut prompt => result,
                };
                #[cfg(test)]
                shared.test_record_producer(|| {
                    serde_json::json!({
                        "stage": "prompt-returned",
                        "id": prompt_id,
                    })
                });
                if prompt_id.is_none() {
                    warn!(target: "acp.runner", session = %session_id, "prompt write to agent failed");
                }
                continue;
            }
            ControlBody::Cancel { scope } => {
                if let Some(acp_session_id) = shared.acp_session_id().await {
                    shared
                        .agent_cancel(
                            &agent_stdin,
                            attachment_id,
                            &acp_session_id,
                            &acp_session_id,
                            scope,
                        )
                        .await;
                }
                continue;
            }
            ControlBody::CancelSession {
                parent_session_id,
                session_id: child_session_id,
                scope,
            } => {
                shared
                    .agent_cancel(
                        &agent_stdin,
                        attachment_id,
                        &parent_session_id,
                        &child_session_id,
                        scope,
                    )
                    .await;
                continue;
            }
            reply @ (ControlBody::ServerResult { .. } | ControlBody::ServerError { .. }) => {
                let (call_id, outcome) = match reply {
                    ControlBody::ServerResult { call_id, result } => (call_id, Ok(result)),
                    ControlBody::ServerError { call_id, error } => (call_id, Err(error)),
                    _ => unreachable!("matched reverse reply"),
                };
                let resolve = shared.resolve_server_call(
                    &agent_stdin,
                    attachment_id,
                    call_id,
                    outcome,
                    &session_id,
                );
                tokio::pin!(resolve);
                let settled = tokio::select! {
                    result = &mut writer => {
                        writer_finished = true;
                        log_writer_exit(result, &session_id);
                        shared.control.lock().await.purge_attachment(attachment_id);
                        shared.control_space.notify_waiters();
                        // Finish the accepted Native write without rerouting its settlement.
                        let _ = resolve.await;
                        break 'connection false;
                    }
                    settled = &mut resolve => settled,
                };
                if !settled {
                    break 'connection false;
                }
                continue;
            }
            ControlBody::AgentCall {
                call_id,
                method,
                params,
                scope,
            } => {
                shared
                    .issue_agent_call(&agent_stdin, attachment_id, call_id, &method, params, scope)
                    .await;
                continue;
            }
            _ => continue,
        };
        let send = handshake_tx.send(command);
        tokio::pin!(send);
        let sent = tokio::select! {
            result = &mut writer => {
                writer_finished = true;
                log_writer_exit(result, &session_id);
                false
            }
            result = &mut send => result.is_ok(),
        };
        if !sent {
            break 'connection false;
        }
    };

    let _ = control_closed_tx.send(true);
    if !writer_finished {
        let _ = writer_shutdown_tx.send(true);
        log_writer_exit(writer.await, &session_id);
    }
    shared
        .disconnect_control(attachment_id, &agent_stdin, &session_id)
        .await;
    drop(handshake_tx);
    let _ = handshake_worker.await;
    terminate_runner
}
pub(super) fn log_writer_exit(
    result: Result<Option<ControlWriteFailure>, tokio::task::JoinError>,
    session_id: &str,
) {
    match result {
        Ok(Some(failure)) => warn!(
            target: "acp.runner",
            session = %session_id,
            cause = ?failure.cause,
            timeout_ms = CONTROL_WRITE_TIMEOUT.as_millis() as u64,
            frame_bytes = failure.frame_bytes,
            queued_frames = failure.queued_frames,
            queued_bytes = failure.queued_bytes,
            "control write failed; retaining frame for the next attach"
        ),
        Ok(None) => {}
        Err(error) => {
            warn!(target: "acp.runner", session = %session_id, "control writer task failed: {error}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::runner::shared::{DeliveryScope, QueuedKind};

    /// A failed write reports why and how much was waiting, never what.
    #[tokio::test(start_paused = true)]
    async fn writer_failure_separates_timeout_from_io_error() {
        for (peer_holds_open_unread, expected) in [
            (true, ControlWriteCause::TimedOut),
            (false, ControlWriteCause::Io(std::io::ErrorKind::BrokenPipe)),
        ] {
            let shared = Arc::new(RunnerShared::new(None));
            let attachment = shared.begin_attachment().await;
            shared.control.lock().await.session_announced = true;
            // Larger than any socket buffer, so an idle reader stalls the write.
            let frames = [4 * 1024 * 1024, 16].map(|len| ControlBody::Notify {
                method: "session/update".into(),
                params: serde_json::json!({ "text": "x".repeat(len) }),
                evidence: Default::default(),
                replay: None,
            });
            let wire: Vec<usize> = frames
                .iter()
                .map(|frame| control_protocol::encode_frame(frame).unwrap().len())
                .collect();
            for frame in frames {
                assert!(
                    shared
                        .enqueue(DeliveryScope::Persistent, QueuedKind::Notify, frame)
                        .await
                );
            }
            let (daemon, runner) = UnixStream::pair().unwrap();
            let _daemon = peer_holds_open_unread.then_some(daemon);
            let (_read, write) = runner.into_split();
            let (_shutdown, shutdown_rx) = watch::channel(false);
            let failure = run_control_writer(write, Arc::clone(&shared), attachment, shutdown_rx)
                .await
                .expect("the write fails");
            assert_eq!(
                failure,
                ControlWriteFailure {
                    cause: expected,
                    frame_bytes: wire[0],
                    queued_frames: 2,
                    queued_bytes: wire.iter().sum(),
                }
            );
        }
    }
}
