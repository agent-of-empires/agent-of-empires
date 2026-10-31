//! The v4 runner control socket: connecting, establishing a session, and
//! routing ACP frames over it.

use crate::acp::control_protocol::{
    self, ControlBody, NativeChannelAvailability, NativeChannelLossCause, NativeChannelLostMarker,
    PromptCompletedMarker, SessionReplayed,
};
use crate::acp::state::{AcpConnectionCause, AcpConnectionSource, Event};
use agent_client_protocol::schema::v1::PromptResponse;
use agent_client_protocol::JsonRpcMessage as _;
use std::collections::HashMap;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::Arc;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::{mpsc, oneshot, watch, Mutex};
use tracing::{debug, info, warn};

use super::connection::ConnectionState;
use super::errors::{acp_error_from_value, acp_internal_error, prompt_runtime_error, AcpError};
use super::lifecycle::TerminalClaim;
use super::rate_limit::classify_rate_limit_error;

/// Bound completion of an admitted control frame after observed loss, using
/// the runner's existing two-second write window rather than hanging an app
/// scope behind a partial write to an unavailable peer.
const LOSS_WRITE_GRACE: std::time::Duration = std::time::Duration::from_secs(2);
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
    pub(super) connection_state: Arc<ConnectionState>,
    write: Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    handshake_rx: Mutex<mpsc::Receiver<ControlBody>>,
    sessions_established: AtomicU64,
    native_cache: Arc<AtomicBool>,
    native_ready: Arc<AtomicBool>,
    sessions_replayed: watch::Sender<u64>,
    pub(super) completion: Arc<std::sync::Mutex<PromptCompletion>>,
    /// A matched local outcome held until the crate reaches its
    /// [`PromptCompletedMarker`], or until one of the reader's relays fails.
    settled: Arc<std::sync::Mutex<Option<LocalOutcome>>>,
    raw_fd: RawFd,
    peer_identity: crate::acp::runner_lifecycle::RunnerIdentity,
    peer_process: crate::process::CapturedProcess,
}

type LocalOutcome = (
    oneshot::Sender<ControlPromptResult>,
    control_protocol::PromptOutcome,
);

#[derive(Debug, PartialEq)]
pub(super) enum ControlPromptResult {
    Outcome(control_protocol::PromptOutcome),
    NativeLost,
}

fn observe_native_loss(state: &ConnectionState, cause: NativeChannelLossCause) {
    state.observe(
        AcpConnectionSource::Native,
        match cause {
            NativeChannelLossCause::StdoutEof => AcpConnectionCause::StdoutEof,
            NativeChannelLossCause::ReadFailed => AcpConnectionCause::ReadFailed,
            NativeChannelLossCause::WriteFailed => AcpConnectionCause::WriteFailed,
            NativeChannelLossCause::ProtocolEnded => AcpConnectionCause::ProtocolEnded,
        },
    );
}
#[derive(serde::Serialize)]
struct CompletionMarkerLine<'a> {
    jsonrpc: &'static str,
    method: &'a str,
    params: &'a PromptCompletedMarker,
}

pub(super) enum PromptCompletion {
    Adopted,
    Pending {
        // `None` until `PromptStarted`, so retained history read during send
        // cannot resolve this waiter.
        prompt_req_id: Option<i64>,
        tx: oneshot::Sender<ControlPromptResult>,
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

/// `true` once the line sits in the pipe for the crate; `false` once this
/// write half is closed, by the crate hanging up or by the reader's teardown.
async fn shim_write_line<T: serde::Serialize + ?Sized>(
    duplex: &Mutex<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
    value: &T,
) -> bool {
    use tokio::io::AsyncWriteExt;
    let Ok(mut bytes) = serde_json::to_vec(value) else {
        return false;
    };
    bytes.push(b'\n');
    let mut w = duplex.lock().await;
    w.write_all(&bytes).await.is_ok() && w.flush().await.is_ok()
}

/// Preserve a correlated outcome when its relay marker can no longer arrive.
fn release_settled(settled: &std::sync::Mutex<Option<LocalOutcome>>) {
    if let Some((tx, outcome)) = settled.lock().expect("settled mutex poisoned").take() {
        let _ = tx.send(ControlPromptResult::Outcome(outcome));
    }
}

/// A failed relay releases any local outcome whose marker can no longer arrive.
async fn shim_write_line_releasing<T: serde::Serialize + ?Sized>(
    duplex: &Mutex<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
    value: &T,
    settled: &std::sync::Mutex<Option<LocalOutcome>>,
    state: &ConnectionState,
) -> bool {
    if shim_write_line(duplex, value).await {
        return true;
    }
    state.observe(
        crate::acp::state::AcpConnectionSource::Control,
        crate::acp::state::AcpConnectionCause::ForwardPumpEnded,
    );
    release_settled(settled);
    state.mark_ordered_loss();
    false
}

impl DaemonControlClient {
    pub(super) fn shutdown(&self) {
        // SAFETY: `self` keeps this exact socket alive for the call.
        unsafe { libc::shutdown(self.raw_fd, libc::SHUT_RDWR) };
    }

    async fn send(&self, body: ControlBody) -> Result<(), AcpError> {
        let cached_management = matches!(
            &body,
            ControlBody::Initialize { .. }
                | ControlBody::EstablishSession { .. }
                | ControlBody::ResumeSession
        );
        let wire = control_protocol::encode_frame(&body).map_err(|error| {
            warn!(target: "acp.protocol", ?error, "control request encoding refused");
            AcpError::Protocol("request exceeds runner control capacity".into())
        })?;
        self.send_encoded(&wire, cached_management).await
    }

    fn may_report_cache(&self, cached_management: bool) -> bool {
        cached_management
            && self.native_cache.load(AtomicOrdering::Acquire)
            && self
                .connection_state
                .loss()
                .is_some_and(|loss| loss.source == AcpConnectionSource::Native)
    }

    async fn send_encoded(&self, wire: &[u8], cached_management: bool) -> Result<(), AcpError> {
        let lock = self.write.lock();
        tokio::pin!(lock);
        let mut writer = tokio::select! {
            biased;
            writer = &mut lock => writer,
            loss = self.connection_state.wait_unavailable() => {
                if !self.may_report_cache(cached_management) {
                    return Err(AcpError::ConnectionUnavailable(loss));
                }
                match tokio::time::timeout(LOSS_WRITE_GRACE, &mut lock).await {
                    Ok(writer) => writer,
                    Err(_) => return Err(AcpError::ConnectionUnavailable(loss)),
                }
            }
        };
        let write = control_protocol::write_encoded_frame(&mut *writer, wire);
        tokio::pin!(write);
        let mut started = false;
        let write = std::future::poll_fn(|cx| {
            let _admission = self
                .connection_state
                .admission
                .lock()
                .expect("connection admission lock");
            if !started {
                if self.connection_state.is_stopping() {
                    return std::task::Poll::Ready(Err(AcpError::NotRunning));
                }
                if let Some(observation) = self.connection_state.loss() {
                    if !self.may_report_cache(cached_management) {
                        return std::task::Poll::Ready(Err(AcpError::ConnectionUnavailable(
                            observation,
                        )));
                    }
                }
                // Finish an admitted control frame rather than corrupting its
                // length framing on partial writes. The runner separately
                // rejects all native effects once its own channel is lost.
                started = true;
            }
            match std::future::Future::poll(write.as_mut(), cx) {
                std::task::Poll::Pending => std::task::Poll::Pending,
                std::task::Poll::Ready(Ok(())) => std::task::Poll::Ready(Ok(())),
                std::task::Poll::Ready(Err(error)) => {
                    self.connection_state.record_loss(
                        AcpConnectionSource::Control,
                        AcpConnectionCause::WriteFailed,
                    );
                    drop(_admission);
                    warn!(target: "acp.protocol", ?error, "runner control write failed");
                    self.shutdown();
                    std::task::Poll::Ready(Err(AcpError::ConnectionUnavailable(
                        self.connection_state
                            .loss()
                            .expect("write failure recorded"),
                    )))
                }
            }
        });
        tokio::pin!(write);
        tokio::select! {
            biased;
            result = &mut write => result,
            loss = self.connection_state.wait_unavailable() => {
                match tokio::time::timeout(LOSS_WRITE_GRACE, &mut write).await {
                    Ok(result) => result,
                    Err(_) => {
                        // A genuinely stalled partial control write cannot be
                        // reused. Close that exact management stream, not the
                        // retained native process or its ownership record.
                        self.shutdown();
                        Err(AcpError::ConnectionUnavailable(loss))
                    }
                }
            }
        }
    }

    async fn receive_handshake(&self) -> Option<ControlBody> {
        let mut replies = self.handshake_rx.lock().await;
        tokio::select! {
            biased;
            reply = replies.recv() => reply,
            _ = async {
                let loss = self.connection_state.wait_unavailable().await;
                if loss.source == AcpConnectionSource::Native
                    && self.native_cache.load(AtomicOrdering::Acquire)
                {
                    // Genuine cached management replies require no native I/O.
                    // Actual management EOF closes `replies` and still settles us.
                    std::future::pending::<()>().await;
                }
            } => None,
        }
    }

    pub(super) fn native_session_ready(&self) -> bool {
        self.native_ready.load(AtomicOrdering::Acquire) && self.connection_state.loss().is_none()
    }

    pub(super) fn peer_identity(&self) -> crate::acp::runner_lifecycle::RunnerIdentity {
        self.peer_identity
    }

    /// A lost connection does not assert process death or authorize a signal.
    pub(super) async fn wait_peer_exit(&self) {
        loop {
            match self.peer_process.exited() {
                Ok(true) => return,
                Ok(false) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
                Err(error) => {
                    warn!(target: "acp.protocol", ?error, "retained runner exit observation is unavailable");
                    std::future::pending::<()>().await;
                }
            }
        }
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
        match self.receive_handshake().await {
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
        match self.receive_handshake().await {
            Some(ControlBody::SessionReady {
                acp_session_id,
                result,
                ..
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
        match self.receive_handshake().await {
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
        let mut replayed = self.sessions_replayed.subscribe();
        tokio::select! {
            biased;
            _ = replayed.wait_for(|replayed| *replayed >= established) => {}
            _ = self.connection_state.wait_ordered_unavailable() => {}
        }
    }

    pub(super) fn mark_session_replayed(&self, _: SessionReplayed) {
        self.sessions_replayed
            .send_modify(|replayed| *replayed += 1);
    }

    pub(super) fn deliver_prompt_completion(&self, _: PromptCompletedMarker) {
        let settled = self.settled.lock().expect("settled mutex poisoned").take();
        if let Some((tx, outcome)) = settled {
            let _ = tx.send(ControlPromptResult::Outcome(outcome));
        }
    }

    pub(super) fn deliver_native_lost(&self) {
        let mut completion = self.completion.lock().expect("completion mutex poisoned");
        if matches!(*completion, PromptCompletion::Pending { .. }) {
            if let PromptCompletion::Pending { tx, .. } =
                std::mem::replace(&mut *completion, PromptCompletion::LocalIdle)
            {
                let _ = tx.send(ControlPromptResult::NativeLost);
            }
        }
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
    ) -> Result<oneshot::Receiver<ControlPromptResult>, AcpError> {
        self.connection_state.admit(|| ())?;
        let wire =
            control_protocol::encode_frame(&ControlBody::Prompt { request }).map_err(|error| {
                warn!(target: "acp.protocol", ?error, "prompt encoding refused");
                AcpError::Protocol("prompt exceeds runner control capacity".into())
            })?;
        let rx = self.connection_state.admit(|| {
            let mut completion = self.completion.lock().expect("completion mutex poisoned");
            if matches!(*completion, PromptCompletion::Pending { .. }) {
                return Err(AcpError::Protocol(
                    "a local prompt is already awaiting completion".into(),
                ));
            }
            let (tx, rx) = oneshot::channel();
            *completion = PromptCompletion::Pending {
                prompt_req_id: None,
                tx,
            };
            Ok(rx)
        })??;
        debug!(target: "acp.protocol", "prompt completion waiter installed");
        if self.send_encoded(&wire, false).await.is_err() {
            let mut completion = self.completion.lock().expect("completion mutex poisoned");
            if matches!(*completion, PromptCompletion::Pending { .. }) {
                *completion = PromptCompletion::LocalIdle;
            }
        }
        Ok(rx)
    }

    pub(super) async fn cancel(&self) {
        let _ = self.send(ControlBody::Cancel).await;
    }
}

/// A newborn must publish its own declaration; never retrofit a retained record.
pub(super) async fn wait_runner_registration(
    session_id: &str,
    expected_owner: crate::acp::runner_lifecycle::RunnerIdentity,
    newborn: bool,
    deadline: Option<tokio::time::Instant>,
) -> Result<(), AcpError> {
    let deadline =
        deadline.unwrap_or_else(|| tokio::time::Instant::now() + runner_socket_deadline());
    let unconfirmed = || {
        AcpError::RetainedRuntimeUnavailable(Box::new(
            crate::process::worker_registry::ControlAdmissionRefusal::unconfirmed(Some(
                expected_owner,
            )),
        ))
    };
    loop {
        let observed = {
            let registry =
                crate::process::worker_registry::registry_lock_until(session_id, deadline)
                    .await
                    .map_err(|error| {
                        if error.is::<crate::process::worker_registry::RegistryBusy>() {
                            AcpError::RegistryBusy
                        } else {
                            unconfirmed()
                        }
                    })?;
            registry.record()
        };
        match observed {
            Ok(Some(record)) if expected_owner.owns_record(&record) => {
                if let Some(refusal) =
                    crate::process::worker_registry::control_protocol_refusal(&record)
                {
                    return Err(AcpError::RetainedRuntimeUnavailable(Box::new(refusal)));
                }
                return Ok(());
            }
            Ok(Some(record))
                if !newborn
                    || crate::process::worker_registry::retained_process_may_be_alive(&record) =>
            {
                return Err(unconfirmed());
            }
            Ok(None) if !newborn => return Err(unconfirmed()),
            Err(error) => {
                warn!(target: "acp.protocol", ?error, "runner registration observation failed");
                return Err(unconfirmed());
            }
            Ok(_) => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(unconfirmed());
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// Dial one v4 control socket and bind its actual peer before trusting Hello.
pub(super) async fn connect_runner_control_v4(
    control_path: &std::path::Path,
    session_label: String,
    terminal_claim: Arc<TerminalClaim>,
    prompt_in_flight: Arc<std::sync::atomic::AtomicBool>,
    expected_owner: crate::acp::runner_lifecycle::RunnerIdentity,
) -> Result<(Arc<DaemonControlClient>, tokio::io::DuplexStream), AcpError> {
    let bound = runner_socket_deadline();
    let refused = |reason, observed_protocol| {
        AcpError::RetainedRuntimeUnavailable(Box::new(
            crate::process::worker_registry::ControlAdmissionRefusal {
                reason,
                owner: Some(expected_owner),
                declared_protocol: Some(control_protocol::CONTROL_PROTOCOL_VERSION),
                observed_protocol,
                route_fingerprint: Some(
                    crate::process::worker_registry::control_route_fingerprint(control_path),
                ),
            },
        ))
    };
    let unconfirmed = || {
        refused(
            crate::process::worker_registry::ControlRefusalReason::OwnershipUnconfirmed,
            None,
        )
    };
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
                    warn!(target: "acp.protocol", ?error, "runner control connection failed");
                    return Err(unconfirmed());
                }
            }
        };
        if crate::process::worker::connected_peer_pid(&stream) != Some(expected_owner.pid) {
            return Err(unconfirmed());
        }
        let birth = crate::process::process_birth_identity(expected_owner.pid)
            .map_err(|error| {
                warn!(target: "acp.protocol", ?error, "runner peer birth observation failed");
                unconfirmed()
            })?
            .ok_or_else(unconfirmed)?;
        if Some(birth.fingerprint()) != expected_owner.birth_fingerprint {
            return Err(unconfirmed());
        }
        let peer_process = crate::process::CapturedProcess::capture(expected_owner.pid, &birth)
            .map_err(|error| {
                warn!(target: "acp.protocol", ?error, "runner peer capture failed");
                unconfirmed()
            })?
            .ok_or_else(unconfirmed)?;
        let (mut read_half, mut write_half) = stream.into_split();
        match control_protocol::read_frame(&mut read_half).await {
            Ok(Some(ControlBody::Hello {
                control_protocol_version,
                session_id,
            })) if control_protocol_version == control_protocol::CONTROL_PROTOCOL_VERSION
                && session_id == session_label => {}
            Ok(Some(ControlBody::Hello {
                control_protocol_version,
                session_id: _,
            })) => {
                warn!(target: "acp.protocol", session = %session_label, protocol = control_protocol_version, "runner Hello identity or protocol mismatch");
                let reason =
                    if control_protocol_version != control_protocol::CONTROL_PROTOCOL_VERSION {
                        crate::process::worker_registry::ControlRefusalReason::ProtocolIncompatible
                    } else {
                        crate::process::worker_registry::ControlRefusalReason::OwnershipUnconfirmed
                    };
                return Err(refused(reason, Some(control_protocol_version)));
            }
            Ok(Some(frame)) => {
                warn!(target: "acp.protocol", frame_kind = ?std::mem::discriminant(&frame), "runner sent a non-Hello frame before admission");
                return Err(unconfirmed());
            }
            Ok(None) => return Err(unconfirmed()),
            Err(error) => {
                warn!(target: "acp.protocol", ?error, "runner Hello decoding failed");
                return Err(unconfirmed());
            }
        }
        control_protocol::write_frame(
            &mut write_half,
            &ControlBody::Attach {
                control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
            },
        )
        .await
        .map_err(|error| {
            warn!(target: "acp.protocol", ?error, "runner Attach write failed");
            unconfirmed()
        })?;
        Ok((read_half, write_half, peer_process))
    };
    let (mut read_half, write_half, peer_process) = tokio::time::timeout(bound, dial)
        .await
        .map_err(|_| unconfirmed())??;

    info!(
        target: "acp.protocol",
        session = %session_label,
        "runner control channel v4 attached; runner owns the ACP protocol"
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
    let settled = Arc::new(std::sync::Mutex::new(None));
    let reader_settled = settled.clone();
    let reader_session = session_label.clone();
    let reader_shim_write = shim_write.clone();
    let reader_correlation = correlation.clone();
    let ingress = Arc::new(SessionIngress::default());
    let connection_state = Arc::new(ConnectionState::default());
    let reader_ingress = ingress.clone();
    let reader_connection_state = connection_state.clone();
    let native_cache = Arc::new(AtomicBool::new(false));
    let native_ready = Arc::new(AtomicBool::new(false));
    let reader_native_cache = native_cache.clone();
    let reader_native_ready = native_ready.clone();
    let client = Arc::new(DaemonControlClient {
        ingress,
        connection_state,
        write: write_half,
        handshake_rx: Mutex::new(hs_rx),
        sessions_established: AtomicU64::new(0),
        native_cache,
        native_ready,
        sessions_replayed: watch::Sender::new(0),
        completion,
        settled,
        raw_fd,
        peer_identity: expected_owner,
        peer_process,
    });
    tokio::spawn(async move {
        async {
            let mut native_loss_relayed = false;
            loop {
                match control_protocol::read_frame_with_size(&mut read_half).await {
                    Ok(Some((
                        frame @ (ControlBody::Initialized { .. }
                        | ControlBody::SessionReady { .. }
                        | ControlBody::HandshakeFailed { .. }),
                        _,
                    ))) => {
                        if let ControlBody::SessionReady {
                            acp_session_id,
                            native_channel,
                            ..
                        } = &frame
                        {
                            reader_native_cache.store(true, AtomicOrdering::Release);
                            if let NativeChannelAvailability::Unavailable { cause } = native_channel
                            {
                                observe_native_loss(&reader_connection_state, *cause);
                            }
                            reader_native_ready.store(
                                matches!(native_channel, NativeChannelAvailability::Available)
                                    && reader_connection_state.loss().is_none(),
                                AtomicOrdering::Release,
                            );
                            if matches!(native_channel, NativeChannelAvailability::Available) {
                                reader_connection_state.record_usable();
                            }
                            reader_ingress.resolve(None, acp_session_id.clone().into());
                        }
                        if hs_tx.send(frame).await.is_err() {
                            return;
                        }
                    }
                    Ok(Some((
                        ControlBody::NativeChannelLost {
                            cause,
                            cached_session,
                        },
                        _,
                    ))) => {
                        reader_native_cache.store(cached_session, AtomicOrdering::Release);
                        reader_native_ready.store(false, AtomicOrdering::Release);
                        observe_native_loss(&reader_connection_state, cause);
                        {
                            let mut correlation = reader_correlation.lock().await;
                            correlation.forward.clear();
                            correlation.reverse.clear();
                        }
                        if !native_loss_relayed {
                            native_loss_relayed = true;
                            let marker = NativeChannelLostMarker::default();
                            let line = serde_json::json!({
                                "jsonrpc": "2.0",
                                "method": marker.method(),
                                "params": {},
                            });
                            if !shim_write_line_releasing(
                                &reader_shim_write,
                                &line,
                                &reader_settled,
                                &reader_connection_state,
                            )
                            .await
                            {
                                return;
                            }
                        }
                    }
                    Ok(Some((ControlBody::PromptStarted { prompt_req_id }, _))) => {
                        let mut completion =
                            reader_completion.lock().expect("completion mutex poisoned");
                        if let PromptCompletion::Pending {
                            prompt_req_id: id @ None,
                            ..
                        } = &mut *completion
                        {
                            *id = Some(prompt_req_id);
                        }
                    }
                    Ok(Some((
                        ControlBody::PromptCompleted {
                            prompt_req_id,
                            outcome,
                        },
                        _,
                    ))) => {
                        let adopted = matches!(
                            *reader_completion.lock().expect("completion mutex poisoned"),
                            PromptCompletion::Adopted
                        );
                        if adopted {
                            if let Some(adopted) = prepare_adopted_completion(
                                outcome,
                                &reader_completion,
                                &reader_prompt_in_flight,
                                &terminal_claim,
                            ) {
                                let params = PromptCompletedMarker {
                                    adopted: Some(Box::new(adopted)),
                                };
                                let marker = CompletionMarkerLine {
                                    jsonrpc: "2.0",
                                    method: params.method(),
                                    params: &params,
                                };
                                if !shim_write_line_releasing(
                                    &reader_shim_write,
                                    &marker,
                                    &reader_settled,
                                    &reader_connection_state,
                                )
                                .await
                                {
                                    return;
                                }
                            }
                            continue;
                        }
                        let waiter = {
                            let mut completion =
                                reader_completion.lock().expect("completion mutex poisoned");
                            match &*completion {
                                PromptCompletion::Pending {
                                    prompt_req_id: Some(id),
                                    ..
                                } if *id == prompt_req_id => {
                                    match std::mem::replace(
                                        &mut *completion,
                                        PromptCompletion::LocalIdle,
                                    ) {
                                        PromptCompletion::Pending { tx, .. } => Some(tx),
                                        _ => unreachable!(),
                                    }
                                }
                                PromptCompletion::Adopted => continue,
                                _ => continue,
                            }
                        };
                        if let Some(tx) = waiter {
                            *reader_settled.lock().expect("settled mutex poisoned") =
                                Some((tx, outcome));
                            let params = PromptCompletedMarker::default();
                            let marker = CompletionMarkerLine {
                                jsonrpc: "2.0",
                                method: params.method(),
                                params: &params,
                            };
                            if !shim_write_line_releasing(
                                &reader_shim_write,
                                &marker,
                                &reader_settled,
                                &reader_connection_state,
                            )
                            .await
                            {
                                return;
                            }
                        }
                    }
                    // Reverse lane: served by the crate's request handlers. The
                    // ordered dispatcher starts each handler before reading the
                    // next frame, so SessionReady must publish its candidate first.
                    Ok(Some((
                        ControlBody::ServerCall {
                            call_id,
                            method,
                            params,
                        },
                        _,
                    ))) => {
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
                        if !shim_write_line_releasing(
                            &reader_shim_write,
                            &line,
                            &reader_settled,
                            &reader_connection_state,
                        )
                        .await
                        {
                            return;
                        }
                    }
                    // Preserve producer bytes across the private SDK transport
                    // without nesting or trusting any native accounting claim.
                    Ok(Some((ControlBody::Notify { method, mut params }, wire_bytes))) => {
                        // Only this daemon mints completion barriers and their classified payload.
                        if PromptCompletedMarker::matches_method(&method)
                            || NativeChannelLostMarker::matches_method(&method)
                        {
                            continue;
                        }
                        if method == "session/update" {
                            match &mut params {
                                serde_json::Value::Object(fields) => {
                                    fields.insert(
                                        CONTROL_FRAME_BYTES_FIELD.into(),
                                        wire_bytes.into(),
                                    );
                                }
                                serde_json::Value::Array(fields) => fields.push(wire_bytes.into()),
                                _ => {}
                            }
                        }
                        let mut line = serde_json::json!({"jsonrpc": "2.0"});
                        line["method"] = method.into();
                        line["params"] = params;
                        if !shim_write_line_releasing(
                            &reader_shim_write,
                            &line,
                            &reader_settled,
                            &reader_connection_state,
                        )
                        .await
                        {
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
                            if !shim_write_line_releasing(
                                &reader_shim_write,
                                &line,
                                &reader_settled,
                                &reader_connection_state,
                            )
                            .await
                            {
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
                            if !shim_write_line_releasing(
                                &reader_shim_write,
                                &line,
                                &reader_settled,
                                &reader_connection_state,
                            )
                            .await
                            {
                                return;
                            }
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        reader_connection_state.observe(
                            crate::acp::state::AcpConnectionSource::Control,
                            crate::acp::state::AcpConnectionCause::IncomingEnded,
                        );
                        return;
                    }
                    Err(e) => {
                        reader_connection_state.observe(
                            crate::acp::state::AcpConnectionSource::Control,
                            if e.downcast_ref::<std::io::Error>().is_some() {
                                crate::acp::state::AcpConnectionCause::ReadFailed
                            } else {
                                crate::acp::state::AcpConnectionCause::ProtocolEnded
                            },
                        );
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
        reader_connection_state.observe(
            crate::acp::state::AcpConnectionSource::Control,
            crate::acp::state::AcpConnectionCause::ForwardPumpEnded,
        );
        {
            let mut completion = reader_completion.lock().expect("completion mutex poisoned");
            // EOF aborts a local waiter, not an adopted marker already ordered
            // into the SDK pipe. Explicit local activation still supersedes it.
            if matches!(*completion, PromptCompletion::Pending { .. }) {
                *completion = PromptCompletion::LocalIdle;
            }
        }
        let mut shim_write = reader_shim_write.lock().await;
        let _ = shim_write.shutdown().await;
    });

    // Crate output: a line with `method` is a forward request, anything else
    // answers a reverse call.
    let pump_control = client.clone();
    let pump_shim_write = shim_write.clone();
    let pump_correlation = correlation.clone();
    let pump_session = session_label.clone();
    let pump_connection_state = client.connection_state.clone();
    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(shim_read);
        let mut line = String::new();
        loop {
            line.clear();
            match tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line).await {
                Ok(0) => {
                    pump_connection_state.observe(
                        crate::acp::state::AcpConnectionSource::Control,
                        crate::acp::state::AcpConnectionCause::ForwardPumpEnded,
                    );
                    // SAFETY: pump_write retains this exact socket.
                    unsafe { libc::shutdown(raw_fd, libc::SHUT_RDWR) };
                    return;
                }
                Ok(_) => {}
                Err(error) => {
                    pump_connection_state.observe(
                        crate::acp::state::AcpConnectionSource::Control,
                        crate::acp::state::AcpConnectionCause::ForwardPumpEnded,
                    );
                    // SAFETY: pump_write retains this exact socket.
                    unsafe { libc::shutdown(raw_fd, libc::SHUT_RDWR) };
                    debug!(
                        target: "acp.protocol",
                        session = %pump_session,
                        "shim transport read ended: {error}"
                    );
                    return;
                }
            }
            // The native-loss receipt settled application scopes. SDK-private
            // pre-loss callbacks may finish/drop later, but cannot allocate a
            // forwarded ID or re-enter native I/O after observed loss.
            if pump_connection_state.loss().is_some()
                || pump_connection_state.driver_finished()
                || pump_connection_state.is_stopping()
            {
                continue;
            }
            let Ok(mut value) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                continue;
            };
            let mut forward = None;
            let frame = if let Some(method) = value
                .get("method")
                .and_then(|method| method.as_str())
                .map(str::to_owned)
            {
                let Some(id) = value.get_mut("id").map(std::mem::take) else {
                    continue;
                };
                let mut correlation = pump_correlation.lock().await;
                let Ok(call_id) = pump_connection_state.admit(|| correlation.forward_id()) else {
                    continue;
                };
                forward = Some((call_id, id));
                ControlBody::AgentCall {
                    call_id,
                    method,
                    params: value
                        .get_mut("params")
                        .map(std::mem::take)
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
                        // The reader's teardown closes this half too, so a
                        // failure here does not mean the crate is gone.
                        if !shim_write_line(&pump_shim_write, &response).await {
                            pump_connection_state.observe(
                                crate::acp::state::AcpConnectionSource::Control,
                                crate::acp::state::AcpConnectionCause::ForwardPumpEnded,
                            );
                            // SAFETY: pump_write retains this exact socket.
                            unsafe { libc::shutdown(raw_fd, libc::SHUT_RDWR) };
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

            drop(frame);
            drop(value);
            let forwarded_call = if let Some((call_id, id)) = forward.take() {
                let mut correlation = pump_correlation.lock().await;
                if pump_connection_state
                    .admit(|| correlation.forward.insert(call_id, id))
                    .is_err()
                {
                    continue;
                }
                Some(call_id)
            } else {
                None
            };
            if pump_control.send_encoded(&wire, false).await.is_err() {
                if let Some(call_id) = forwarded_call {
                    pump_correlation.lock().await.forward.remove(&call_id);
                }
                if pump_connection_state
                    .loss()
                    .is_some_and(|loss| loss.source == AcpConnectionSource::Native)
                {
                    continue;
                }
                return;
            }
        }
    });

    Ok((client, crate_side))
}

/// Unknown successful stop reasons retain the existing completion fallback.
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
        PromptOutcome::Completed { stop_reason: None } => "prompt_complete".to_string(),
        PromptOutcome::Error { .. } | PromptOutcome::Aborted => "prompt_failed".to_string(),
    }
}

fn adopted_prompt_completion(
    outcome: control_protocol::PromptOutcome,
) -> (Option<control_protocol::AdoptedPromptDiagnostic>, String) {
    let reason = control_outcome_reason(&outcome);
    match prompt_outcome_to_response(outcome) {
        Ok(_) => (None, reason),
        Err(error) => {
            if let Some(info) = classify_rate_limit_error(&error, None) {
                (
                    control_protocol::AdoptedPromptDiagnostic::new(Event::RateLimit { info }),
                    "rate_limited".into(),
                )
            } else {
                warn!(target: "acp.protocol", error = ?error, "adopted prompt failed; preserving runner and conversation");
                (
                    control_protocol::AdoptedPromptDiagnostic::new(prompt_runtime_error(
                        &error,
                        crate::session::ErrorEvidenceSource::RunnerCompletion,
                    )),
                    "prompt_failed".into(),
                )
            }
        }
    }
}

fn prepare_adopted_completion(
    outcome: control_protocol::PromptOutcome,
    completion: &std::sync::Mutex<PromptCompletion>,
    in_flight: &std::sync::atomic::AtomicBool,
    terminal: &TerminalClaim,
) -> Option<control_protocol::AdoptedPromptCompletion> {
    let turn = {
        let completion = completion.lock().expect("completion mutex poisoned");
        if !matches!(*completion, PromptCompletion::Adopted)
            || !in_flight.load(AtomicOrdering::Relaxed)
            || !terminal.claim()
        {
            return None;
        }
        terminal
            .claimed_turn()
            .expect("adopted completion owns its turn")
    };
    let (diagnostic, reason) = adopted_prompt_completion(outcome);
    Some(control_protocol::AdoptedPromptCompletion {
        turn,
        diagnostic,
        reason,
    })
}

pub(super) async fn publish_adopted_completion(
    adopted: control_protocol::AdoptedPromptCompletion,
    events: &mpsc::Sender<Event>,
    completion: &std::sync::Mutex<PromptCompletion>,
    in_flight: &std::sync::atomic::AtomicBool,
    terminal: &TerminalClaim,
) -> bool {
    let control_protocol::AdoptedPromptCompletion {
        turn,
        diagnostic,
        reason,
    } = adopted;
    let count = 1 + usize::from(diagnostic.is_some());
    let Ok(mut permits) = events.reserve_many(count).await else {
        return false;
    };
    let completion = completion.lock().expect("completion mutex poisoned");
    // Recheck after capacity waits, then enqueue both events before local activation.
    if matches!(*completion, PromptCompletion::Adopted) {
        return terminal.publish_claimed(turn, reason == "prompt_failed", || {
            if let Some(event) = diagnostic {
                permits
                    .next()
                    .expect("diagnostic permit")
                    .send(event.into_event());
            }
            permits
                .next()
                .expect("terminal permit")
                .send(Event::Stopped { reason });
            in_flight.store(false, AtomicOrdering::Release);
        });
    }
    false
}

/// Same result shape as the crate `send_request` it replaces.
pub(super) async fn establish_session_v4<Resp: serde::de::DeserializeOwned>(
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

/// Preserve private error code/data for structured classifiers. An abort is
/// an unsuccessful operation, not a synthetic successful `end_turn`.
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
        PromptOutcome::Aborted => Err(acp_internal_error(
            "prompt aborted before completion".into(),
        )),
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

    fn fixture_owner() -> crate::acp::runner_lifecycle::RunnerIdentity {
        let pid = std::process::id();
        let birth = crate::process::process_birth_identity(pid)
            .unwrap()
            .unwrap();
        crate::acp::runner_lifecycle::RunnerIdentity {
            pid,
            generation: 1,
            birth_fingerprint: Some(birth.fingerprint()),
        }
    }

    fn register_fixture(
        session_id: &str,
        socket: &std::path::Path,
    ) -> crate::session::test_support::AppDirGuard {
        let guard = crate::session::test_support::isolate_app_dir_at(socket.parent().unwrap());
        let owner = fixture_owner();
        let mut record = crate::process::worker_registry::WorkerRecord::new(
            session_id.into(),
            owner.pid,
            socket.into(),
            "fixture".into(),
            "codex".into(),
            socket.parent().unwrap().into(),
            None,
            vec![],
            vec![],
            None,
            None,
        )
        .with_generation(owner.generation);
        record.control_protocol_version = Some(control_protocol::CONTROL_PROTOCOL_VERSION);
        crate::process::worker_registry::save(&record).unwrap();
        guard
    }

    async fn connect(
        control: &std::path::Path,
        session: &str,
        terminal: Arc<TerminalClaim>,
        in_flight: bool,
    ) -> Result<(Arc<DaemonControlClient>, tokio::io::DuplexStream), AcpError> {
        let in_flight = Arc::new(AtomicBool::new(in_flight));
        connect_runner_control_v4(
            control,
            session.into(),
            terminal,
            in_flight,
            fixture_owner(),
        )
        .await
    }

    struct PromptControlPeer {
        client: Arc<DaemonControlClient>,
        peer: UnixStream,
        transport: BufReader<tokio::io::DuplexStream>,
        events: mpsc::Receiver<Event>,
        event_tx: mpsc::Sender<Event>,
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
                connect_runner_control_v4(
                    &socket,
                    "prompt".into(),
                    terminal.clone(),
                    in_flight.clone(),
                    fixture_owner(),
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
                event_tx,
                terminal,
                in_flight,
            }
        }

        async fn send(&mut self, frame: ControlBody) {
            control_protocol::write_frame(&mut self.peer, &frame)
                .await
                .unwrap();
        }

        async fn prompt(&mut self) -> oneshot::Receiver<ControlPromptResult> {
            let rx = self
                .client
                .prompt(serde_json::json!({}))
                .await
                .expect("fixture prompt is admitted");
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
        /// so this is a deterministic barrier. Completion markers ahead of it
        /// are handled as the crate would.
        async fn drain(&mut self) {
            self.send(ControlBody::Notify {
                method: "test/barrier".into(),
                params: serde_json::json!({}),
            })
            .await;
            loop {
                let mut line = String::new();
                tokio::time::timeout(Duration::from_secs(2), self.transport.read_line(&mut line))
                    .await
                    .expect("control reader must reach the notification barrier")
                    .unwrap();
                let notification: serde_json::Value = serde_json::from_str(&line).unwrap();
                if notification["method"] == "test/barrier" {
                    return;
                }
                assert_eq!(notification["method"], "_aoe/prompt_completed");
                let marker: PromptCompletedMarker =
                    serde_json::from_value(notification["params"].clone()).unwrap();
                if let Some(adopted) = marker.adopted {
                    publish_adopted_completion(
                        *adopted,
                        &self.event_tx,
                        &self.client.completion,
                        &self.in_flight,
                        &self.terminal,
                    )
                    .await;
                } else {
                    self.client
                        .deliver_prompt_completion(PromptCompletedMarker::default());
                }
            }
        }

        /// Drop the crate side so a reader relay fails as it would for a dead
        /// transport. The replacement only keeps the field typed.
        fn close_crate_side(&mut self) {
            let (dead, peer) = tokio::io::duplex(64);
            drop(peer);
            self.transport = BufReader::new(dead);
        }

        /// Wait until the reader has settled the matched outcome, which a
        /// relayed marker alone never resolves.
        async fn await_settled_outcome(
            &self,
            completion: &mut oneshot::Receiver<ControlPromptResult>,
        ) {
            tokio::time::timeout(Duration::from_secs(2), async {
                while self
                    .client
                    .settled
                    .lock()
                    .expect("settled mutex poisoned")
                    .is_none()
                {
                    assert_eq!(
                        completion.try_recv(),
                        Err(oneshot::error::TryRecvError::Empty),
                        "the waiter must stay open and unresolved until the reader settles the outcome"
                    );
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the reader must settle the matched outcome");
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
    async fn historical_completions_preserve_local_terminal_ownership() {
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
        assert_eq!(
            completion.await.unwrap(),
            ControlPromptResult::Outcome(end("end_turn"))
        );
        peer.assert_local_terminal_ownership();

        {
            let mut peer = PromptControlPeer::new().await;
            let completion = peer.prompt().await;
            peer.send(ControlBody::PromptStarted { prompt_req_id: 9 })
                .await;
            peer.completed(9, "end_turn").await;
            peer.completed(7, "cancelled").await;
            peer.drain().await;
            peer.assert_local_terminal_ownership();
            assert_eq!(
                completion.await.unwrap(),
                ControlPromptResult::Outcome(end("end_turn"))
            );
        }

        {
            let mut peer = PromptControlPeer::new().await;
            peer.client.supersede_adopted_turn();
            peer.completed(7, "cancelled").await;
            peer.drain().await;
            peer.assert_local_terminal_ownership();
        }
    }

    #[tokio::test]
    async fn adopted_request_errors_preserve_diagnostics_and_next_local_prompt() {
        for (message, kind) in [
            ("provider connection failure", "provider_error"),
            ("invalid credentials", "auth_error"),
            ("tool failure", "tool_error"),
            ("unknown failure", "unknown"),
        ] {
            let mut peer = PromptControlPeer::new().await;
            let error = PromptOutcome::Error {
                code: -32603,
                message: message.into(),
                data: Some(serde_json::json!({"errorKind": kind, "privateTokenMarker": "secret"})),
            };
            peer.send(ControlBody::PromptCompleted {
                prompt_req_id: 7,
                outcome: error.clone(),
            })
            .await;
            peer.drain().await;
            let failure = peer
                .events
                .try_recv()
                .expect("diagnostic precedes terminal");
            match &failure {
                Event::PromptRuntimeError {
                    message: got,
                    evidence: Some(evidence),
                    ..
                } => {
                    assert_eq!(got, message);
                    assert_eq!(
                        evidence.source,
                        crate::session::ErrorEvidenceSource::RunnerCompletion
                    );
                    assert_eq!(evidence.runtime, crate::session::RuntimeEvidence::Unknown);
                    assert_eq!(evidence.origin, None);
                    assert_eq!(evidence.retryable, None);
                }
                event => panic!("expected operation error, got {event:?}"),
            }
            assert!(!serde_json::to_string(&failure)
                .unwrap()
                .contains("privateTokenMarker"));
            assert!(
                matches!(peer.events.try_recv(), Ok(Event::Stopped { reason }) if reason == "prompt_failed")
            );
            assert!(peer.terminal.claimed());
            assert!(!peer.in_flight.load(AtomicOrdering::Relaxed));
            peer.client.supersede_adopted_turn();
            peer.terminal.begin_turn();
            peer.in_flight.store(true, AtomicOrdering::Relaxed);
            let mut next = peer.prompt().await;
            peer.send(ControlBody::PromptStarted { prompt_req_id: 9 })
                .await;
            peer.send(ControlBody::PromptCompleted {
                prompt_req_id: 7,
                outcome: error,
            })
            .await;
            peer.drain().await;
            assert_eq!(next.try_recv(), Err(oneshot::error::TryRecvError::Empty));
            peer.assert_local_terminal_ownership();
            peer.completed(9, "end_turn").await;
            peer.drain().await;
            assert_eq!(
                next.await.unwrap(),
                ControlPromptResult::Outcome(end("end_turn"))
            );
            peer.assert_local_terminal_ownership();
        }
        let mut peer = PromptControlPeer::new().await;
        peer.send(ControlBody::PromptCompleted {
            prompt_req_id: 7,
            outcome: PromptOutcome::Aborted,
        })
        .await;
        peer.drain().await;
        assert!(matches!(
            peer.events.try_recv(),
            Ok(Event::PromptRuntimeError { .. })
        ));
        assert!(
            matches!(peer.events.try_recv(), Ok(Event::Stopped { reason }) if reason == "prompt_failed")
        );
        assert!(prompt_outcome_to_response(PromptOutcome::Aborted).is_err());
    }

    #[tokio::test]
    async fn adopted_error_waiting_for_capacity_cannot_close_a_new_local_turn() {
        use futures_util::FutureExt as _;
        let peer = PromptControlPeer::new().await;
        let (events, mut received) = mpsc::channel(2);
        events.send(Event::ThinkingStarted).await.unwrap();
        let adopted = prepare_adopted_completion(
            PromptOutcome::Error {
                code: -32603,
                message: "old failure".into(),
                data: None,
            },
            &peer.client.completion,
            &peer.in_flight,
            &peer.terminal,
        )
        .unwrap();
        let delivery = publish_adopted_completion(
            adopted,
            &events,
            &peer.client.completion,
            &peer.in_flight,
            &peer.terminal,
        );
        tokio::pin!(delivery);
        assert!(
            delivery.as_mut().now_or_never().is_none(),
            "the error pair is blocked on capacity"
        );
        peer.client.supersede_adopted_turn();
        peer.terminal.begin_turn();
        peer.in_flight.store(true, AtomicOrdering::Relaxed);
        assert!(matches!(
            received.recv().await,
            Some(Event::ThinkingStarted)
        ));
        assert!(!delivery.await);
        assert!(matches!(
            received.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(peer.in_flight.load(AtomicOrdering::Relaxed));
        assert!(!peer.terminal.claimed());
    }

    #[tokio::test]
    async fn adopted_error_claims_terminal_before_waiting_for_event_capacity() {
        use futures_util::FutureExt as _;
        let peer = PromptControlPeer::new().await;
        let (events, mut received) = mpsc::channel(2);
        events.send(Event::ThinkingStarted).await.unwrap();
        let adopted = prepare_adopted_completion(
            PromptOutcome::Error {
                code: -32603,
                message: "request failed".into(),
                data: None,
            },
            &peer.client.completion,
            &peer.in_flight,
            &peer.terminal,
        )
        .unwrap();
        let delivery = publish_adopted_completion(
            adopted,
            &events,
            &peer.client.completion,
            &peer.in_flight,
            &peer.terminal,
        );
        tokio::pin!(delivery);
        assert!(delivery.as_mut().now_or_never().is_none());
        assert!(
            !peer.terminal.claim(),
            "idle fallback cannot replace a received completion"
        );
        assert!(
            peer.in_flight.load(AtomicOrdering::Relaxed),
            "adoption owns buffered notifications until publication commits"
        );
        received.recv().await.unwrap();
        assert!(delivery.await);
        assert!(matches!(
            received.recv().await,
            Some(Event::PromptRuntimeError { .. })
        ));
        assert!(
            matches!(received.recv().await, Some(Event::Stopped { reason }) if reason == "prompt_failed")
        );
    }

    #[tokio::test]
    async fn duplicate_local_prompt_preserves_first_waiter_and_sends_no_prompt() {
        let mut peer = PromptControlPeer::new().await;
        let completion = peer.prompt().await;
        let duplicate = peer
            .client
            .prompt(serde_json::json!({"duplicate": true}))
            .await;
        assert!(matches!(duplicate, Err(AcpError::Protocol(_))));
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
        assert_eq!(
            completion.await.unwrap(),
            ControlPromptResult::Outcome(end("cancelled"))
        );
        peer.assert_local_terminal_ownership();
    }

    /// Relay loss cannot erase the already received, correlated completion.
    /// The separate connection loss still fences every further native control.
    #[tokio::test]
    async fn failed_marker_relay_preserves_completed_result_without_further_native_controls() {
        let mut peer = PromptControlPeer::new().await;
        let completion = peer.prompt().await;
        peer.send(ControlBody::PromptStarted { prompt_req_id: 9 })
            .await;
        peer.close_crate_side();
        peer.completed(9, "end_turn").await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), completion)
                .await
                .expect("a completion the crate never receives must settle")
                .unwrap(),
            ControlPromptResult::Outcome(end("end_turn"))
        );
        assert_eq!(
            peer.client.connection_state.loss().unwrap().cause,
            crate::acp::state::AcpConnectionCause::ForwardPumpEnded
        );
        peer.client.cancel().await;
        assert!(matches!(
            peer.client.prompt(serde_json::json!({})).await,
            Err(AcpError::ConnectionUnavailable(_))
        ));
        let mut byte = [0_u8; 1];
        match peer.peer.try_read(&mut byte) {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Ok(0) => {}
            other => panic!("an unavailable SDK route must send no native controls: {other:?}"),
        }
    }

    /// A marker already buffered for the crate stays deliverable, so the
    /// reader's teardown must not release the sender: the turn would end as
    /// aborted, losing its real stop reason and any rate-limit parking.
    #[tokio::test]
    async fn runner_hangup_still_delivers_a_relayed_outcome() {
        let mut peer = PromptControlPeer::new().await;
        let mut completion = peer.prompt().await;
        peer.send(ControlBody::PromptStarted { prompt_req_id: 9 })
            .await;
        // No crate reads this peer's transport, so the relayed marker stays
        // undispatched.
        peer.completed(9, "end_turn").await;
        peer.await_settled_outcome(&mut completion).await;
        // Hang the runner up so the reader's teardown runs, crate side up.
        peer.peer.shutdown().await.unwrap();
        // Reading to EOF drains the buffer ahead of the shutdown, and only the
        // reader's teardown produces that EOF, so this waits out the teardown
        // rather than racing it.
        let mut relayed = String::new();
        tokio::time::timeout(
            Duration::from_secs(2),
            peer.transport.read_to_string(&mut relayed),
        )
        .await
        .expect("a relayed marker must reach the crate")
        .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(relayed.trim())
                .unwrap()
                .get("method")
                .unwrap(),
            &serde_json::json!(PromptCompletedMarker::default().method())
        );
        peer.client
            .deliver_prompt_completion(PromptCompletedMarker::default());
        assert_eq!(
            completion.await.unwrap(),
            ControlPromptResult::Outcome(end("end_turn"))
        );
    }

    /// Failure of a later relay likewise preserves the genuine settled outcome
    /// while retiring the local route, rather than reclassifying completion as loss.
    #[tokio::test]
    async fn failed_relay_after_a_relayed_marker_preserves_completed_result() {
        let mut peer = PromptControlPeer::new().await;
        let mut completion = peer.prompt().await;
        peer.send(ControlBody::PromptStarted { prompt_req_id: 9 })
            .await;
        peer.completed(9, "end_turn").await;
        peer.await_settled_outcome(&mut completion).await;
        // Reading the marker proves the relay that wrote it returned, so the
        // notification below is the relay that fails.
        let mut relayed = String::new();
        tokio::time::timeout(
            Duration::from_secs(2),
            peer.transport.read_line(&mut relayed),
        )
        .await
        .expect("the relayed marker must reach the crate")
        .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(relayed.trim())
                .unwrap()
                .get("method")
                .unwrap(),
            &serde_json::json!(PromptCompletedMarker::default().method())
        );
        peer.close_crate_side();
        peer.send(ControlBody::Notify {
            method: "test/after".into(),
            params: serde_json::json!({}),
        })
        .await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), completion)
                .await
                .expect("an unreachable crate must settle the waiter")
                .unwrap(),
            ControlPromptResult::Outcome(end("end_turn"))
        );
        assert_eq!(
            peer.client.connection_state.loss().unwrap().cause,
            crate::acp::state::AcpConnectionCause::ForwardPumpEnded
        );
        peer.client.cancel().await;
        assert!(matches!(
            peer.client.prompt(serde_json::json!({})).await,
            Err(AcpError::ConnectionUnavailable(_))
        ));
        let mut byte = [0_u8; 1];
        match peer.peer.try_read(&mut byte) {
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Ok(0) => {}
            other => panic!("an unavailable SDK route must send no native controls: {other:?}"),
        }
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

        let (_, crate_side) = connect(&control, "oversize", Arc::new(TerminalClaim::new()), false)
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
        let (client, crate_side) = connect(&control, "eof", Arc::new(TerminalClaim::new()), false)
            .await
            .unwrap();
        let completion = client
            .prompt(serde_json::json!({}))
            .await
            .expect("prompt is admitted before EOF");
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

    #[tokio::test]
    #[serial_test::serial]
    async fn attached_prompt_errors_never_rebase_a_successfully_resumed_session() {
        use crate::acp::acp_client::AcpClient;
        use crate::acp::state::AcpSessionId;

        for message in ["Unsupported ACP session", "Unsupported session mode"] {
            let tmp = tempfile::tempdir().unwrap();
            let socket = tmp.path().join("s.sock");
            let _home = register_fixture("resume", &socket);
            let (_, listener) = bind(&tmp);
            let runner = async {
                let mut peer = accept_hello(
                    &listener,
                    "resume",
                    control_protocol::CONTROL_PROTOCOL_VERSION,
                )
                .await;
                let (mut prompts, mut initializes, mut resumes) = (0, 0, 0);
                while let Some(frame) = control_protocol::read_frame(&mut peer).await.unwrap() {
                    let reply = match frame {
                        ControlBody::Attach { .. } => continue,
                        ControlBody::Initialize { .. } => {
                            initializes += 1;
                            ControlBody::Initialized {
                                result: serde_json::json!({"protocolVersion":1,"agentCapabilities":{}}),
                            }
                        }
                        ControlBody::ResumeSession => {
                            resumes += 1;
                            ControlBody::SessionReady {
                                acp_session_id: "sid-stored".into(),
                                result: serde_json::json!({}),
                                native_channel:
                                    control_protocol::NativeChannelAvailability::Available,
                            }
                        }
                        ControlBody::Prompt { request } => {
                            assert_eq!(request["sessionId"], "sid-stored");
                            prompts += 1;
                            control_protocol::write_frame(
                                &mut peer,
                                &ControlBody::PromptStarted {
                                    prompt_req_id: prompts,
                                },
                            )
                            .await
                            .unwrap();
                            ControlBody::PromptCompleted {
                                prompt_req_id: prompts,
                                outcome: if prompts == 1 {
                                    PromptOutcome::Error {
                                        code: -32603,
                                        message: message.into(),
                                        data: Some(serde_json::json!({
                                            "reason":"incoming_transport_closed",
                                            "privateTokenMarker":"must-not-leak"
                                        })),
                                    }
                                } else {
                                    end("end_turn")
                                },
                            }
                        }
                        frame => panic!("unexpected resumed-session request: {frame:?}"),
                    };
                    control_protocol::write_frame(&mut peer, &reply)
                        .await
                        .unwrap();
                }
                assert_eq!((initializes, resumes, prompts), (1, 1, 2));
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
                    None,
                    None,
                    None,
                )
                .await
                .unwrap();
                for (text, expected_reason) in [
                    ("continue", "prompt_failed"),
                    ("explicit next request", "prompt_complete"),
                ] {
                    client.send_prompt(text, &[]).await.unwrap();
                    let mut diagnostics = 0;
                    loop {
                        let event = client
                            .next_event()
                            .await
                            .expect("the same SDK/control connection remains live");
                        assert!(!serde_json::to_string(&event)
                            .unwrap()
                            .contains("privateTokenMarker"));
                        match event {
                            Event::SessionContextReset { .. } => {
                                panic!("ordinary request prose must not rebase")
                            }
                            Event::AgentStartupError { message: error, .. } => panic!("{error}"),
                            Event::AcpConnectionUnavailable { .. } => {
                                panic!("peer data is not an EOF observation")
                            }
                            Event::PromptRuntimeError { message: error, .. } => {
                                assert_eq!(expected_reason, "prompt_failed");
                                assert_eq!(error, message);
                                diagnostics += 1;
                            }
                            Event::Stopped { reason } => {
                                assert_eq!(reason, expected_reason);
                                assert_eq!(
                                    diagnostics,
                                    usize::from(expected_reason == "prompt_failed")
                                );
                                break;
                            }
                            _ => {}
                        }
                    }
                }
                client.shutdown().await.unwrap();
            };
            tokio::time::timeout(Duration::from_secs(10), async {
                tokio::join!(runner, daemon);
            })
            .await
            .expect("same-runtime request recovery completes");
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn native_identity_reattach_uses_runner_id_and_drains_adopted_updates() {
        use crate::acp::acp_client::AcpClient;
        use crate::acp::state::AcpSessionId;
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("native.sock");
        let _home = register_fixture("native-resume", &socket);
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
                            &ControlBody::SessionReady { acp_session_id: "actual".into(), result: serde_json::json!({}), native_channel: crate::acp::control_protocol::NativeChannelAvailability::Available },
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
                None,
                None,
                None,
            )
            .await
            .unwrap();
            let mut chunks = Vec::new();
            let mut resets = 0;
            let mut assigned = false;
            while chunks.len() < 204 || !assigned {
                match client.next_event().await.unwrap() {
                    Event::AgentMessageChunk { text, .. } => chunks.push(text),
                    Event::SessionContextReset { .. } => resets += 1,
                    Event::AcpSessionAssigned { .. } => assigned = true,
                    Event::AgentStartupError { message, .. } => panic!("{message}"),
                    _ => {}
                }
            }
            callbacks_ready.await.unwrap();
            client.shutdown().await.unwrap();
            assert_eq!(resets, 1, "a different runner id announces one reset");
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
    #[serial_test::serial]
    async fn same_id_reattach_announces_no_reset() {
        use crate::acp::acp_client::AcpClient;
        use crate::acp::state::AcpSessionId;
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("same-id.sock");
        let _home = register_fixture("same-id", &socket);
        let control = crate::process::worker::control_socket_sibling(&socket);
        let listener = tokio::net::UnixListener::bind(control).unwrap();
        let runner = async {
            let (mut peer, _) = listener.accept().await.unwrap();
            control_protocol::write_frame(
                &mut peer,
                &ControlBody::Hello {
                    control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
                    session_id: "same-id".into(),
                },
            )
            .await
            .unwrap();
            while let Some(frame) = control_protocol::read_frame(&mut peer).await.unwrap() {
                match frame {
                    ControlBody::Attach { .. } => {}
                    ControlBody::Initialize { .. } => {
                        control_protocol::write_frame(&mut peer, &ControlBody::Initialized {
                            result: serde_json::json!({"protocolVersion":1,"agentCapabilities":{}}),
                        }).await.unwrap();
                    }
                    ControlBody::ResumeSession => {
                        control_protocol::write_frame(
                            &mut peer,
                            &ControlBody::SessionReady { acp_session_id: "actual".into(), result: serde_json::json!({}), native_channel: crate::acp::control_protocol::NativeChannelAvailability::Available },
                        )
                        .await
                        .unwrap();
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
                "actual".into(),
                true,
                AcpSessionId("same-id".into()),
                None,
                "codex".into(),
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();
            // The reset, when emitted, precedes the assignment.
            loop {
                match client.next_event().await.unwrap() {
                    Event::SessionContextReset { .. } => panic!("same id must not reset"),
                    Event::AcpSessionAssigned { .. } => break,
                    Event::AgentStartupError { message, .. } => panic!("{message}"),
                    _ => {}
                }
            }
            client.shutdown().await.unwrap();
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(runner, daemon);
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn native_identity_reattach_preserves_a_full_wire_byte_backlog() {
        use crate::acp::acp_client::AcpClient;
        use crate::acp::state::AcpSessionId;
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("byte-backlog.sock");
        let _home = register_fixture("byte-backlog", &socket);
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
                            &ControlBody::SessionReady { acp_session_id: "s".into(), result: serde_json::json!({}), native_channel: crate::acp::control_protocol::NativeChannelAvailability::Available },
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
                None,
                None,
                None,
            )
            .await
            .expect("producer-admitted backlog must attach");
            let mut calls = Vec::new();
            while calls.len() < 3 {
                match client.next_event().await.unwrap() {
                    Event::ToolCallStarted { tool_call } => calls.push(tool_call.id),
                    Event::AgentStartupError { message, .. } => panic!("{message}"),
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
        for (outcome, want) in [
            (end("end_turn"), vec!["stopped:prompt_complete"]),
            (
                PromptOutcome::Error {
                    code: -32000,
                    message: "rate limit exceeded".into(),
                    data: Some(serde_json::json!({"errorKind": "rate_limit"})),
                },
                vec!["rate_limit", "stopped:rate_limited"],
            ),
        ] {
            let mut peer = PromptControlPeer::new().await;
            peer.send(ControlBody::PromptCompleted {
                prompt_req_id: 5,
                outcome,
            })
            .await;
            peer.drain().await;
            let mut got = Vec::new();
            while let Ok(event) = peer.events.try_recv() {
                got.push(match event {
                    Event::RateLimit { .. } => "rate_limit".to_string(),
                    Event::Stopped { reason } => format!("stopped:{reason}"),
                    other => panic!("unexpected event {other:?}"),
                });
            }
            assert_eq!(got, want);
            assert!(peer.terminal.claimed());
            assert!(!peer.in_flight.load(AtomicOrdering::Relaxed));
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
        use crate::process::worker_registry::ControlRefusalReason;
        for (path, expected) in [
            (&mismatch, ControlRefusalReason::ProtocolIncompatible),
            (&absent, ControlRefusalReason::OwnershipUnconfirmed),
            (&overlong, ControlRefusalReason::OwnershipUnconfirmed),
        ] {
            let guard = Arc::new(TerminalClaim::new());
            let Err(error) = connect(path, "s", guard.clone(), false).await else {
                panic!("attach to {} must fail", path.display());
            };
            let AcpError::RetainedRuntimeUnavailable(refusal) = error else {
                panic!("attach failures must preserve typed non-destructive refusal");
            };
            assert_eq!(refusal.reason, expected);
            assert_eq!(refusal.owner, Some(fixture_owner()));
            assert!(!guard.claimed());
        }
        let _ = fake.await;
    }

    /// A load must not count as replayed until the crate has applied the replay
    /// sent after the session reply (#4016), and a prompt must not resolve
    /// before the updates sent ahead of its completion, such as a rate-limit
    /// reset, have been applied.
    #[tokio::test]
    async fn barriers_wait_for_updates_applied_by_the_crate() {
        use super::super::session_identity::SessionIngressNotification;
        use agent_client_protocol::{ByteStreams, Client};
        use futures_util::FutureExt as _;
        use std::sync::atomic::{AtomicBool, AtomicUsize};
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
                    ControlBody::Prompt { .. } => {
                        for body in [
                            ControlBody::PromptStarted { prompt_req_id: 1 },
                            ControlBody::Notify {
                                method: "session/update".into(),
                                params: serde_json::json!({
                                    "sessionId": "s",
                                    "update": {
                                        "sessionUpdate": "usage_update",
                                        "used": 100,
                                        "size": 200,
                                        "_meta": {
                                            "_claude/rateLimit": {
                                                "status": "rejected",
                                                "rateLimitType": "five_hour",
                                                "resetsAt": 4_102_444_800_i64
                                            }
                                        }
                                    }
                                }),
                            },
                            ControlBody::PromptCompleted {
                                prompt_req_id: 1,
                                outcome: end("end_turn"),
                            },
                        ] {
                            control_protocol::write_frame(&mut peer, &body)
                                .await
                                .unwrap();
                        }
                    }
                    ControlBody::EstablishSession { .. } => {
                        for body in [
                            ControlBody::SessionReady { acp_session_id: "s".into(), result: serde_json::json!({}), native_channel: crate::acp::control_protocol::NativeChannelAvailability::Available },
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
            let (client, crate_side) = connect_runner_control_v4(
                &control,
                "replayed".into(),
                Arc::new(TerminalClaim::new()),
                Arc::new(AtomicBool::new(false)),
                fixture_owner(),
            )
            .await
            .unwrap();
            // Each update blocks its handler until the test releases it.
            let (gate_tx, gate_rx) =
                mpsc::unbounded_channel::<(oneshot::Sender<()>, oneshot::Receiver<()>)>();
            let gate = Arc::new(Mutex::new(gate_rx));
            let arm = || {
                let (entered_tx, entered_rx) = oneshot::channel::<()>();
                let (release_tx, release_rx) = oneshot::channel::<()>();
                gate_tx.send((entered_tx, release_rx)).unwrap();
                (entered_rx, release_tx)
            };
            let applied = Arc::new(AtomicUsize::new(0));
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
                                    SessionIngressNotification::PromptCompleted(marker) => {
                                        control.deliver_prompt_completion(marker);
                                    }
                                    SessionIngressNotification::NativeLost(_) => {
                                        panic!(
                                            "the healthy replay fixture must not lose native I/O"
                                        );
                                    }
                                    SessionIngressNotification::AuthStatus(_) => {}
                                    SessionIngressNotification::Update(_) => {
                                        let (entered, release) =
                                            gate.lock().await.recv().await.unwrap();
                                        entered.send(()).unwrap();
                                        release.await.unwrap();
                                        applied.fetch_add(1, AtomicOrdering::Relaxed);
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
                        let (entered_rx, release_tx) = arm();
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
                        assert_eq!(applied.load(AtomicOrdering::Relaxed), 1);

                        let (entered_rx, release_tx) = arm();
                        let mut completion = client
                            .prompt(serde_json::json!({}))
                            .await
                            .expect("fixture prompt is admitted");
                        entered_rx.await.unwrap();
                        // Wait for the reader to match the completion while the
                        // crate is still applying the update sent before it.
                        while client.settled.lock().unwrap().is_none() {
                            assert_eq!(
                                completion.try_recv(),
                                Err(oneshot::error::TryRecvError::Empty),
                                "a prompt must not resolve before its updates are applied"
                            );
                            tokio::task::yield_now().await;
                        }
                        release_tx.send(()).unwrap();
                        assert_eq!(
                            completion.await.unwrap(),
                            ControlPromptResult::Outcome(end("end_turn"))
                        );
                        assert_eq!(applied.load(AtomicOrdering::Relaxed), 2);
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
    async fn send_adopted_backlog(peer: &mut PromptControlPeer) {
        for index in 0..12 {
            peer.send(ControlBody::Notify {
                method: "session/update".into(),
                params: serde_json::json!({
                    "sessionId":"sid-adopted", "update":{
                        "sessionUpdate":"agent_message_chunk",
                        "content":{"type":"text","text":format!("prior output {index}\n")}
                    }
                }),
            })
            .await;
        }
        peer.send(ControlBody::PromptCompleted {
            prompt_req_id: 7,
            outcome: PromptOutcome::Error {
                code: -32603,
                message: "adopted request failed".into(),
                data: Some(serde_json::json!({"privateTokenMarker":"must-not-leak"})),
            },
        })
        .await;
    }

    #[tokio::test]
    async fn adopted_failure_barrier_orders_buffered_notifications_and_bootstrap_commit() {
        use crate::acp::acp_client::commands::{ClientCmd, ConnectMode};
        use crate::acp::acp_client::connection::{
            run_connection_task, ConnectionParams, RunnerLink,
        };
        use crate::acp::acp_client::SessionResources;
        use crate::acp::agent_compat::ExpectedAgent;
        use crate::acp::fs_handler::FsPolicy;
        use crate::acp::terminal_handler::TerminalManager;
        use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
        for (cached_before_initialize, fail_handshake, close_control, established_first) in [
            (true, false, false, false),
            (false, false, false, false),
            (true, true, false, false),
            (true, false, true, false),
            (false, false, true, false),
            (false, false, true, true),
        ] {
            let mut peer = PromptControlPeer::new().await;
            let control = peer.client.clone();
            let commit = if established_first {
                None
            } else {
                Some(control.ingress.fence.write().await)
            };
            let adopted_turn = peer.terminal.current_turn();
            if cached_before_initialize {
                send_adopted_backlog(&mut peer).await;
            }
            let cwd = tempfile::tempdir().unwrap();
            let (ready_tx, ready_rx) = oneshot::channel();
            let (commands, cmd_rx) = mpsc::channel(4);
            let (replacement, discarded) = tokio::io::duplex(1);
            drop(discarded);
            let transport =
                std::mem::replace(&mut peer.transport, BufReader::new(replacement)).into_inner();
            let (read, write) = tokio::io::split(transport);
            let shutdown = tokio_util::sync::CancellationToken::new();
            let task = tokio::spawn(run_connection_task(
                agent_client_protocol::ByteStreams::new(write.compat_write(), read.compat()),
                ConnectionParams {
                    connection_state: Arc::clone(&control.connection_state),
                    claude_home: None,
                    event_tx: peer.event_tx.clone(),
                    cmd_rx,
                    child: None,
                    pending_responders: Arc::new(Mutex::new(HashMap::new())),
                    resources: SessionResources {
                        fs_policy: Arc::new(FsPolicy::new(vec![cwd.path().to_path_buf()])),
                        terminals: TerminalManager::new(),
                        cwd: cwd.path().to_path_buf(),
                        label: "prompt".into(),
                        sandbox: None,
                    },
                    mode: ConnectMode::Resume {
                        acp_session_id: "sid-adopted".into(),
                        in_flight_turn: true,
                    },
                    ready_tx,
                    profile: &crate::acp::agent_profiles::GEMINI,
                    expected_agent: ExpectedAgent::Gemini,
                    source_profile: None,
                    default_effort: None,
                    default_mode: None,
                    default_model: None,
                    mcp_servers: Vec::new(),
                    runner: Some(RunnerLink {
                        control: control.clone(),
                        terminal_claim: peer.terminal.clone(),
                        prompt_in_flight: peer.in_flight.clone(),
                    }),
                    cancellation: tokio_util::sync::CancellationToken::new(),
                    shutdown: shutdown.clone(),
                },
            ));
            let initialize = tokio::time::timeout(
                Duration::from_secs(10),
                control_protocol::read_frame(&mut peer.peer),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(matches!(initialize, Some(ControlBody::Initialize { .. })));
            if fail_handshake {
                peer.send(ControlBody::HandshakeFailed {
                    error: serde_json::json!({"code":-32603,"message":"initialize failed","data":{"privateTokenMarker":"must-not-leak"}}),
                }).await;
                let error = ready_rx.await.unwrap().unwrap_err();
                assert!(!error.to_string().contains("privateTokenMarker"));
                tokio::time::timeout(Duration::from_secs(10), task)
                    .await
                    .unwrap()
                    .unwrap();
                continue;
            }
            peer.send(ControlBody::Initialized {
                result: serde_json::json!({"protocolVersion":1,"agentCapabilities":{}}),
            })
            .await;
            let resume = tokio::time::timeout(
                Duration::from_secs(10),
                control_protocol::read_frame(&mut peer.peer),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(matches!(resume, Some(ControlBody::ResumeSession)));
            ready_rx.await.unwrap().unwrap();
            let mut events = Vec::new();
            if !established_first {
                while peer
                    .event_tx
                    .try_send(Event::AuthStatusUpdated { status: None })
                    .is_ok()
                {}
            }
            peer.send(ControlBody::SessionReady {
                acp_session_id: "sid-adopted".into(),
                result: serde_json::json!({}),
                native_channel: crate::acp::control_protocol::NativeChannelAvailability::Available,
            })
            .await;
            if established_first {
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let event = peer.events.recv().await.unwrap();
                        let assigned = matches!(event, Event::AcpSessionAssigned { .. });
                        events.push(event);
                        if assigned {
                            break;
                        }
                    }
                })
                .await
                .unwrap();
                while peer
                    .event_tx
                    .try_send(Event::AuthStatusUpdated { status: None })
                    .is_ok()
                {}
            }
            assert_eq!(
                peer.event_tx.capacity(),
                0,
                "public queue is full before adopted completion"
            );
            if !cached_before_initialize {
                send_adopted_backlog(&mut peer).await;
            }
            tokio::time::timeout(Duration::from_secs(10), async {
                while !peer.terminal.claimed() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(
                peer.in_flight.load(AtomicOrdering::Acquire),
                "buffered notifications still belong to the adopted operation"
            );
            if close_control {
                peer.peer.shutdown().await.unwrap();
                // Only the reader owns hs_tx. Closed proves EOF cleanup finished
                // while publication is still blocked. Do not consume SessionReady.
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let ended = {
                            let receiver = control.handshake_rx.lock().await;
                            receiver.is_closed()
                        };
                        if ended {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                assert!(
                    matches!(
                        *control.completion.lock().unwrap(),
                        PromptCompletion::Adopted
                    ),
                    "EOF must not supersede the already-ordered adopted marker"
                );
            }
            drop(commit);
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let event = peer.events.recv().await.unwrap();
                    let stopped = matches!(event, Event::Stopped { .. });
                    events.push(event);
                    if stopped {
                        break;
                    }
                }
            })
            .await
            .unwrap();
            let diagnostic = events
                .iter()
                .position(|event| matches!(event, Event::PromptRuntimeError { .. }))
                .unwrap();
            assert_eq!(
                events[..diagnostic]
                    .iter()
                    .filter(|event| matches!(event, Event::AgentMessageChunk { .. }))
                    .count(),
                12
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, Event::Stopped { .. }))
                    .count(),
                1
            );
            assert!(
                matches!(events.last(), Some(Event::Stopped { reason }) if reason == "prompt_failed")
            );
            assert_eq!(
                peer.terminal.current_turn(),
                adopted_turn,
                "prior buffered output cannot invent another main-turn epoch"
            );
            assert!(!peer.in_flight.load(AtomicOrdering::Acquire));
            assert!(!serde_json::to_string(&events)
                .unwrap()
                .contains("privateTokenMarker"));
            if close_control {
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let event = peer.events.recv().await.unwrap();
                        let fatal = matches!(event, Event::AcpConnectionUnavailable { .. });
                        events.push(event);
                        if fatal {
                            break;
                        }
                    }
                })
                .await
                .unwrap();
                // The genuine control peer is this still-live test process.
                // Socket EOF retires its route, not its captured process custody;
                // only explicit stop permits joining the retained connection task.
                assert_eq!(control.peer_identity(), fixture_owner());
                assert!(!control.peer_process.exited().unwrap());
                assert!(!task.is_finished());
                control.cancel().await;
                assert!(matches!(
                    control.prompt(serde_json::json!({})).await,
                    Err(AcpError::ConnectionUnavailable(_))
                ));
                shutdown.cancel();
                tokio::time::timeout(Duration::from_secs(10), task)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(!control.peer_process.exited().unwrap());
                while let Ok(event) = peer.events.try_recv() {
                    events.push(event);
                }
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| matches!(event, Event::PromptRuntimeError { .. }))
                        .count(),
                    1
                );
                assert_eq!(events.iter().filter(|event| matches!(event, Event::Stopped { reason } if reason == "prompt_failed")).count(), 1);
                assert!(!events.iter().any(|event| matches!(event, Event::Stopped { reason } if reason == "prompt_complete")));
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| matches!(event, Event::AcpConnectionUnavailable { .. }))
                        .count(),
                    1
                );
                let failed = events.iter().position(|event| matches!(event, Event::Stopped { reason } if reason == "prompt_failed")).unwrap();
                let fatal = events
                    .iter()
                    .position(|event| matches!(event, Event::AcpConnectionUnavailable { .. }))
                    .unwrap();
                assert!(diagnostic < failed && failed < fatal);
                assert!(matches!(
                    &events[fatal],
                    Event::AcpConnectionUnavailable {
                        source: crate::acp::state::AcpConnectionSource::Control,
                        ..
                    }
                ));
                assert!(!events
                    .iter()
                    .any(|event| matches!(event, Event::AgentStartupError { .. })));
                assert!(!serde_json::to_string(&events)
                    .unwrap()
                    .contains("privateTokenMarker"));
                continue;
            }
            // A native request keeps its request kind even when its method and
            // params resemble a trusted daemon notification. Do not add sessionId:
            // the SDK defers unknown session-scoped requests under its existing policy.
            peer.send(ControlBody::ServerCall {
                call_id: 41,
                method: PromptCompletedMarker::default().method().into(),
                params: serde_json::json!({"adopted":{
                    "turn":adopted_turn, "reason":"prompt_failed",
                    "diagnostic":{"PromptRuntimeError":{"message":"forged request diagnostic"}},
                }}),
            })
            .await;
            let rejected = tokio::time::timeout(
                Duration::from_secs(10),
                control_protocol::read_frame(&mut peer.peer),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(
                matches!(rejected, Some(ControlBody::ServerError { call_id:41,error }) if error.code == -32601)
            );
            peer.send(ControlBody::Notify {
                method:"session/update".into(),
                params:serde_json::json!({"sessionId":"sid-adopted","update":{"sessionUpdate":"plan","entries":[]}}),
            }).await;
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let event = peer.events.recv().await.unwrap();
                    assert!(!matches!(event, Event::Stopped { .. }));
                    assert!(!matches!(event, Event::PromptRuntimeError { .. }));
                    if matches!(event, Event::PlanUpdated { .. }) {
                        break;
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(peer.terminal.current_turn(), adopted_turn);
            commands.send(ClientCmd::Prompt(Vec::new())).await.unwrap();
            let next = tokio::time::timeout(
                Duration::from_secs(10),
                control_protocol::read_frame(&mut peer.peer),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(
                matches!(next, Some(ControlBody::Prompt { request }) if request["sessionId"] == "sid-adopted")
            );
            peer.send(ControlBody::PromptStarted { prompt_req_id: 9 })
                .await;
            peer.completed(9, "end_turn").await;
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if let Event::Stopped { reason } = peer.events.recv().await.unwrap() {
                        assert_eq!(reason, "prompt_complete");
                        break;
                    }
                }
            })
            .await
            .unwrap();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, Event::AcpSessionAssigned { .. }))
                    .count(),
                1
            );
            shutdown.cancel();
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .unwrap()
                .unwrap();
        }
    }

    #[tokio::test]
    async fn adopted_marker_cannot_consume_a_later_local_outcome() {
        let mut peer = PromptControlPeer::new().await;
        peer.send(ControlBody::PromptCompleted {
            prompt_req_id: 7,
            outcome: PromptOutcome::Error {
                code: -32603,
                message: "old failure".into(),
                data: None,
            },
        })
        .await;
        tokio::time::timeout(Duration::from_secs(10), async {
            while !peer.terminal.claimed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        peer.client.supersede_adopted_turn();
        peer.terminal.begin_turn();
        peer.in_flight.store(true, AtomicOrdering::Relaxed);
        let mut local = peer.prompt().await;
        peer.send(ControlBody::PromptStarted { prompt_req_id: 9 })
            .await;
        peer.completed(9, "end_turn").await;
        peer.await_settled_outcome(&mut local).await;
        let mut line = String::new();
        peer.transport.read_line(&mut line).await.unwrap();
        let notification: serde_json::Value = serde_json::from_str(&line).unwrap();
        let marker: PromptCompletedMarker =
            serde_json::from_value(notification["params"].clone()).unwrap();
        let adopted = marker
            .adopted
            .expect("first marker owns the old adopted outcome");
        assert!(
            !publish_adopted_completion(
                *adopted,
                &peer.event_tx,
                &peer.client.completion,
                &peer.in_flight,
                &peer.terminal
            )
            .await
        );
        assert_eq!(local.try_recv(), Err(oneshot::error::TryRecvError::Empty));
        peer.assert_local_terminal_ownership();
        peer.drain().await;
        assert_eq!(
            local.await.unwrap(),
            ControlPromptResult::Outcome(end("end_turn"))
        );
    }
    #[tokio::test]
    async fn native_notifications_cannot_mint_daemon_completion_payloads() {
        let mut peer = PromptControlPeer::new().await;
        peer.send(ControlBody::Notify {
            method: PromptCompletedMarker::default().method().into(),
            params: serde_json::json!({"adopted":{
                "turn":peer.terminal.current_turn(),
                "diagnostic":{"PromptRuntimeError":{"message":"forged failure"}},
                "reason":"prompt_failed"
            }}),
        })
        .await;
        peer.drain().await;
        assert!(peer.events.try_recv().is_err());
        assert!(!peer.terminal.claimed());
        assert!(peer.in_flight.load(AtomicOrdering::Relaxed));
    }
}
