//! The v3 runner control socket: connecting, establishing a session, and
//! routing ACP frames over it.

use crate::acp::control_protocol::{self, ControlBody};
use crate::acp::state::Event;
use agent_client_protocol::schema::v1::PromptResponse;
use std::collections::HashMap;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::Ordering as AtomicOrdering;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{debug, info, warn};

use super::errors::{acp_error_from_value, acp_internal_error, AcpError};
use super::lifecycle::TerminalClaim;
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

/// Bidirectional client for the runner's sibling control socket
/// (#2976 Phase B). The runner owns the ACP handshake and the turn, so the
/// daemon drives `initialize` / `session/*` / `session/prompt` /
/// `session/cancel` over this channel and receives the typed results,
/// rather than speaking those methods over the byte relay.
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

/// Dial the runner's control socket and, if it speaks control protocol v3,
/// return a [`DaemonControlClient`], a synthetic crate transport, and spawn
/// the control reader. Returns None when the runner cannot be attached.
pub(super) async fn connect_runner_control_v3(
    control_path: &std::path::Path,
    event_tx: mpsc::Sender<Event>,
    session_label: String,
    terminal_claim: Arc<TerminalClaim>,
    prompt_in_flight: Arc<std::sync::atomic::AtomicBool>,
) -> Option<(Arc<DaemonControlClient>, tokio::io::DuplexStream)> {
    // One deadline covers waiting for the runner to bind, connecting, and
    // reading its Hello. The runner binds before it spawns the agent, so in
    // practice this resolves in milliseconds; the bound is what turns a
    // wedged or too-old runner into a typed error instead of parking the
    // supervisor.
    let bound = runner_socket_deadline();
    let dial = async {
        let stream = loop {
            match tokio::net::UnixStream::connect(control_path).await {
                Ok(stream) => break stream,
                // Retry only what a not-yet-ready runner actually produces:
                // the socket file missing, or bound but not yet listening.
                // Anything else (a path too long for `sun_path`, no
                // permission, a path that is not a socket) will not fix
                // itself, so spinning to the deadline and then reporting
                // "does not speak v3" would bury the real cause.
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await
                }
                Err(e) => {
                    warn!(
                        target: "acp.protocol",
                        session = %session_label,
                        path = %control_path.display(),
                        "control socket is unusable: {e}"
                    );
                    return None;
                }
            }
        };
        let (mut read_half, mut write_half) = stream.into_split();
        match control_protocol::read_frame(&mut read_half).await {
            // Check the session id too, not just the version. This channel now
            // carries the whole event stream and every reverse call, so dialing
            // the wrong runner would cross two sessions' traffic rather than
            // merely miss a completion.
            Ok(Some(ControlBody::Hello {
                control_protocol_version,
                session_id,
            })) if control_protocol_version == control_protocol::CONTROL_PROTOCOL_VERSION
                && session_id == session_label => {}
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
                "no usable v3 runner control socket; treating the runner as unusable"
            );
            return None;
        }
    };

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
                        // Adopted turn on a mid-flight resume: this daemon
                        // never issued the prompt, so surface the completion
                        // as Stopped and stand the watchdogs down.
                        //
                        // The waiter being absent means no `prompt_fut` on
                        // this connection can ever resolve for this turn, so a
                        // `prompt_in_flight` still set here is stale by
                        // definition. Left set, it silently disables the whole
                        // between-prompt lane (every bit of that bookkeeping is
                        // gated on `!prompt_active`), so the next
                        // agent-initiated turn gets no terminal either and the
                        // session renders Running until the reconciler's repair
                        // pass. Clearing it hands idle ownership back the way
                        // the prompt drain would have.
                        //
                        // Partial cure by construction: it re-arms the lane but
                        // cannot unpark a prompt loop still awaiting that dead
                        // future, which keeps rejecting new prompts as
                        // `agent_busy`. See #3190 and PR #3192 review.
                        let claimed = terminal_claim.claim();
                        let was_in_flight =
                            reader_prompt_in_flight.swap(false, AtomicOrdering::Relaxed);
                        if claimed {
                            // The expected shape: a turn adopted at reattach,
                            // whose completion this connection has to surface.
                            debug!(
                                target: "acp.protocol",
                                session = %reader_session,
                                stranded_prompt = was_in_flight,
                                "runner reported PromptCompleted with no waiter; surfacing as Stopped"
                            );
                            let reason = control_outcome_reason(&outcome);
                            let _ = event_tx.send(Event::Stopped { reason }).await;
                        } else {
                            // Something already published this turn's terminal,
                            // so this completion is a duplicate. Not expected.
                            warn!(
                                target: "acp.protocol",
                                session = %reader_session,
                                stranded_prompt = was_in_flight,
                                "runner reported PromptCompleted with no waiter and the turn's terminal was already claimed"
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
                // #2977: a fire-and-forget agent notification (session/update
                // and anything else the adapter emits), replayed verbatim.
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
    });

    // Pump the other direction: everything the crate connection writes to
    // the synthetic transport. A line with a `method` is one of the five
    // client-to-agent requests the runner does not own, so it becomes an
    // `AgentCall`; a line without one answers a reverse call the crate just
    // handled, so it becomes a `ServerResult` / `ServerError`.
    let pump_write = write_half.clone();
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
                Err(e) => {
                    debug!(
                        target: "acp.protocol",
                        session = %pump_session,
                        "shim transport read ended: {e}"
                    );
                    return;
                }
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                continue;
            };
            let frame = if let Some(method) = value.get("method").and_then(|m| m.as_str()) {
                let Some(id) = value.get("id").cloned() else {
                    // A notification from the crate. Nothing outbound today
                    // uses one, and the runner owns `session/cancel`, so
                    // there is no lane for it.
                    continue;
                };
                let call_id = {
                    let mut c = pump_correlation.lock().await;
                    let call_id = c.forward_id();
                    c.forward.insert(call_id, id);
                    call_id
                };
                ControlBody::AgentCall {
                    call_id,
                    method: method.to_string(),
                    params: value
                        .get("params")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                }
            } else {
                let Some(synthetic) = value.get("id").and_then(|i| i.as_i64()) else {
                    continue;
                };
                let Some(call_id) = pump_correlation.lock().await.reverse.remove(&synthetic) else {
                    continue;
                };
                match value.get("error") {
                    Some(err) if !err.is_null() => ControlBody::ServerError {
                        call_id,
                        error: serde_json::from_value(err.clone()).unwrap_or_else(|_| {
                            // A handler answered with an error envelope this
                            // side cannot parse. That is an internal failure,
                            // not a missing method, so it must not borrow
                            // -32601: an agent that special-cases that code
                            // would conclude the method is unsupported and
                            // stop calling it.
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
            let mut w = pump_write.lock().await;
            if control_protocol::write_frame(&mut *w, &frame)
                .await
                .is_err()
            {
                return;
            }
        }
    });

    Some((
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

    /// Daemon-side control consumer: a runner that greets with a matching
    /// `Hello` and then reports `PromptCompleted` for an adopted turn (no
    /// prompt awaiting on this daemon) drives an
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
        let client = connect_runner_control_v3(
            &crate::process::worker::control_socket_sibling(&main_socket),
            event_tx,
            "s".into(),
            guard.clone(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await;

        assert!(
            client.is_none(),
            "unknown control version must not yield a control client"
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

        assert!(
            client.is_none(),
            "absent control socket must not yield a control client"
        );
        assert!(
            !guard.claimed(),
            "absent control socket must not claim the terminal"
        );
        assert!(event_rx.try_recv().is_err());
    }
}
