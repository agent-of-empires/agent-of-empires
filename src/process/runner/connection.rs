//! One daemon control-channel attachment: attach handshake, reader loop, and writer task.

use super::jsonrpc::read_frame_bounded;
use super::shared::{RunnerShared, CONTROL_ATTACH_TIMEOUT, CONTROL_WRITE_TIMEOUT};
use super::STDOUT_READ_BUF;
use crate::acp::control_protocol::{self, ControlBody};
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
    loop {
        match read_frame_bounded(&mut reader, &mut line).await {
            Ok(0) => {
                debug!(target: "acp.runner", session = %session_id, "agent stdout EOF");
                break;
            }
            Ok(_) => {
                shared.deliver_line(&line, &agent_stdin).await;
            }
            Err(e) => {
                warn!(target: "acp.runner", session = %session_id, "stdout read error: {e}");
                break;
            }
        }
    }
    shared.abort_agent_calls(&session_id).await;
}

/// The queue keeps the front entry until a full write and flush succeeds.
pub(super) async fn run_control_writer(
    mut out: tokio::net::unix::OwnedWriteHalf,
    shared: Arc<RunnerShared>,
    session_id: String,
    attachment_id: u64,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        if *shutdown.borrow() {
            return;
        }
        while let Some((entry_id, wire)) = shared.next_outbound(attachment_id).await {
            let written = tokio::select! {
                biased;
                _ = shutdown.changed() => {
                    shared.release_outbound(entry_id).await;
                    return;
                }
                result = tokio::time::timeout(
                    CONTROL_WRITE_TIMEOUT,
                    control_protocol::write_encoded_frame(&mut out, &wire),
                ) => matches!(result, Ok(Ok(()))),
            };
            if !written {
                warn!(
                    target: "acp.runner",
                    session = %session_id,
                    "control write failed or timed out; retaining frame for the next attach"
                );
                shared.release_outbound(entry_id).await;
                return;
            }
            shared.commit_outbound(attachment_id, entry_id).await;
        }
        tokio::select! {
            _ = shared.control_wake.notified() => {}
            _ = shutdown.changed() => return,
        }
    }
}

pub(super) enum HandshakeCommand {
    Initialize(serde_json::Value),
    ResumeSession,
    EstablishSession {
        method: String,
        request: serde_json::Value,
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
    if !shared.install_control(&mut write_half, &session_id).await {
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
        Ok(Ok(Some(frame))) => {
            warn!(target: "acp.runner", session = %session_id, ?frame, "first daemon frame was not Attach");
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

    if let Some(wire) = shared.cached_prompt_completion().await {
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
    let (writer_shutdown_tx, writer_shutdown_rx) = watch::channel(false);
    let mut writer = tokio::spawn(run_control_writer(
        write_half
            .take()
            .expect("write half present after greeting"),
        Arc::clone(&shared),
        session_id.clone(),
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
            let mut dispatch_release = None;
            let mut established = false;
            let handshake = async {
                match command {
                    HandshakeCommand::Initialize(request) => match handshake_shared
                        .run_or_replay_initialize(&handshake_stdin, request, &mut dispatch_release)
                        .await
                    {
                        Ok(result) => ControlBody::Initialized { result },
                        Err(error) => {
                            warn!(target: "acp.runner", session = %handshake_session, "initialize failed: {error}");
                            ControlBody::HandshakeFailed { error }
                        }
                    },
                    HandshakeCommand::ResumeSession => {
                        match handshake_shared.resume_session().await {
                            Ok((acp_session_id, result)) => ControlBody::SessionReady {
                                acp_session_id,
                                result,
                            },
                            Err(error) => ControlBody::HandshakeFailed { error },
                        }
                    }
                    HandshakeCommand::EstablishSession { method, request } => {
                        match handshake_shared
                            .run_or_replay_session(
                                &handshake_stdin,
                                &method,
                                request,
                                &mut dispatch_release,
                            )
                            .await
                        {
                            Ok((acp_session_id, result)) => {
                                handshake_done.store(true, Ordering::Release);
                                established = true;
                                ControlBody::SessionReady {
                                    acp_session_id,
                                    result,
                                }
                            }
                            Err(error) => {
                                warn!(target: "acp.runner", session = %handshake_session, "{method} failed: {error}");
                                ControlBody::HandshakeFailed { error }
                            }
                        }
                    }
                }
            };
            let Some(frame) = await_handshake_or_control_loss(&mut control_closed, handshake).await
            else {
                return;
            };
            handshake_shared
                .enqueue_handshake(attachment_id, frame, established)
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
                Ok(None) => break 'connection !handshake_complete.load(Ordering::Acquire),
                Err(error) => {
                    warn!(target: "acp.runner", session = %session_id, "control read error: {error}");
                    break 'connection !handshake_complete.load(Ordering::Acquire);
                }
            }
        };
        let command = match body {
            ControlBody::Initialize { request } => HandshakeCommand::Initialize(request),
            ControlBody::ResumeSession => HandshakeCommand::ResumeSession,
            ControlBody::EstablishSession { method, request } => {
                HandshakeCommand::EstablishSession { method, request }
            }
            ControlBody::Prompt { request } => {
                let prompt = shared.agent_prompt(&agent_stdin, attachment_id, request);
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
                if prompt_id.is_none() {
                    warn!(target: "acp.runner", session = %session_id, "prompt write to agent failed");
                }
                continue;
            }
            ControlBody::Cancel => {
                if let Some(acp_session_id) = shared.acp_session_id().await {
                    shared.agent_cancel(&agent_stdin, &acp_session_id).await;
                }
                continue;
            }
            ControlBody::ServerResult { call_id, result } => {
                shared
                    .resolve_server_call(
                        &agent_stdin,
                        attachment_id,
                        call_id,
                        Ok(result),
                        &session_id,
                    )
                    .await;
                continue;
            }
            ControlBody::ServerError { call_id, error } => {
                shared
                    .resolve_server_call(
                        &agent_stdin,
                        attachment_id,
                        call_id,
                        Err(error),
                        &session_id,
                    )
                    .await;
                continue;
            }
            ControlBody::AgentCall {
                call_id,
                method,
                params,
            } => {
                shared
                    .issue_agent_call(&agent_stdin, attachment_id, call_id, &method, params)
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
        let _ = writer.await;
    }
    shared
        .disconnect_control(attachment_id, &agent_stdin, &session_id)
        .await;
    drop(handshake_tx);
    let _ = handshake_worker.await;
    terminate_runner
}
pub(super) fn log_writer_exit(result: Result<(), tokio::task::JoinError>, session_id: &str) {
    if let Err(error) = result {
        warn!(target: "acp.runner", session = %session_id, "control writer task failed: {error}");
    }
}
