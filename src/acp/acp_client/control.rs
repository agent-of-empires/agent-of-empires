//! The v3 runner control socket: connecting, establishing a session, and
//! routing ACP frames over it.

use crate::acp::control_protocol::{self, ControlBody};
use crate::acp::state::Event;
use agent_client_protocol::schema::v1::PromptResponse;
use std::collections::HashMap;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::Ordering as AtomicOrdering;
use std::sync::Arc;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, info, warn};

use super::errors::{acp_error_from_value, acp_internal_error, AcpError};
use super::lifecycle::TerminalClaim;
use super::rate_limit::classify_rate_limit_error;
use super::runner::runner_socket_deadline;

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
/// Bidirectional client for a v3 runner control socket. The runner owns the
/// handshake and turn; the daemon drives them over this channel.
///
/// `initialize` / `session/*` responses arrive sequentially on
/// `handshake_rx`; a turn's `PromptCompleted` is routed to the oneshot in
/// `completion` when a prompt is awaiting, else (an adopted turn on a
/// mid-flight resume, where this daemon never issued the prompt) the reader
/// CAS-claims the terminal guard and fires `Stopped` so the UI clears.
pub(super) struct DaemonControlClient {
    write: Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    handshake_rx: Mutex<mpsc::Receiver<ControlBody>>,
    completion: Arc<std::sync::Mutex<Option<oneshot::Sender<control_protocol::PromptOutcome>>>>,
    raw_fd: RawFd,
}

/// Correlation state for the control-channel transport shim (#2977).
///
/// With `<id>.sock` retired there is no byte stream for the crate
/// connection to speak over, but everything it does on the runner path
/// besides the handshake and the turn is still ordinary ACP: nine incoming
/// request handlers, the `session/update` notification handler, and five
/// outgoing methods the runner does not own. Rather than rewrite the
/// 2,800-line connection task around a second driver, the shim gives the
/// crate a synthetic in-process duplex and translates at the boundary. The
/// crate is unchanged, the direct-stdio path is untouched, and the relay
/// socket is still gone.
///
/// Two independent id spaces meet here, and neither side may see the
/// other's:
///
/// - **Reverse** (runner -> crate): the runner's `call_id` is mapped onto a
///   synthetic integer JSON-RPC id, because the crate needs an id to route
///   a request to a handler and hand back a `Responder`.
/// - **Forward** (crate -> runner): the crate allocates its own (UUID
///   string) request id, which is mapped onto a `call_id` for the runner.
#[derive(Default)]
struct ShimCorrelation {
    /// Synthetic JSON-RPC id -> the runner's `call_id`, for answering a
    /// reverse call once a crate handler has produced a response.
    reverse: HashMap<i64, u64>,
    /// The runner's forward `call_id` -> the crate's own request id, for
    /// handing an `AgentResult` back to the waiting `send_request`.
    forward: HashMap<u64, serde_json::Value>,
    /// Allocator for synthetic reverse ids. Negative and descending so a
    /// synthetic id can never be mistaken for one the crate minted, which
    /// would silently cross the two lanes.
    next_synthetic: i64,
}

/// Process-wide seed for forward `call_id`s, so the space is monotonic
/// across daemon connections rather than restarting at zero on each attach.
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

/// Serialize one ndjson line into the crate-facing duplex. Returns false
/// once the crate side has hung up.
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

    /// Run the ACP `initialize` the runner owns; returns the raw result
    /// value to deserialize into `InitializeResponse`. A `HandshakeFailed`
    /// is surfaced as the reconstructed crate error so the caller propagates
    /// the same AgentStartupError (with data.details) as direct stdio.
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

    /// Issue a turn: register the completion oneshot, send the `Prompt`
    /// frame, and return the receiver the prompt loop awaits. The runner
    /// assigns the `session/prompt` id and reports `PromptCompleted`.
    pub(super) async fn prompt(
        &self,
        request: serde_json::Value,
    ) -> oneshot::Receiver<control_protocol::PromptOutcome> {
        let (tx, rx) = oneshot::channel();
        let displaced = self
            .completion
            .lock()
            .expect("completion mutex poisoned")
            .replace(tx);
        // Installing over an unresolved waiter drops the previous prompt's
        // receiver, so that turn's loop sees `Err -> Aborted` instead of its
        // real outcome. It should be impossible (one prompt is in flight at a
        // time) which is exactly why it is worth a line when it happens: a
        // stranded prompt future is the leading suspect for a session that
        // keeps rendering Running with no terminal event. See #3190.
        if displaced.is_some() {
            warn!(
                target: "acp.protocol",
                "installing a prompt completion waiter over an unresolved one"
            );
        } else {
            debug!(target: "acp.protocol", "prompt completion waiter installed");
        }
        if self.send(ControlBody::Prompt { request }).await.is_err() {
            // Write failed: drop the parked sender so `rx` resolves to Err ->
            // Aborted immediately instead of hanging until the cancel /
            // orphan watchdog eventually unwedges the turn.
            self.completion
                .lock()
                .expect("completion mutex poisoned")
                .take();
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

    // The crate connection's synthetic transport. `crate_side` is handed to
    // `ByteStreams`; `shim_side` is split so the reader can inject inbound
    // ACP lines and the pump can read the crate's outbound ones. 64 KiB
    // matches the pipe size the stdio path gets.
    let (crate_side, shim_side) = tokio::io::duplex(64 * 1024);
    let (shim_read, shim_write) = tokio::io::split(shim_side);
    let shim_write = Arc::new(Mutex::new(shim_write));
    let correlation = Arc::new(Mutex::new(ShimCorrelation::default()));

    let raw_fd = write_half.as_ref().as_raw_fd();
    let write_half = Arc::new(Mutex::new(write_half));

    let reader_prompt_in_flight = prompt_in_flight.clone();
    let (hs_tx, hs_rx) = mpsc::channel::<ControlBody>(8);
    let completion: Arc<
        std::sync::Mutex<Option<oneshot::Sender<control_protocol::PromptOutcome>>>,
    > = Arc::new(std::sync::Mutex::new(None));
    let reader_completion = completion.clone();
    let reader_session = session_label.clone();
    let reader_shim_write = shim_write.clone();
    let reader_correlation = correlation.clone();
    tokio::spawn(async move {
        async {
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
                    let waiter = reader_completion
                        .lock()
                        .expect("completion mutex poisoned")
                        .take();
                    if let Some(tx) = waiter {
                        let _ = tx.send(outcome);
                    } else {
                        // A waiterless completion belongs to an adopted turn
                        // only when durable history still marks a prompt in
                        // flight. A retained runner completion also replays
                        // after the daemon already committed the terminal;
                        // that case must be ignored rather than published
                        // twice. Clearing the flag first hands idle ownership
                        // back before any terminal event is emitted.
                        let was_in_flight =
                            reader_prompt_in_flight.swap(false, AtomicOrdering::Relaxed);
                        if !was_in_flight {
                            debug!(
                                target: "acp.protocol",
                                session = %reader_session,
                                "ignoring replayed PromptCompleted for a durable terminal"
                            );
                            continue;
                        }
                        if terminal_claim.claim() {
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
                        } else {
                            warn!(
                                target: "acp.protocol",
                                session = %reader_session,
                                "runner reported PromptCompleted after the turn terminal was claimed"
                            );
                        }
                    }
                }
                // #2977 reverse lane: an agent-to-client request. Injected
                // into the crate transport as an ordinary JSON-RPC request
                // under a synthetic id, so the nine `on_receive_request`
                // handlers serve it exactly as they did off the relay. The
                // crate spawns each handler, so a permission parked for
                // minutes never blocks this reader or the frames behind it.
                Ok(Some(ControlBody::ServerCall {
                    call_id,
                    method,
                    params,
                })) => {
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
                // Fire-and-forget agent notification, forwarded verbatim.
                Ok(Some(ControlBody::Notify { method, params })) => {
                    let line = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": method,
                        "params": params,
                    });
                    if !shim_write_line(&reader_shim_write, &line).await {
                        return;
                    }
                }
                // #2977 forward lane: the runner's answer to a request the
                // crate connection made. Handed back under the crate's own
                // id so its `send_request` future resolves.
                Ok(Some(ControlBody::AgentResult { call_id, result })) => {
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
                Ok(Some(ControlBody::AgentError { call_id, error })) => {
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
        reader_completion
            .lock()
            .expect("completion mutex poisoned")
            .take();
        let mut shim_write = reader_shim_write.lock().await;
        let _ = shim_write.shutdown().await;
    });

    // Pump the other direction: everything the crate connection writes to
    // the synthetic transport. A line with a `method` is one of the five
    // client-to-agent requests the runner does not own, so it becomes an
    // `AgentCall`; a line without one answers a reverse call the crate just
    // handled, so it becomes a `ServerResult` / `ServerError`.
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
            write: write_half,
            handshake_rx: Mutex::new(hs_rx),
            completion,
            raw_fd,
        }),
        crate_side,
    ))
}

/// Map a runner-reported prompt outcome to an `Event::Stopped` reason. A
/// completed turn renders as Idle regardless of stop reason, so the
/// default is `prompt_complete`; the one reason with special downstream
/// handling (`rate_limited`) is preserved when the agent reports it. An
/// agent error-envelope or an aborted turn also renders Idle, so they map
/// to `prompt_complete` as well; the turn is over either way.
///
/// The other ACP stop reasons (`cancelled`, `max_tokens`, `refusal`,
/// `max_turn_requests`) are preserved verbatim as of #2977 rather than
/// collapsing into `prompt_complete`. They all still render Idle, so nothing
/// downstream had to change, but the reason now reaches the UI and the event
/// log, where "the agent hit its token ceiling" and "the turn ended normally"
/// stop looking identical after the fact.
pub(super) fn control_outcome_reason(
    outcome: &crate::acp::control_protocol::PromptOutcome,
) -> String {
    use crate::acp::control_protocol::PromptOutcome;
    match outcome {
        PromptOutcome::Completed {
            stop_reason: Some(r),
        } => match r.as_str() {
            // The one reason with special downstream handling; the adapter
            // spells it both ways.
            "rate_limited" | "rate_limit" => "rate_limited".to_string(),
            "cancelled" | "max_tokens" | "refusal" | "max_turn_requests" => r.clone(),
            // An unrecognized stop reason still renders Idle; report the
            // generic terminal rather than inventing a reason string the UI
            // has no mapping for.
            _ => "prompt_complete".to_string(),
        },
        // No stop reason, an agent error envelope, or a runner-side abort:
        // the turn is over either way.
        _ => "prompt_complete".to_string(),
    }
}

/// Drive a session-creation request over control protocol v3 and
/// deserialize the runner's cached result into the crate response type,
/// so each `session/new|load|fork` site's `Result<Resp, Error>` matches
/// the crate `send_request` path it replaces (including the failure path:
/// the runner-forwarded agent error propagates verbatim).
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

/// Adapt a runner-reported [`PromptOutcome`](control_protocol::PromptOutcome)
/// into the `Result<PromptResponse, Error>` the prompt loop already
/// consumes, so the loop body is identical for control v3 and direct stdio.
/// A completed turn maps to its `StopReason`; an agent
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
        // Reconstruct the crate error verbatim so transport choice does not
        // change standard, ACP-specific, or custom JSON-RPC error taxonomy.
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

    /// An oversized reverse response resolves the runner call with a bounded
    /// error and leaves the same control connection usable for the next call.
    #[tokio::test]
    async fn oversized_reverse_reply_becomes_error_without_poisoning_connection() {
        use crate::acp::control_protocol::{self, ControlBody};
        use std::sync::atomic::AtomicBool;
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use tokio::net::UnixListener;

        let tmp = tempfile::tempdir().unwrap();
        let main_socket = tmp.path().join("oversize.sock");
        let control = crate::process::worker::control_socket_sibling(&main_socket);
        let listener = UnixListener::bind(&control).unwrap();
        let fake = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut read, mut write) = stream.into_split();
            control_protocol::write_frame(
                &mut write,
                &ControlBody::Hello {
                    control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
                    session_id: "oversize".into(),
                },
            )
            .await
            .unwrap();
            let _ = control_protocol::read_frame(&mut read).await.unwrap();
            for call_id in [41, 42] {
                control_protocol::write_frame(
                    &mut write,
                    &ControlBody::ServerCall {
                        call_id,
                        method: "fs/read_text_file".into(),
                        params: serde_json::json!({}),
                    },
                )
                .await
                .unwrap();
                let reply = control_protocol::read_frame(&mut read)
                    .await
                    .unwrap()
                    .unwrap();
                if call_id == 41 {
                    assert!(matches!(
                        reply,
                        ControlBody::ServerError { call_id: 41, error }
                            if error.code == control_protocol::INTERNAL_ERROR
                    ));
                } else {
                    assert!(matches!(
                        reply,
                        ControlBody::ServerResult { call_id: 42, result }
                            if result == serde_json::json!({"ok": true})
                    ));
                }
            }
        });

        let (event_tx, _) = mpsc::channel::<Event>(1);
        let (_, crate_side) = connect_runner_control_v3(
            &control,
            event_tx,
            "oversize".into(),
            Arc::new(TerminalClaim::new()),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();
        let (read, mut write) = tokio::io::split(crate_side);
        let mut read = BufReader::new(read);
        let mut line = String::new();
        read.read_line(&mut line).await.unwrap();
        let first: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let huge = "x".repeat(control_protocol::MAX_CONTROL_FRAME_BYTES as usize);
        let mut response = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0", "id": first["id"], "result": {"content": huge},
        }))
        .unwrap();
        response.push(b'\n');
        write.write_all(&response).await.unwrap();

        line.clear();
        read.read_line(&mut line).await.unwrap();
        let second: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let mut response = serde_json::to_vec(&serde_json::json!({
            "jsonrpc": "2.0", "id": second["id"], "result": {"ok": true},
        }))
        .unwrap();
        response.push(b'\n');
        write.write_all(&response).await.unwrap();
        fake.await.unwrap();
    }

    #[tokio::test]
    async fn runner_control_eof_closes_transport_and_cancels_prompt() {
        use crate::acp::control_protocol::{self, ControlBody};
        use std::sync::atomic::AtomicBool;
        use std::time::Duration;
        use tokio::io::AsyncReadExt;
        use tokio::net::UnixListener;

        let tmp = tempfile::tempdir().unwrap();
        let main_socket = tmp.path().join("eof.sock");
        let control = crate::process::worker::control_socket_sibling(&main_socket);
        let listener = UnixListener::bind(&control).unwrap();
        let fake = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut read, mut write) = stream.into_split();
            control_protocol::write_frame(
                &mut write,
                &ControlBody::Hello {
                    control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
                    session_id: "eof".into(),
                },
            )
            .await
            .unwrap();
            assert!(matches!(
                control_protocol::read_frame(&mut read).await.unwrap(),
                Some(ControlBody::Attach { .. })
            ));
            assert!(matches!(
                control_protocol::read_frame(&mut read).await.unwrap(),
                Some(ControlBody::Prompt { .. })
            ));
        });

        let (client, crate_side) = connect_runner_control_v3(
            &control,
            mpsc::channel::<Event>(1).0,
            "eof".into(),
            Arc::new(TerminalClaim::new()),
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap();
        let completion = client.prompt(serde_json::json!({})).await;
        fake.await.unwrap();

        assert!(
            tokio::time::timeout(Duration::from_secs(1), completion)
                .await
                .expect("prompt completion must resolve after control EOF")
                .is_err(),
            "control EOF must cancel the in-flight prompt"
        );
        let (mut crate_read, _crate_write) = tokio::io::split(crate_side);
        let mut byte = [0_u8; 1];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), crate_read.read(&mut byte))
                .await
                .expect("crate transport must observe control EOF")
                .unwrap(),
            0
        );
    }

    #[test]
    fn prompt_error_preserves_code_message_and_data() {
        use agent_client_protocol::ErrorCode;
        use control_protocol::PromptOutcome;

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
            assert_eq!(error.code, expected, "{code}");
            assert_eq!(error.message, "boom", "{code}");
            assert_eq!(error.data, Some(data), "{code}");
        }
    }

    /// A waiterless completion for an adopted turn publishes its terminal
    /// event and disarms the resume-idle watchdog.
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
        // Set, as a stranded prompt loop would leave it: the reader must hand
        // idle ownership back when it surfaces the waiterless completion.
        let prompt_in_flight = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let client = connect_runner_control_v3(
            &crate::process::worker::control_socket_sibling(&main_socket),
            event_tx,
            "s".into(),
            guard.clone(),
            prompt_in_flight.clone(),
        )
        .await
        .expect("v3 control client")
        .0;

        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), event_rx.recv())
            .await
            .expect("timed out waiting for Stopped")
            .expect("event channel closed");
        assert!(matches!(ev, Event::Stopped { reason } if reason == "prompt_complete"));
        assert!(guard.claimed(), "the turn's terminal must be claimed");
        assert!(
            !prompt_in_flight.load(std::sync::atomic::Ordering::Relaxed),
            "a waiterless completion must hand idle ownership back so the lane can arm"
        );
        drop(client);
        let _ = fake.await;
    }

    #[tokio::test]
    async fn adopted_rate_limit_emits_metadata_before_stopped() {
        use crate::acp::control_protocol::{self, ControlBody, PromptOutcome};
        use tokio::net::UnixListener;

        let tmp = tempfile::tempdir().unwrap();
        let main_socket = tmp.path().join("rate.sock");
        let control = crate::process::worker::control_socket_sibling(&main_socket);
        let listener = UnixListener::bind(&control).unwrap();
        let fake = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut read, mut write) = stream.into_split();
            control_protocol::write_frame(
                &mut write,
                &ControlBody::Hello {
                    control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
                    session_id: "rate".into(),
                },
            )
            .await
            .unwrap();
            let _ = control_protocol::read_frame(&mut read).await;
            control_protocol::write_frame(
                &mut write,
                &ControlBody::PromptCompleted {
                    prompt_req_id: 7,
                    outcome: PromptOutcome::Error {
                        code: -32000,
                        message: "rate limit exceeded".into(),
                        data: Some(serde_json::json!({"errorKind": "rate_limit"})),
                    },
                },
            )
            .await
            .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        });

        let (event_tx, mut event_rx) = mpsc::channel::<Event>(8);
        let (client, _) = connect_runner_control_v3(
            &control,
            event_tx,
            "rate".into(),
            Arc::new(TerminalClaim::new()),
            Arc::new(std::sync::atomic::AtomicBool::new(true)),
        )
        .await
        .expect("control client");
        let first = event_rx.recv().await.expect("rate-limit event");
        let second = event_rx.recv().await.expect("terminal event");
        assert!(matches!(first, Event::RateLimit { .. }));
        assert!(matches!(second, Event::Stopped { reason } if reason == "rate_limited"));
        drop(client);
        let _ = fake.await;
    }

    /// Clears a process-wide env var on drop, so a panicking test cannot leak
    /// it into whatever runs next.
    struct RestoreEnvOnDrop(&'static str);

    impl Drop for RestoreEnvOnDrop {
        fn drop(&mut self) {
            // SAFETY: callers hold a default-key `#[serial]` lock.
            unsafe {
                std::env::remove_var(self.0);
            }
        }
    }

    /// A runner whose `Hello` advertises an unknown control-protocol version
    /// is not trusted: no terminal event is fabricated and the guard remains
    /// unclaimed.
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
        let client = connect_runner_control_v3(
            &crate::process::worker::control_socket_sibling(&main_socket),
            event_tx,
            "s".into(),
            guard.clone(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await;

        let error = match client {
            Ok(_) => panic!("unknown control version must fail"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("runner Hello mismatch"),
            "unexpected mismatch error: {error:#}"
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

    /// A runner that never binds the control socket yields no client and
    /// leaves the guard unclaimed. As of #2977 there is no relay to fall back
    /// to, so the caller turns this into a typed spawn error rather than a
    /// downgrade; a live worker of an older generation is replaced by the
    /// reconciler instead of being attached.
    #[tokio::test]
    #[serial_test::serial]
    async fn runner_control_absent_socket_leaves_guard_unclaimed() {
        let tmp = tempfile::tempdir().unwrap();
        // No control listener is bound at the sibling path.
        let main_socket = tmp.path().join("s.sock");

        // A missing socket is legitimately retryable (the runner binds it
        // shortly after spawn), so the dial waits out its deadline. Shrink the
        // deadline rather than the retry, so the test does not spend the full
        // production window proving a negative. `#[serial]` because this is a
        // process-wide env var.
        // SAFETY: serialized against other default-key serial tests.
        unsafe {
            std::env::set_var("AOE_ACP_RUNNER_SOCKET_TIMEOUT_MS", "150");
        }
        let _restore = RestoreEnvOnDrop("AOE_ACP_RUNNER_SOCKET_TIMEOUT_MS");

        let (event_tx, mut event_rx) = mpsc::channel::<Event>(8);
        let guard = Arc::new(TerminalClaim::new());
        let client = connect_runner_control_v3(
            &crate::process::worker::control_socket_sibling(&main_socket),
            event_tx,
            "s".into(),
            guard.clone(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await;

        let error = match client {
            Ok(_) => panic!("absent control socket must fail"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("timed out attaching runner control socket"),
            "unexpected missing-socket error: {error:#}"
        );
        assert!(
            !guard.claimed(),
            "absent control socket must not claim the terminal"
        );
        assert!(event_rx.try_recv().is_err());
    }
    #[tokio::test]
    async fn nonretryable_dial_error_preserves_os_cause() {
        let tmp = tempfile::tempdir().unwrap();
        let control = tmp.path().join("x".repeat(200));
        let (event_tx, _event_rx) = mpsc::channel::<Event>(1);
        let result = connect_runner_control_v3(
            &control,
            event_tx,
            "s".into(),
            Arc::new(TerminalClaim::new()),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await;
        let error = match result {
            Ok(_) => panic!("overlong Unix socket path must fail"),
            Err(error) => error,
        };
        let message = format!("{error:#}");
        assert!(
            message.contains("connect runner control socket"),
            "{message}"
        );
        assert!(!message.contains("timed out attaching"), "{message}");
    }
}
