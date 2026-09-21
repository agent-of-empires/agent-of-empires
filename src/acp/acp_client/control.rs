//! The v3 runner control socket: connecting, establishing a session, and
//! routing ACP frames over it.

use crate::acp::control_protocol::{self, ControlBody, SessionReplayed};
use crate::acp::state::Event;
use agent_client_protocol::schema::v1::PromptResponse;
use std::collections::HashMap;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::sync::Arc;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::{mpsc, oneshot, watch, Mutex};
use tracing::{debug, info, warn};

use super::errors::{acp_error_from_value, acp_internal_error, AcpError};
use super::lifecycle::TerminalClaim;
use super::rate_limit::classify_rate_limit_error;
use super::runner::runner_socket_deadline;
use super::session_identity::{SessionIngress, CONTROL_FRAME_BYTES_FIELD};

/// Cancels a socket handshake abandoned mid-construction. Closing this exact
/// channel cancels only its own runner.
pub(super) struct ShutdownControlOnDrop(pub(super) Option<Arc<DaemonControlClient>>);

impl Drop for ShutdownControlOnDrop {
    fn drop(&mut self) {
        if let Some(control) = self.0.take() {
            control.shutdown();
        }
    }
}
/// The runner owns the handshake and the turn. `PromptStarted` binds the local
/// waiter to the runner's request id, and only that id's completion resolves
/// it; before any local prompt, a waiterless completion ends an adopted turn.
pub(super) struct DaemonControlClient {
    pub(super) ingress: Arc<SessionIngress>,
    write: Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    handshake_rx: Mutex<mpsc::Receiver<ControlBody>>,
    sessions_established: AtomicU64,
    sessions_replayed: watch::Sender<u64>,
    completion: Arc<std::sync::Mutex<PromptCompletion>>,
    raw_fd: RawFd,
}

enum PromptCompletion {
    Adopted,
    Pending {
        // `None` until `PromptStarted`, so retained history read during send
        // cannot resolve this waiter.
        prompt_req_id: Option<i64>,
        tx: oneshot::Sender<control_protocol::PromptOutcome>,
    },
    // Kept after delivery: a retained completion can arrive before the prompt
    // loop receives its outcome and clears prompt_in_flight.
    LocalIdle,
}

/// The crate connection speaks over a synthetic duplex, and this maps ids at
/// the boundary (#2977). Reverse (runner to crate) calls get synthetic
/// JSON-RPC ids, forward ones get runner `call_id`s, and the two id spaces
/// must never cross.
#[derive(Default)]
struct ShimCorrelation {
    reverse: HashMap<i64, u64>,
    forward: HashMap<u64, serde_json::Value>,
    /// Negative and descending so it never collides with a crate-minted id.
    next_synthetic: i64,
}

/// Process-wide, so forward ids stay monotonic across attaches.
static NEXT_FORWARD_CALL_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl ShimCorrelation {
    fn synthetic_id(&mut self) -> i64 {
        self.next_synthetic -= 1;
        self.next_synthetic
    }

    fn forward_id(&mut self) -> u64 {
        NEXT_FORWARD_CALL_ID.fetch_add(1, AtomicOrdering::Relaxed)
    }
}

/// False once the crate side has hung up.
async fn shim_write_line(
    duplex: &Mutex<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
    value: &serde_json::Value,
) -> bool {
    use tokio::io::AsyncWriteExt;
    let Ok(mut bytes) = serde_json::to_vec(value) else {
        return false;
    };
    bytes.push(b'\n');
    let mut w = duplex.lock().await;
    w.write_all(&bytes).await.is_ok() && w.flush().await.is_ok()
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

    /// A `HandshakeFailed` reconstructs the agent's own error, as direct stdio
    /// would.
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
            }) => {
                self.sessions_established
                    .fetch_add(1, AtomicOrdering::Relaxed);
                Ok((acp_session_id, result))
            }
            Some(ControlBody::HandshakeFailed { error }) => Err(acp_error_from_value(error)),
            _ => Err(acp_internal_error(
                "control channel closed during session establishment".into(),
            )),
        }
    }

    /// Obtain the runner's committed identity without touching agent session state.
    pub(super) async fn resume_session(&self) -> Result<String, agent_client_protocol::Error> {
        self.send(ControlBody::ResumeSession)
            .await
            .map_err(|e| acp_internal_error(format!("control write failed: {e}")))?;
        match self.handshake_rx.lock().await.recv().await {
            Some(ControlBody::SessionReady { acp_session_id, .. }) => Ok(acp_session_id),
            Some(ControlBody::HandshakeFailed { error }) => Err(acp_error_from_value(error)),
            _ => Err(acp_internal_error(
                "control channel closed during resume".into(),
            )),
        }
    }

    /// Wait until the crate has applied the updates the runner queued ahead of
    /// every established session's replay barrier.
    pub(super) async fn session_replayed(&self) {
        let established = self.sessions_established.load(AtomicOrdering::Relaxed);
        let _ = self
            .sessions_replayed
            .subscribe()
            .wait_for(|replayed| *replayed >= established)
            .await;
    }

    pub(super) fn mark_session_replayed(&self, _: SessionReplayed) {
        self.sessions_replayed
            .send_modify(|replayed| *replayed += 1);
    }

    /// Transfer terminal ownership before the command loop arms a local turn.
    pub(super) fn supersede_adopted_turn(&self) {
        let mut completion = self.completion.lock().expect("completion mutex poisoned");
        if matches!(*completion, PromptCompletion::Adopted) {
            *completion = PromptCompletion::LocalIdle;
        }
    }

    /// Registers the completion waiter before sending the `Prompt` frame.
    pub(super) async fn prompt(
        &self,
        request: serde_json::Value,
    ) -> oneshot::Receiver<control_protocol::PromptOutcome> {
        let (tx, rx) = oneshot::channel();
        {
            let mut completion = self.completion.lock().expect("completion mutex poisoned");
            if matches!(*completion, PromptCompletion::Pending { .. }) {
                let _ = tx.send(control_protocol::PromptOutcome::Error {
                    code: control_protocol::INTERNAL_ERROR as i32,
                    message: "a local prompt is already awaiting completion".into(),
                    data: None,
                });
                return rx;
            }
            *completion = PromptCompletion::Pending {
                prompt_req_id: None,
                tx,
            };
        }
        debug!(target: "acp.protocol", "prompt completion waiter installed");
        if self.send(ControlBody::Prompt { request }).await.is_err() {
            // Dropping the waiter resolves `rx` as aborted right away.
            *self.completion.lock().expect("completion mutex poisoned") =
                PromptCompletion::LocalIdle;
        }
        rx
    }

    pub(super) async fn cancel(&self) {
        let _ = self.send(ControlBody::Cancel).await;
    }
}

/// Dial and validate one v3 runner control socket. Retry only startup races;
/// preserve all permanent I/O, framing, identity, and version failures.
pub(super) async fn connect_runner_control_v3(
    control_path: &std::path::Path,
    event_tx: mpsc::Sender<Event>,
    session_label: String,
    terminal_claim: Arc<TerminalClaim>,
    prompt_in_flight: Arc<std::sync::atomic::AtomicBool>,
) -> anyhow::Result<(Arc<DaemonControlClient>, tokio::io::DuplexStream)> {
    let bound = runner_socket_deadline();
    let dial = async {
        let stream = loop {
            match tokio::net::UnixStream::connect(control_path).await {
                Ok(stream) => break stream,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await
                }
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "connect runner control socket {}: {error}",
                        control_path.display()
                    ));
                }
            }
        };
        let (mut read_half, mut write_half) = stream.into_split();
        match control_protocol::read_frame(&mut read_half).await {
            Ok(Some(ControlBody::Hello {
                control_protocol_version,
                session_id,
            })) if control_protocol_version == control_protocol::CONTROL_PROTOCOL_VERSION
                && session_id == session_label => {}
            Ok(Some(ControlBody::Hello {
                control_protocol_version,
                session_id,
            })) => {
                return Err(anyhow::anyhow!(
                    "runner Hello mismatch: expected session {session_label:?} protocol v{}, got session {session_id:?} protocol v{control_protocol_version}",
                    control_protocol::CONTROL_PROTOCOL_VERSION
                ));
            }
            Ok(Some(frame)) => {
                return Err(anyhow::anyhow!("runner sent {:?} before Hello", frame));
            }
            Ok(None) => return Err(anyhow::anyhow!("runner closed before Hello")),
            Err(error) => return Err(anyhow::anyhow!("read runner Hello: {error}")),
        }
        control_protocol::write_frame(
            &mut write_half,
            &ControlBody::Attach {
                control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
            },
        )
        .await
        .map_err(|error| anyhow::anyhow!("write runner Attach: {error}"))?;
        Ok((read_half, write_half))
    };
    let (mut read_half, write_half) = tokio::time::timeout(bound, dial).await.map_err(|_| {
        anyhow::anyhow!(
            "timed out attaching runner control socket {}",
            control_path.display()
        )
    })??;

    info!(
        target: "acp.protocol",
        session = %session_label,
        "runner control channel v3 attached; runner owns the ACP protocol"
    );

    // 64 KiB matches the stdio pipe size.
    let (crate_side, shim_side) = tokio::io::duplex(64 * 1024);
    let (shim_read, shim_write) = tokio::io::split(shim_side);
    let shim_write = Arc::new(Mutex::new(shim_write));
    let correlation = Arc::new(Mutex::new(ShimCorrelation::default()));

    let raw_fd = write_half.as_ref().as_raw_fd();
    let write_half = Arc::new(Mutex::new(write_half));

    let reader_prompt_in_flight = prompt_in_flight.clone();
    let (hs_tx, hs_rx) = mpsc::channel::<ControlBody>(8);
    let completion = Arc::new(std::sync::Mutex::new(PromptCompletion::Adopted));
    let reader_completion = completion.clone();
    let reader_session = session_label.clone();
    let reader_shim_write = shim_write.clone();
    let reader_correlation = correlation.clone();
    let ingress = Arc::new(SessionIngress::default());
    let reader_ingress = ingress.clone();
    tokio::spawn(async move {
        async {
            loop {
            match control_protocol::read_frame_with_size(&mut read_half).await {
                Ok(Some((
                    frame @ (ControlBody::Initialized { .. }
                    | ControlBody::SessionReady { .. }
                    | ControlBody::HandshakeFailed { .. }),
                    _,
                ))) => {
                    if let ControlBody::SessionReady { acp_session_id, .. } = &frame {
                        reader_ingress.resolve(None, acp_session_id.clone().into());
                    }
                    if hs_tx.send(frame).await.is_err() {
                        return;
                    }
                }
                Ok(Some((ControlBody::PromptStarted { prompt_req_id }, _))) => {
                    let mut completion = reader_completion.lock().expect("completion mutex poisoned");
                    if let PromptCompletion::Pending { prompt_req_id: id @ None, .. } = &mut *completion {
                        *id = Some(prompt_req_id);
                    }
                }
                Ok(Some((ControlBody::PromptCompleted { prompt_req_id, outcome }, _))) => {
                    let waiter = {
                        let mut completion = reader_completion.lock().expect("completion mutex poisoned");
                        match &*completion {
                            PromptCompletion::Pending { prompt_req_id: Some(id), .. } if *id == prompt_req_id => {
                                match std::mem::replace(&mut *completion, PromptCompletion::LocalIdle) {
                                    PromptCompletion::Pending { tx, .. } => Some(tx),
                                    _ => unreachable!(),
                                }
                            }
                            PromptCompletion::Adopted => {
                                // Serialize this decision with local turn activation:
                                // retained history must never clear a new turn's flag.
                                if !reader_prompt_in_flight.swap(false, AtomicOrdering::Relaxed) {
                                    debug!(
                                        target: "acp.protocol",
                                        session = %reader_session,
                                        "ignoring replayed PromptCompleted for a durable terminal"
                                    );
                                    continue;
                                }
                                if !terminal_claim.claim() {
                                    warn!(
                                        target: "acp.protocol",
                                        session = %reader_session,
                                        "runner reported PromptCompleted after the turn terminal was claimed"
                                    );
                                    continue;
                                }
                                None
                            }
                            _ => continue,
                        }
                    };
                    if let Some(tx) = waiter {
                        let _ = tx.send(outcome);
                    } else {
                        debug!(
                            target: "acp.protocol",
                            session = %reader_session,
                            "runner reported PromptCompleted for an adopted turn"
                        );
                        let reason = match prompt_outcome_to_response(outcome.clone()) {
                            Err(error) => {
                                if let Some(info) = classify_rate_limit_error(&error, None) {
                                    let _ = event_tx.send(Event::RateLimit { info }).await;
                                    "rate_limited".to_string()
                                } else {
                                    control_outcome_reason(&outcome)
                                }
                            }
                            Ok(_) => control_outcome_reason(&outcome),
                        };
                        let _ = event_tx.send(Event::Stopped { reason }).await;
                    }
                }
                // Reverse lane: served by the crate's request handlers. The
                // ordered dispatcher starts each handler before reading the
                // next frame, so SessionReady must publish its candidate first.
                Ok(Some((ControlBody::ServerCall {
                    call_id,
                    method,
                    params,
                }, _))) => {
                    let synthetic = {
                        let mut c = reader_correlation.lock().await;
                        let id = c.synthetic_id();
                        c.reverse.insert(id, call_id);
                        id
                    };
                    let line = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": synthetic,
                        "method": method,
                        "params": params,
                    });
                    if !shim_write_line(&reader_shim_write, &line).await {
                        return;
                    }
                }
                // Preserve producer bytes across the private SDK transport
                // without nesting or trusting any native accounting claim.
                Ok(Some((ControlBody::Notify { method, mut params }, wire_bytes))) => {
                    if method == "session/update" {
                        match &mut params {
                            serde_json::Value::Object(fields) => {
                                fields.insert(CONTROL_FRAME_BYTES_FIELD.into(), wire_bytes.into());
                            }
                            serde_json::Value::Array(fields) => fields.push(wire_bytes.into()),
                            _ => {}
                        }
                    }
                    let mut line = serde_json::json!({"jsonrpc": "2.0"});
                    line["method"] = method.into();
                    line["params"] = params;
                    if !shim_write_line(&reader_shim_write, &line).await {
                        return;
                    }
                }
                // Forward lane: resolve the crate's own pending request.
                Ok(Some((ControlBody::AgentResult { call_id, result }, _))) => {
                    let id = reader_correlation.lock().await.forward.remove(&call_id);
                    if let Some(id) = id {
                        let line = serde_json::json!({
                            "jsonrpc": "2.0", "id": id, "result": result,
                        });
                        if !shim_write_line(&reader_shim_write, &line).await {
                            return;
                        }
                    }
                }
                Ok(Some((ControlBody::AgentError { call_id, error }, _))) => {
                    let id = reader_correlation.lock().await.forward.remove(&call_id);
                    if let Some(id) = id {
                        let line = serde_json::json!({
                            "jsonrpc": "2.0", "id": id, "error": error,
                        });
                        if !shim_write_line(&reader_shim_write, &line).await {
                            return;
                        }
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
        }
        .await;
        *reader_completion.lock().expect("completion mutex poisoned") = PromptCompletion::LocalIdle;
        let mut shim_write = reader_shim_write.lock().await;
        let _ = shim_write.shutdown().await;
    });

    // Crate output: a line with `method` is a forward request, anything else
    // answers a reverse call.
    let pump_write = write_half.clone();
    let pump_shim_write = shim_write.clone();
    let pump_correlation = correlation.clone();
    let pump_session = session_label.clone();
    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(shim_read);
        let mut line = String::new();
        loop {
            line.clear();
            match tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line).await {
                Ok(0) => return,
                Ok(_) => {}
                Err(error) => {
                    debug!(
                        target: "acp.protocol",
                        session = %pump_session,
                        "shim transport read ended: {error}"
                    );
                    return;
                }
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                continue;
            };
            let mut forward = None;
            let frame = if let Some(method) = value.get("method").and_then(|method| method.as_str())
            {
                let Some(id) = value.get("id").cloned() else {
                    continue;
                };
                let call_id = pump_correlation.lock().await.forward_id();
                forward = Some((call_id, id));
                ControlBody::AgentCall {
                    call_id,
                    method: method.to_string(),
                    params: value
                        .get("params")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                }
            } else {
                let Some(synthetic) = value.get("id").and_then(|id| id.as_i64()) else {
                    continue;
                };
                let Some(call_id) = pump_correlation.lock().await.reverse.remove(&synthetic) else {
                    continue;
                };
                match value.get("error") {
                    Some(error) if !error.is_null() => ControlBody::ServerError {
                        call_id,
                        error: serde_json::from_value(error.clone()).unwrap_or_else(|_| {
                            control_protocol::JsonRpcError::new(
                                control_protocol::INTERNAL_ERROR,
                                "handler produced a malformed error",
                            )
                        }),
                    },
                    _ => ControlBody::ServerResult {
                        call_id,
                        result: value
                            .get("result")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                    },
                }
            };

            let wire = match control_protocol::encode_frame(&frame) {
                Ok(wire) => wire,
                Err(error) => {
                    if let Some((_, id)) = forward.take() {
                        let response = serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": id,
                            "error": {
                                "code": control_protocol::INTERNAL_ERROR,
                                "message": format!("request exceeds control transport capacity: {error}"),
                            },
                        });
                        if !shim_write_line(&pump_shim_write, &response).await {
                            return;
                        }
                        continue;
                    }
                    let call_id = match frame {
                        ControlBody::ServerResult { call_id, .. }
                        | ControlBody::ServerError { call_id, .. } => call_id,
                        _ => unreachable!("only server replies reach the reverse fallback"),
                    };
                    let fallback = ControlBody::ServerError {
                        call_id,
                        error: control_protocol::JsonRpcError::new(
                            control_protocol::INTERNAL_ERROR,
                            format!("daemon response exceeds control transport capacity: {error}"),
                        ),
                    };
                    match control_protocol::encode_frame(&fallback) {
                        Ok(wire) => wire,
                        Err(_) => return,
                    }
                }
            };

            if let Some((call_id, id)) = forward.as_ref() {
                pump_correlation
                    .lock()
                    .await
                    .forward
                    .insert(*call_id, id.clone());
            }
            let write_failed = {
                let mut writer = pump_write.lock().await;
                if control_protocol::write_encoded_frame(&mut *writer, &wire)
                    .await
                    .is_err()
                {
                    let _ = writer.shutdown().await;
                    true
                } else {
                    false
                }
            };
            if write_failed {
                if let Some((call_id, id)) = forward {
                    pump_correlation.lock().await.forward.remove(&call_id);
                    let response = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {
                            "code": control_protocol::DAEMON_GONE,
                            "message": "runner control transport closed",
                        },
                    });
                    let _ = shim_write_line(&pump_shim_write, &response).await;
                }
                return;
            }
        }
    });

    Ok((
        Arc::new(DaemonControlClient {
            ingress,
            write: write_half,
            handshake_rx: Mutex::new(hs_rx),
            sessions_established: AtomicU64::new(0),
            sessions_replayed: watch::Sender::new(0),
            completion,
            raw_fd,
        }),
        crate_side,
    ))
}

/// Known ACP stop reasons pass through verbatim; anything else ends as
/// `prompt_complete`.
pub(super) fn control_outcome_reason(
    outcome: &crate::acp::control_protocol::PromptOutcome,
) -> String {
    use crate::acp::control_protocol::PromptOutcome;
    match outcome {
        PromptOutcome::Completed {
            stop_reason: Some(r),
        } => match r.as_str() {
            // The adapter spells it both ways.
            "rate_limited" | "rate_limit" => "rate_limited".to_string(),
            "cancelled" | "max_tokens" | "refusal" | "max_turn_requests" => r.clone(),
            _ => "prompt_complete".to_string(),
        },
        _ => "prompt_complete".to_string(),
    }
}

/// Same result shape as the crate `send_request` it replaces.
pub(super) async fn establish_session_v3<Resp: serde::de::DeserializeOwned>(
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

/// Maps a runner outcome onto the prompt loop's direct-stdio result shape.
/// Errors keep code and data so rate limits still classify; an abort ends
/// the turn as `end_turn`.
pub(super) fn prompt_outcome_to_response(
    outcome: control_protocol::PromptOutcome,
) -> Result<PromptResponse, agent_client_protocol::Error> {
    use control_protocol::PromptOutcome;
    // `PromptResponse` is `#[non_exhaustive]`, so it is deserialized.
    let build = |stop: &str| {
        serde_json::from_value::<PromptResponse>(serde_json::json!({ "stopReason": stop }))
            .map_err(|e| acp_internal_error(format!("build prompt response: {e}")))
    };
    match outcome {
        PromptOutcome::Completed { stop_reason } => {
            build(stop_reason.as_deref().unwrap_or("end_turn"))
        }
        PromptOutcome::Aborted => build("end_turn"),
        PromptOutcome::Error {
            code,
            message,
            data,
        } => {
            let mut error = agent_client_protocol::Error::new(code, message);
            error.data = data;
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::control_protocol::PromptOutcome;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    use tokio::net::{UnixListener, UnixStream};

    fn bind(tmp: &tempfile::TempDir) -> (std::path::PathBuf, UnixListener) {
        let control = crate::process::worker::control_socket_sibling(&tmp.path().join("s.sock"));
        let listener = UnixListener::bind(&control).unwrap();
        (control, listener)
    }

    /// Accept one daemon and greet it as runner `session`.
    async fn accept_hello(listener: &UnixListener, session: &str, version: u32) -> UnixStream {
        let (mut peer, _) = listener.accept().await.unwrap();
        let hello = ControlBody::Hello {
            control_protocol_version: version,
            session_id: session.into(),
        };
        let _ = control_protocol::write_frame(&mut peer, &hello).await;
        peer
    }

    async fn connect(
        control: &std::path::Path,
        session: &str,
        event_tx: mpsc::Sender<Event>,
        terminal: Arc<TerminalClaim>,
        in_flight: bool,
    ) -> anyhow::Result<(Arc<DaemonControlClient>, tokio::io::DuplexStream)> {
        let in_flight = Arc::new(AtomicBool::new(in_flight));
        connect_runner_control_v3(control, event_tx, session.into(), terminal, in_flight).await
    }

    struct PromptControlPeer {
        client: Arc<DaemonControlClient>,
        peer: UnixStream,
        transport: BufReader<tokio::io::DuplexStream>,
        events: mpsc::Receiver<Event>,
        terminal: Arc<TerminalClaim>,
        in_flight: Arc<AtomicBool>,
    }

    impl Drop for PromptControlPeer {
        fn drop(&mut self) {
            self.client.shutdown();
        }
    }

    impl PromptControlPeer {
        async fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let (socket, listener) = bind(&tmp);
            let terminal = Arc::new(TerminalClaim::new());
            let in_flight = Arc::new(AtomicBool::new(true));
            let (event_tx, events) = mpsc::channel(8);
            let (client, peer) = tokio::join!(
                connect_runner_control_v3(
                    &socket,
                    event_tx,
                    "prompt".into(),
                    terminal.clone(),
                    in_flight.clone(),
                ),
                async {
                    let mut peer = accept_hello(
                        &listener,
                        "prompt",
                        control_protocol::CONTROL_PROTOCOL_VERSION,
                    )
                    .await;
                    assert!(matches!(
                        control_protocol::read_frame(&mut peer).await.unwrap(),
                        Some(ControlBody::Attach { .. })
                    ));
                    peer
                }
            );
            let (client, transport) = client.unwrap();
            Self {
                client,
                peer,
                transport: BufReader::new(transport),
                events,
                terminal,
                in_flight,
            }
        }

        async fn send(&mut self, frame: ControlBody) {
            control_protocol::write_frame(&mut self.peer, &frame)
                .await
                .unwrap();
        }

        async fn prompt(&mut self) -> oneshot::Receiver<PromptOutcome> {
            let rx = self.client.prompt(serde_json::json!({})).await;
            assert!(matches!(
                control_protocol::read_frame(&mut self.peer).await.unwrap(),
                Some(ControlBody::Prompt { .. })
            ));
            rx
        }

        async fn completed(&mut self, prompt_req_id: i64, reason: &str) {
            let outcome = end(reason);
            self.send(ControlBody::PromptCompleted {
                prompt_req_id,
                outcome,
            })
            .await;
        }

        /// The reader forwards Notify only after handling every earlier frame,
        /// so this is a deterministic barrier.
        async fn drain(&mut self) {
            self.send(ControlBody::Notify {
                method: "test/barrier".into(),
                params: serde_json::json!({}),
            })
            .await;
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(2), self.transport.read_line(&mut line))
                .await
                .expect("control reader must reach the notification barrier")
                .unwrap();
            let notification: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(notification["method"], "test/barrier");
        }

        fn assert_local_terminal_ownership(&mut self) {
            assert!(self.in_flight.load(AtomicOrdering::Relaxed));
            assert!(!self.terminal.claimed());
            assert!(matches!(
                self.events.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
        }
    }

    fn end(reason: &str) -> PromptOutcome {
        PromptOutcome::Completed {
            stop_reason: Some(reason.into()),
        }
    }

    #[tokio::test]
    async fn historical_completion_old_first_preserves_local_waiter() {
        let mut peer = PromptControlPeer::new().await;
        let mut completion = peer.prompt().await;
        // History read before and after the runner assigns the new id must
        // resolve neither the waiter nor an adopted terminal.
        peer.completed(7, "cancelled").await;
        peer.drain().await;
        assert_eq!(
            completion.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        );
        peer.assert_local_terminal_ownership();
        peer.send(ControlBody::PromptStarted { prompt_req_id: 9 })
            .await;
        peer.completed(7, "cancelled").await;
        peer.drain().await;
        assert_eq!(
            completion.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        );
        peer.assert_local_terminal_ownership();
        peer.completed(9, "end_turn").await;
        peer.drain().await;
        assert_eq!(completion.await.unwrap(), end("end_turn"));
        peer.assert_local_terminal_ownership();
    }

    #[tokio::test]
    async fn historical_completion_new_first_preserves_local_terminal_ownership() {
        let mut peer = PromptControlPeer::new().await;
        let completion = peer.prompt().await;
        peer.send(ControlBody::PromptStarted { prompt_req_id: 9 })
            .await;
        peer.completed(9, "end_turn").await;
        peer.completed(7, "cancelled").await;
        peer.drain().await;
        peer.assert_local_terminal_ownership();
        assert_eq!(completion.await.unwrap(), end("end_turn"));
    }

    #[tokio::test]
    async fn duplicate_local_prompt_preserves_first_waiter_and_sends_no_prompt() {
        let mut peer = PromptControlPeer::new().await;
        let completion = peer.prompt().await;
        let mut duplicate = peer
            .client
            .prompt(serde_json::json!({"duplicate": true}))
            .await;
        assert!(matches!(
            duplicate.try_recv().unwrap(),
            PromptOutcome::Error { code, .. } if i64::from(code) == control_protocol::INTERNAL_ERROR
        ));
        // Cancel arriving next proves the duplicate sent no Prompt frame.
        peer.client.cancel().await;
        assert!(matches!(
            control_protocol::read_frame(&mut peer.peer).await.unwrap(),
            Some(ControlBody::Cancel)
        ));
        peer.send(ControlBody::PromptStarted { prompt_req_id: 9 })
            .await;
        peer.completed(9, "cancelled").await;
        peer.drain().await;
        assert_eq!(completion.await.unwrap(), end("cancelled"));
        peer.assert_local_terminal_ownership();
    }

    #[tokio::test]
    async fn local_activation_rejects_history_before_waiter_registration() {
        let mut peer = PromptControlPeer::new().await;
        peer.client.supersede_adopted_turn();
        peer.completed(7, "cancelled").await;
        peer.drain().await;
        peer.assert_local_terminal_ownership();
    }

    /// An oversized reverse response becomes a bounded error and the
    /// connection stays usable.
    #[tokio::test]
    async fn oversized_reverse_reply_becomes_error_without_poisoning_connection() {
        use tokio::io::AsyncWriteExt;
        let tmp = tempfile::tempdir().unwrap();
        let (control, listener) = bind(&tmp);
        let fake = tokio::spawn(async move {
            let stream = accept_hello(
                &listener,
                "oversize",
                control_protocol::CONTROL_PROTOCOL_VERSION,
            )
            .await;
            let (mut read, mut write) = stream.into_split();
            let _ = control_protocol::read_frame(&mut read).await.unwrap();
            for call_id in [41, 42] {
                let call = ControlBody::ServerCall {
                    call_id,
                    method: "fs/read_text_file".into(),
                    params: serde_json::json!({}),
                };
                control_protocol::write_frame(&mut write, &call)
                    .await
                    .unwrap();
                let reply = control_protocol::read_frame(&mut read)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(match (call_id, reply) {
                    (41, ControlBody::ServerError { call_id: 41, error }) =>
                        error.code == control_protocol::INTERNAL_ERROR,
                    (
                        42,
                        ControlBody::ServerResult {
                            call_id: 42,
                            result,
                        },
                    ) => result == serde_json::json!({"ok": true}),
                    _ => false,
                });
            }
        });

        let (event_tx, _) = mpsc::channel::<Event>(1);
        let (_, crate_side) = connect(
            &control,
            "oversize",
            event_tx,
            Arc::new(TerminalClaim::new()),
            false,
        )
        .await
        .unwrap();
        let (read, mut write) = tokio::io::split(crate_side);
        let mut read = BufReader::new(read);
        let huge = "x".repeat(control_protocol::MAX_CONTROL_FRAME_BYTES as usize);
        for result in [
            serde_json::json!({"content": huge}),
            serde_json::json!({"ok": true}),
        ] {
            let mut line = String::new();
            read.read_line(&mut line).await.unwrap();
            let call: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            let mut response = serde_json::to_vec(&serde_json::json!({
                "jsonrpc": "2.0", "id": call["id"], "result": result,
            }))
            .unwrap();
            response.push(b'\n');
            write.write_all(&response).await.unwrap();
        }
        fake.await.unwrap();
    }

    #[tokio::test]
    async fn runner_control_eof_closes_transport_and_cancels_prompt() {
        let tmp = tempfile::tempdir().unwrap();
        let (control, listener) = bind(&tmp);
        let fake = tokio::spawn(async move {
            let mut peer =
                accept_hello(&listener, "eof", control_protocol::CONTROL_PROTOCOL_VERSION).await;
            assert!(matches!(
                control_protocol::read_frame(&mut peer).await.unwrap(),
                Some(ControlBody::Attach { .. })
            ));
            assert!(matches!(
                control_protocol::read_frame(&mut peer).await.unwrap(),
                Some(ControlBody::Prompt { .. })
            ));
        });
        let (client, crate_side) = connect(
            &control,
            "eof",
            mpsc::channel(1).0,
            Arc::new(TerminalClaim::new()),
            false,
        )
        .await
        .unwrap();
        let completion = client.prompt(serde_json::json!({})).await;
        fake.await.unwrap();
        assert!(tokio::time::timeout(Duration::from_secs(1), completion)
            .await
            .expect("prompt completion must resolve after control EOF")
            .is_err());
        let (mut crate_read, _crate_write) = tokio::io::split(crate_side);
        let mut byte = [0_u8; 1];
        let read = tokio::time::timeout(Duration::from_secs(1), crate_read.read(&mut byte))
            .await
            .expect("crate transport must observe control EOF");
        assert_eq!(read.unwrap(), 0);
    }

    #[test]
    fn prompt_error_preserves_code_message_and_data() {
        use agent_client_protocol::ErrorCode;
        for (code, expected) in [
            (-32601, ErrorCode::MethodNotFound),
            (-32000, ErrorCode::AuthRequired),
            (42, ErrorCode::Other(42)),
        ] {
            let data = serde_json::json!({"detail": "kept"});
            let error = prompt_outcome_to_response(PromptOutcome::Error {
                code,
                message: "boom".into(),
                data: Some(data.clone()),
            })
            .unwrap_err();
            assert_eq!(
                (error.code, error.message.as_str(), error.data),
                (expected, "boom", Some(data))
            );
        }
    }

    #[tokio::test]
    async fn attached_session_only_resets_for_missing_session_errors() {
        use crate::acp::acp_client::AcpClient;
        use crate::acp::state::AcpSessionId;

        for (message, should_reset) in [
            ("Unsupported ACP session", true),
            ("Unsupported session mode", false),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let socket = tmp.path().join("s.sock");
            let (_, listener) = bind(&tmp);
            let runner = async {
                let mut peer = accept_hello(
                    &listener,
                    "resume",
                    control_protocol::CONTROL_PROTOCOL_VERSION,
                )
                .await;
                while let Some(frame) = control_protocol::read_frame(&mut peer).await.unwrap() {
                    let reply = match frame {
                        ControlBody::Attach { .. } => continue,
                        ControlBody::Initialize { .. } => ControlBody::Initialized {
                            result: serde_json::json!({
                                "protocolVersion": 1, "agentCapabilities": {}
                            }),
                        },
                        ControlBody::ResumeSession => ControlBody::SessionReady {
                            acp_session_id: "sid-stored".into(),
                            result: serde_json::json!({}),
                        },
                        ControlBody::Prompt { request } => {
                            assert_eq!(request["sessionId"], "sid-stored");
                            let started = ControlBody::PromptStarted { prompt_req_id: 1 };
                            control_protocol::write_frame(&mut peer, &started)
                                .await
                                .unwrap();
                            ControlBody::PromptCompleted {
                                prompt_req_id: 1,
                                outcome: PromptOutcome::Error {
                                    code: -32603,
                                    message: message.into(),
                                    data: None,
                                },
                            }
                        }
                        frame => panic!("unexpected resumed-session request: {frame:?}"),
                    };
                    control_protocol::write_frame(&mut peer, &reply)
                        .await
                        .unwrap();
                }
            };
            let daemon = async {
                let mut client = AcpClient::attach(
                    socket,
                    tmp.path().into(),
                    vec![],
                    "sid-stored".into(),
                    false,
                    AcpSessionId("resume".into()),
                    None,
                    "codex".into(),
                    None,
                )
                .await
                .unwrap();
                client.send_prompt("continue", &[]).await.unwrap();
                let mut recovery = Vec::new();
                while let Some(event) = client.next_event().await {
                    match event {
                        Event::SessionContextReset { .. } => recovery.push("reset"),
                        Event::Stopped { reason } => {
                            assert_eq!(reason, "stored_session_rejected");
                            recovery.push("stopped");
                        }
                        Event::AgentStartupError { message: error } => {
                            assert!(error.contains(message), "{error}");
                            recovery.push("error");
                        }
                        _ => {}
                    }
                }
                let want = if should_reset {
                    vec!["reset", "stopped"]
                } else {
                    vec!["error"]
                };
                assert_eq!(recovery, want, "{message}");
                let _ = client.shutdown().await;
            };
            tokio::time::timeout(Duration::from_secs(10), async {
                tokio::join!(runner, daemon);
            })
            .await
            .expect("attach recovery must finish and close its control socket");
        }
    }

    #[tokio::test]
    async fn native_identity_reattach_uses_runner_id_and_drains_adopted_updates() {
        use crate::acp::acp_client::AcpClient;
        use crate::acp::state::AcpSessionId;
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("native.sock");
        let control = crate::process::worker::control_socket_sibling(&socket);
        let listener = tokio::net::UnixListener::bind(control).unwrap();
        let own_file = temp.path().join("own");
        let foreign_file = temp.path().join("foreign");
        let (callbacks_done, callbacks_ready) = oneshot::channel();
        let runner = async {
            let (mut peer, _) = listener.accept().await.unwrap();
            control_protocol::write_frame(
                &mut peer,
                &ControlBody::Hello {
                    control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
                    session_id: "native-resume".into(),
                },
            )
            .await
            .unwrap();
            let mut callbacks_done = Some(callbacks_done);
            let mut replies = 0;
            while let Some(frame) = control_protocol::read_frame(&mut peer).await.unwrap() {
                match frame {
                    ControlBody::Attach { .. } => {}
                    ControlBody::Initialize { .. } => {
                        control_protocol::write_frame(&mut peer, &ControlBody::Initialized {
                            result: serde_json::json!({"protocolVersion":1,"agentCapabilities":{}}),
                        }).await.unwrap();
                    }
                    ControlBody::ResumeSession => {
                        for index in 0..200 {
                            control_protocol::write_frame(&mut peer, &ControlBody::Notify {
                                method: "session/update".into(),
                                params: serde_json::json!({"sessionId":"actual","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":format!("own-{index}")}}}),
                            }).await.unwrap();
                        }
                        control_protocol::write_frame(&mut peer, &ControlBody::Notify {
                            method: "session/update".into(),
                            params: serde_json::json!({"sessionId":"stale","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"FOREIGN"}}}),
                        }).await.unwrap();
                        let update = |text: &str| {
                            serde_json::json!({
                                "sessionUpdate":"agent_message_chunk",
                                "content":{"type":"text","text":text}
                            })
                        };
                        // Invalid native shapes must not become valid by mistaking
                        // a native field/tail for the shim-owned accounting marker.
                        for params in [
                            serde_json::Value::Null,
                            serde_json::json!(7),
                            serde_json::json!([]),
                            serde_json::json!({"update":update("INVALID")}),
                            serde_json::json!({"notification":{"sessionId":"actual","update":update("INVALID")},"__aoe_control_frame_bytes":0}),
                            serde_json::json!(["actual", update("INVALID"), null, 0]),
                        ] {
                            control_protocol::write_frame(
                                &mut peer,
                                &ControlBody::Notify {
                                    method: "session/update".into(),
                                    params,
                                },
                            )
                            .await
                            .unwrap();
                        }
                        // Positional params and the existing JSON nesting ceiling
                        // survive the private accounting transport unchanged.
                        let deep = (0..124).fold(serde_json::Value::Null, |value, _| {
                            serde_json::Value::Array(vec![value])
                        });
                        for params in [
                            serde_json::json!(["actual",update("own-200"),{"deep":deep}]),
                            serde_json::json!({"sessionId":"actual","update":update("own-201"),"__aoe_control_frame_bytes":{"untrusted":true}}),
                            serde_json::json!({"sessionId":"actual","update":update("own-202"),"__aoe_control_frame_bytes":u64::MAX}),
                            serde_json::json!(["actual", update("own-203")]),
                        ] {
                            control_protocol::write_frame(
                                &mut peer,
                                &ControlBody::Notify {
                                    method: "session/update".into(),
                                    params,
                                },
                            )
                            .await
                            .unwrap();
                        }
                        control_protocol::write_frame(
                            &mut peer,
                            &ControlBody::SessionReady {
                                acp_session_id: "actual".into(),
                                result: serde_json::json!({}),
                            },
                        )
                        .await
                        .unwrap();
                        for (call_id, session_id, path) in
                            [(1, "actual", &own_file), (2, "stale", &foreign_file)]
                        {
                            control_protocol::write_frame(&mut peer, &ControlBody::ServerCall {
                                call_id, method: "fs/write_text_file".into(),
                                params: serde_json::json!({"sessionId":session_id,"path":path,"content":"owned"}),
                            }).await.unwrap();
                        }
                    }
                    ControlBody::ServerResult { call_id, .. } => {
                        assert_eq!(call_id, 1);
                        replies += 1;
                    }
                    ControlBody::ServerError { call_id, error } => {
                        assert_eq!(call_id, 2);
                        assert_eq!(error.code, -32602);
                        replies += 1;
                    }
                    other => panic!("unexpected frame {other:?}"),
                }
                if replies == 2 {
                    if let Some(done) = callbacks_done.take() {
                        done.send(()).unwrap();
                    }
                }
            }
        };
        let daemon = async {
            let mut client = AcpClient::attach(
                socket,
                temp.path().into(),
                vec![],
                "stale".into(),
                true,
                AcpSessionId("native-resume".into()),
                None,
                "codex".into(),
                None,
            )
            .await
            .unwrap();
            let mut chunks = Vec::new();
            while chunks.len() < 204 {
                match client.next_event().await.unwrap() {
                    Event::AgentMessageChunk { text, .. } => chunks.push(text),
                    Event::AgentStartupError { message } => panic!("{message}"),
                    _ => {}
                }
            }
            callbacks_ready.await.unwrap();
            client.shutdown().await.unwrap();
            assert_eq!(
                chunks,
                (0..204)
                    .map(|index| format!("own-{index}"))
                    .collect::<Vec<_>>()
            );
            assert_eq!(std::fs::read_to_string(&own_file).unwrap(), "owned");
            assert!(!foreign_file.exists());
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(runner, daemon);
        })
        .await
        .unwrap();
    }
    #[tokio::test]
    async fn native_identity_reattach_preserves_a_full_wire_byte_backlog() {
        use crate::acp::acp_client::AcpClient;
        use crate::acp::state::AcpSessionId;
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("byte-backlog.sock");
        let control = crate::process::worker::control_socket_sibling(&socket);
        let listener = tokio::net::UnixListener::bind(control).unwrap();
        let own_file = temp.path().join("own");
        let (callback_done, callback_ready) = oneshot::channel();
        let runner = async {
            let (mut peer, _) = listener.accept().await.unwrap();
            control_protocol::write_frame(
                &mut peer,
                &ControlBody::Hello {
                    control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
                    session_id: "byte-backlog".into(),
                },
            )
            .await
            .unwrap();
            let mut callback_done = Some(callback_done);
            while let Some(frame) = control_protocol::read_frame(&mut peer).await.unwrap() {
                match frame {
                    ControlBody::Attach { .. } => {}
                    ControlBody::Initialize { .. } => {
                        control_protocol::write_frame(&mut peer, &ControlBody::Initialized {
                            result: serde_json::json!({"protocolVersion":1,"agentCapabilities":{}}),
                        }).await.unwrap();
                    }
                    ControlBody::ResumeSession => {
                        let mut wire_total = 0;
                        for id in ["a", "b", "c"] {
                            // Integer priorities become typed f64 values. Their later
                            // encoding can exceed the original valid runner backlog.
                            let body = ControlBody::Notify {
                                method: "session/update".into(),
                                params: serde_json::json!({
                                    "sessionId":"s",
                                    "update":{"sessionUpdate":"tool_call","toolCallId":id,"title":"t",
                                        "content":vec![serde_json::json!({"type":"content","content":{
                                            "type":"text","text":"x","annotations":{"priority":1}}});100]},
                                    "_meta":{"pad":"x".repeat(44_730_469)}
                                }),
                            };
                            let frame = control_protocol::encode_frame(&body).unwrap();
                            wire_total += frame.len();
                            assert!(wire_total <= control_protocol::MAX_CONTROL_QUEUE_BYTES);
                            control_protocol::write_encoded_frame(&mut peer, &frame)
                                .await
                                .unwrap();
                        }
                        // SDK dispatch is ordered: this denied callback proves all
                        // three updates reached pending admission before readiness.
                        control_protocol::write_frame(
                            &mut peer,
                            &ControlBody::ServerCall {
                                call_id: 1,
                                method: "fs/read_text_file".into(),
                                params: serde_json::json!({"sessionId":"unknown","path":own_file}),
                            },
                        )
                        .await
                        .unwrap();
                    }
                    ControlBody::ServerError { call_id: 1, error } => {
                        assert_eq!(error.code, -32602);
                        control_protocol::write_frame(
                            &mut peer,
                            &ControlBody::SessionReady {
                                acp_session_id: "s".into(),
                                result: serde_json::json!({}),
                            },
                        )
                        .await
                        .unwrap();
                        control_protocol::write_frame(&mut peer, &ControlBody::ServerCall {
                            call_id: 2, method: "fs/write_text_file".into(),
                            params: serde_json::json!({"sessionId":"s","path":own_file,"content":"owned"}),
                        }).await.unwrap();
                    }
                    ControlBody::ServerResult { call_id: 2, .. } => {
                        callback_done.take().unwrap().send(()).unwrap();
                    }
                    other => panic!("unexpected frame {other:?}"),
                }
            }
        };
        let daemon = async {
            let mut client = AcpClient::attach(
                socket,
                temp.path().into(),
                vec![],
                "stale".into(),
                true,
                AcpSessionId("byte-backlog".into()),
                None,
                "codex".into(),
                None,
            )
            .await
            .expect("producer-admitted backlog must attach");
            let mut calls = Vec::new();
            while calls.len() < 3 {
                match client.next_event().await.unwrap() {
                    Event::ToolCallStarted { tool_call } => calls.push(tool_call.id),
                    Event::AgentStartupError { message } => panic!("{message}"),
                    _ => {}
                }
            }
            callback_ready.await.unwrap();
            client.shutdown().await.unwrap();
            assert_eq!(calls, ["a", "b", "c"]);
            assert_eq!(std::fs::read_to_string(&own_file).unwrap(), "owned");
        };
        tokio::time::timeout(std::time::Duration::from_secs(120), async {
            tokio::join!(runner, daemon);
        })
        .await
        .unwrap();
    }

    /// A waiterless completion for an adopted turn publishes its terminal,
    /// in order after any rate-limit metadata, and hands idle ownership back.
    #[tokio::test]
    async fn adopted_completion_publishes_terminal() {
        let rate_limited = PromptOutcome::Error {
            code: -32000,
            message: "rate limit exceeded".into(),
            data: Some(serde_json::json!({"errorKind": "rate_limit"})),
        };
        for (outcome, want) in [
            (end("end_turn"), vec!["stopped:prompt_complete"]),
            (rate_limited, vec!["rate_limit", "stopped:rate_limited"]),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let (control, listener) = bind(&tmp);
            let fake = tokio::spawn(async move {
                let mut peer =
                    accept_hello(&listener, "s", control_protocol::CONTROL_PROTOCOL_VERSION).await;
                let _ = control_protocol::read_frame(&mut peer).await;
                let frame = ControlBody::PromptCompleted {
                    prompt_req_id: 5,
                    outcome,
                };
                control_protocol::write_frame(&mut peer, &frame)
                    .await
                    .unwrap();
                peer
            });
            let (event_tx, mut event_rx) = mpsc::channel::<Event>(8);
            let guard = Arc::new(TerminalClaim::new());
            // As a stranded prompt loop would leave it.
            let prompt_in_flight = Arc::new(AtomicBool::new(true));
            let (client, _transport) = connect_runner_control_v3(
                &control,
                event_tx,
                "s".into(),
                guard.clone(),
                prompt_in_flight.clone(),
            )
            .await
            .unwrap();
            // Holding the runner end open lets the reader deliver before EOF.
            let _peer = fake.await.unwrap();
            let mut got = Vec::new();
            while got.len() < want.len() {
                let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
                    .await
                    .expect("timed out waiting for the adopted terminal")
                    .expect("event channel closed");
                got.push(match event {
                    Event::RateLimit { .. } => "rate_limit".to_string(),
                    Event::Stopped { reason } => format!("stopped:{reason}"),
                    other => panic!("unexpected event {other:?}"),
                });
            }
            assert_eq!(got, want);
            assert!(guard.claimed());
            assert!(!prompt_in_flight.load(AtomicOrdering::Relaxed));
            client.shutdown();
        }
    }

    /// An untrusted Hello, an absent socket, or a hard dial error yields no
    /// client, no terminal claim, and no event.
    #[tokio::test]
    #[serial_test::serial]
    async fn failed_attach_leaves_guard_unclaimed() {
        let _env = crate::session::test_support::EnvGuard::set(&[(
            "AOE_ACP_RUNNER_SOCKET_TIMEOUT_MS",
            "150",
        )]);
        let tmp = tempfile::tempdir().unwrap();
        let (mismatch, listener) = bind(&tmp);
        let fake = tokio::spawn(async move { accept_hello(&listener, "s", 999).await });
        let absent =
            crate::process::worker::control_socket_sibling(&tmp.path().join("absent.sock"));
        let overlong = tmp.path().join("x".repeat(200));
        for (path, expected, unexpected) in [
            (&mismatch, "runner Hello mismatch", None),
            (&absent, "timed out attaching runner control socket", None),
            (
                &overlong,
                "connect runner control socket",
                Some("timed out attaching"),
            ),
        ] {
            let (event_tx, mut event_rx) = mpsc::channel::<Event>(8);
            let guard = Arc::new(TerminalClaim::new());
            let Err(error) = connect(path, "s", event_tx, guard.clone(), false).await else {
                panic!("attach to {} must fail", path.display());
            };
            let message = format!("{error:#}");
            assert!(message.contains(expected), "{message}");
            assert!(unexpected.is_none_or(|u| !message.contains(u)), "{message}");
            assert!(!guard.claimed());
            assert!(event_rx.try_recv().is_err());
        }
        let _ = fake.await;
    }

    /// The runner's session reply precedes the replay it follows; a load must
    /// not count as replayed until the crate has applied that replay (#4016).
    #[tokio::test]
    async fn session_replayed_waits_for_replay_sent_after_the_reply() {
        use super::super::session_identity::SessionIngressNotification;
        use agent_client_protocol::{ByteStreams, Client};
        use futures_util::FutureExt as _;
        use std::sync::atomic::AtomicBool;
        use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

        let tmp = tempfile::tempdir().unwrap();
        let control =
            crate::process::worker::control_socket_sibling(&tmp.path().join("replayed.sock"));
        let listener = tokio::net::UnixListener::bind(&control).unwrap();
        let runner = async {
            let (mut peer, _) = listener.accept().await.unwrap();
            control_protocol::write_frame(
                &mut peer,
                &ControlBody::Hello {
                    control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
                    session_id: "replayed".into(),
                },
            )
            .await
            .unwrap();
            while let Some(frame) = control_protocol::read_frame(&mut peer).await.unwrap() {
                match frame {
                    ControlBody::Attach { .. } => {}
                    ControlBody::EstablishSession { .. } => {
                        for body in [
                            ControlBody::SessionReady {
                                acp_session_id: "s".into(),
                                result: serde_json::json!({}),
                            },
                            ControlBody::Notify {
                                method: "session/update".into(),
                                params: serde_json::json!({"sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"replayed"}}}),
                            },
                            ControlBody::Notify {
                                method: "_aoe/session_replayed".into(),
                                params: serde_json::json!({}),
                            },
                        ] {
                            control_protocol::write_frame(&mut peer, &body)
                                .await
                                .unwrap();
                        }
                    }
                    other => panic!("unexpected frame {other:?}"),
                }
            }
        };
        let daemon = async {
            let (client, crate_side) = connect_runner_control_v3(
                &control,
                mpsc::channel::<Event>(1).0,
                "replayed".into(),
                Arc::new(TerminalClaim::new()),
                Arc::new(AtomicBool::new(false)),
            )
            .await
            .unwrap();
            let (entered_tx, entered_rx) = oneshot::channel::<()>();
            let (release_tx, release_rx) = oneshot::channel::<()>();
            let gate = Arc::new(Mutex::new(Some((entered_tx, release_rx))));
            let applied = Arc::new(AtomicBool::new(false));
            let marker_client = client.clone();
            let (read, write) = tokio::io::split(crate_side);
            Client
                .builder()
                // One handler for both, as the connection registers them.
                .on_receive_notification(
                    {
                        let applied = applied.clone();
                        move |notification: SessionIngressNotification, _cx| {
                            let gate = gate.clone();
                            let applied = applied.clone();
                            let control = marker_client.clone();
                            async move {
                                match notification {
                                    SessionIngressNotification::Replayed(marker) => {
                                        control.mark_session_replayed(marker);
                                    }
                                    SessionIngressNotification::Update(_) => {
                                        if let Some((entered, release)) = gate.lock().await.take() {
                                            entered.send(()).unwrap();
                                            release.await.unwrap();
                                        }
                                        applied.store(true, AtomicOrdering::Relaxed);
                                    }
                                }
                                Ok(())
                            }
                        }
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .connect_with(
                    ByteStreams::new(write.compat_write(), read.compat()),
                    |_connection| async move {
                        client
                            .establish_session("session/load", serde_json::json!({}))
                            .await
                            .unwrap();
                        entered_rx.await.unwrap();
                        let replayed = client.session_replayed();
                        tokio::pin!(replayed);
                        assert!(
                            replayed.as_mut().now_or_never().is_none(),
                            "a load must not count as replayed mid-replay"
                        );
                        release_tx.send(()).unwrap();
                        replayed.await;
                        assert!(applied.load(AtomicOrdering::Relaxed));
                        client.shutdown();
                        Ok(())
                    },
                )
                .await
                .unwrap();
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(runner, daemon);
        })
        .await
        .unwrap();
    }
}
